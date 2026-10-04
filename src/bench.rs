//! `just-video bench-seek`: how long a video takes to open and to jump around
//! in, through the same open path and decode thread as the headset player,
//! without OpenXR. Runs on the headset over SMB or on a PC with a local file
//! (optionally behind a simulated network link).

use crate::library::{self, OpenTiming};
use crate::media::Media;
use crate::readahead::{BlockSource, BoxFuture, ReadAhead, ReadAheadReader};
use crate::smb::SmbSession;
use crate::xr::player::{Playback, SeekReport};
use serde::Serialize;
use std::{
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A network link shared by all reads: one at a time at `bytes_per_second`,
/// each answered a round trip after it was sent. Reads queue behind each other
/// as they do on a real connection, so stale read-ahead delays new reads.
pub struct Link {
    bytes_per_second: f64,
    rtt: Duration,
    busy_until: Mutex<Instant>,
}

impl Link {
    pub fn new(megabits_per_second: f64, rtt_ms: f64) -> Arc<Self> {
        Arc::new(Self {
            bytes_per_second: megabits_per_second * 1e6 / 8.0,
            rtt: Duration::from_secs_f64(rtt_ms / 1e3),
            busy_until: Mutex::new(Instant::now()),
        })
    }

    /// When a read of `len` bytes sent now has fully arrived.
    fn arrival(&self, len: usize) -> Instant {
        let mut busy = self.busy_until.lock().expect("link");
        // The request reaches the server half a round trip later; its data
        // then waits for everything already queued on the link.
        let start = (*busy).max(Instant::now() + self.rtt / 2);
        *busy = start + Duration::from_secs_f64(len as f64 / self.bytes_per_second);
        *busy + self.rtt / 2
    }
}

/// A local file as a read-ahead source, optionally behind a [`Link`].
pub struct LocalFile {
    file: std::fs::File,
    link: Option<Arc<Link>>,
}

impl BlockSource for LocalFile {
    fn fetch(self: Arc<Self>, offset: u64, len: usize) -> BoxFuture<'static, io::Result<Vec<u8>>> {
        Box::pin(async move {
            if let Some(link) = &self.link {
                tokio::time::sleep_until(link.arrival(len).into()).await;
            }
            let mut buffer = vec![0u8; len];
            std::os::unix::fs::FileExt::read_exact_at(&self.file, &mut buffer, offset)?;
            Ok(buffer)
        })
    }
}

pub enum Input {
    /// A file on an SMB session that was just connected (in `connect_ms`).
    Smb {
        /// The video is read over all of them.
        sessions: Vec<Arc<SmbSession>>,
        share: String,
        path: Vec<String>,
        connect_ms: f64,
    },
    Local {
        path: std::path::PathBuf,
        link: Option<Arc<Link>>,
    },
}

pub struct Options {
    pub hw: Option<&'static str>,
    pub read_ahead: ReadAhead,
    /// Open as if resuming here (seconds).
    pub resume: Option<f64>,
    /// Where the jumps start from (seconds).
    pub from: f64,
    /// Random jumps after the fixed ones.
    pub random: usize,
    /// Instead of jumping: play this many seconds from `from`, ticking like
    /// the headset's frame loop at `hz`, and copy each new picture as the
    /// renderer's upload does.
    pub play: Option<f64>,
    pub hz: f64,
    /// While playing: jump `jump_by` seconds this often (seconds), like D-pad presses.
    pub jump_every: Option<f64>,
    pub jump_by: f64,
}

/// Steady playback, as the headset's frame loop sees it.
#[derive(Serialize, Default)]
pub struct PlayReport {
    pub seconds: f64,
    pub hz: f64,
    pub fps: f64,
    /// Video frames due in that time.
    pub due: u64,
    /// New pictures shown (uploaded).
    pub shown: u64,
    /// Decoded frames passed over because a later one was already due.
    pub skipped: u64,
    /// Ticks showing a picture more than 1.5 frames older than the clock.
    pub behind_ticks: u64,
    pub ticks: u64,
    /// Ticks that took longer than a refresh period (loop work only).
    pub over_budget_ticks: u64,
    /// Copying a picture out of the decoder's buffer, as the upload does.
    pub copy_ms_mean: f64,
    pub copy_ms_max: f64,
    pub copy_mb_per_s: f64,
    /// `advance` (frame selection), excluding the copy.
    pub advance_ms_mean: f64,
    pub advance_ms_max: f64,
    /// Jumps made while playing (`jump_every`).
    pub jumps: u64,
    /// Why playback stopped early.
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct Jump {
    pub label: String,
    pub from: f64,
    pub target: f64,
    /// Request to the new picture on screen.
    pub shown_ms: Option<f64>,
    /// Request to its keyframe on screen, when shown first as a preview.
    pub preview_ms: Option<f64>,
    /// What the decode thread did for it.
    pub decoder: Option<SeekReport>,
}

#[derive(Serialize)]
pub struct Report {
    pub name: String,
    pub decoder: String,
    pub open_ms: f64,
    pub open_phases: Vec<(String, f64)>,
    /// Open request to the first picture on screen.
    pub first_frame_ms: Option<f64>,
    pub start: Option<SeekReport>,
    pub jumps: Vec<Jump>,
    pub play: Option<PlayReport>,
    /// The hardware decoder failed at a jump and the CPU took over.
    pub moved_to_cpu: bool,
}

/// Copies every row of `frame` into `staging`, like `Renderer::record_upload`.
fn copy_frame(frame: &crate::media::Frame, staging: &mut Vec<u8>) -> usize {
    let mut offset = 0;
    for plane in 0..frame.plane_count() {
        for row in frame.rows(plane) {
            if staging.len() < offset + row.len() {
                staging.resize(offset + row.len(), 0);
            }
            staging[offset..offset + row.len()].copy_from_slice(row);
            offset += row.len();
        }
    }
    offset
}

/// Plays for `seconds` in real time, ticking at `hz` like the frame loop.
fn play_steady(
    playback: &mut Playback,
    seconds: f64,
    hz: f64,
    jump_every: Option<f64>,
    jump_by: f64,
) -> PlayReport {
    let period = Duration::from_secs_f64(1.0 / hz);
    let clock = Instant::now();
    let mut staging = Vec::new();
    let mut r = PlayReport {
        hz,
        fps: playback.fps(),
        ..Default::default()
    };
    let (mut copy_total, mut advance_total, mut bytes) = (0.0, 0.0, 0usize);
    let skipped_before = playback.stats.skipped_frames;
    let first_time = playback.media_time(now_ns());
    let mut next_tick = Instant::now();
    let mut next_jump = jump_every.map(|s| clock + Duration::from_secs_f64(s));
    while clock.elapsed().as_secs_f64() < seconds {
        if let (Some(at), Some(every)) = (next_jump, jump_every)
            && Instant::now() >= at
        {
            playback.seek((playback.position() + jump_by).max(0.0));
            r.jumps += 1;
            next_jump = Some(at + Duration::from_secs_f64(every));
        }
        let now = now_ns();
        let tick = Instant::now();
        let changed = playback.advance(now);
        let advanced = tick.elapsed().as_secs_f64() * 1e3;
        advance_total += advanced;
        r.advance_ms_max = r.advance_ms_max.max(advanced);
        if changed && let Some(frame) = playback.current() {
            let copy = Instant::now();
            bytes += copy_frame(frame, &mut staging);
            let ms = copy.elapsed().as_secs_f64() * 1e3;
            copy_total += ms;
            r.copy_ms_max = r.copy_ms_max.max(ms);
            r.shown += 1;
        }
        if let (Some(t), Some(pts)) = (
            playback.media_time(now),
            playback.current().and_then(|f| f.pts()),
        ) && t - pts > 1.5 / r.fps
        {
            r.behind_ticks += 1;
        }
        r.ticks += 1;
        if tick.elapsed() > period {
            r.over_budget_ticks += 1;
        }
        if let Some(e) = &playback.error {
            r.error = Some(e.to_string());
            break;
        }
        next_tick += period;
        match next_tick.checked_duration_since(Instant::now()) {
            Some(wait) => std::thread::sleep(wait),
            None => next_tick = Instant::now(),
        }
    }
    let end = playback.media_time(now_ns());
    r.seconds = match (first_time, end) {
        (Some(a), Some(b)) => b - a,
        _ => clock.elapsed().as_secs_f64(),
    };
    r.due = (r.seconds * r.fps).round() as u64;
    r.skipped = playback.stats.skipped_frames - skipped_before;
    if r.shown > 0 {
        r.copy_ms_mean = copy_total / r.shown as f64;
        r.copy_mb_per_s = bytes as f64 / 1e6 / (copy_total / 1e3);
    }
    if r.ticks > 0 {
        r.advance_ms_mean = advance_total / r.ticks as f64;
    }
    r
}

/// One clock for every `advance`, as the headset's predicted display times are.
fn now_ns() -> i64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as i64 + 1_000_000_000
}

/// Plays until the picture from the latest jump is on screen (None after
/// 60 s); also when a preview (its keyframe) showed before that.
fn wait_shown(playback: &mut Playback, since: Instant) -> (Option<f64>, Option<f64>) {
    let mut preview = None;
    let ms = || since.elapsed().as_secs_f64() * 1e3;
    while since.elapsed() < Duration::from_secs(60) {
        let changed = playback.advance(now_ns());
        if playback.settled() {
            return (Some(ms()), preview);
        }
        if changed && preview.is_none() {
            preview = Some(ms());
        }
        if playback.error.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    (None, preview)
}

/// The decode thread's report on the latest jump (it may trail the picture).
fn report_of(playback: &Playback) -> Option<SeekReport> {
    let asked = Instant::now();
    while asked.elapsed() < Duration::from_millis(500) {
        if let Some(report) = playback.seek_report() {
            return Some(report);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

/// Plays on for `time` at normal speed.
fn play_for(playback: &mut Playback, time: Duration) {
    let clock = Instant::now();
    while clock.elapsed() < time {
        playback.advance(now_ns());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Small deterministic generator, so runs jump to the same places.
fn random_fractions(count: usize) -> Vec<f64> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        })
        .collect()
}

pub fn run(input: Input, options: &Options) -> anyhow::Result<Report> {
    let requested = Instant::now();
    let mut timing = OpenTiming::new(requested);
    let (name, mut decoder) = match input {
        Input::Smb {
            sessions,
            share,
            path,
            connect_ms,
        } => {
            timing.note(format!("connect {connect_ms:.0} (before)"));
            let name = path.last().cloned().unwrap_or_default();
            let key = format!("bench/{share}/{}", path.join("/"));
            let opened =
                library::open_video(&sessions, &share, &path, key, options.hw, &mut timing)
                    .map_err(anyhow::Error::msg)?;
            (name, opened.decoder)
        }
        Input::Local { path, link } => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file = std::fs::File::open(&path)?;
            let len = file.metadata()?.len();
            let runtime = Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_time()
                    .build()?,
            );
            let reader =
                ReadAheadReader::new(runtime, LocalFile { file, link }, len, options.read_ahead);
            timing.lap("open file");
            let media = Media::open(&name, reader)?;
            timing.lap("probe");
            timing.note(format!("probe read {}", media.io()));
            let mut decoder = media.into_decoder(options.hw, true, "")?;
            timing.lap("open decoder");
            decoder.requested_at = Some(requested);
            timing.log(&name);
            (name, decoder)
        }
    };
    decoder.requested_at = Some(requested);
    let decoder_name = decoder.stats().decoder.clone();
    let layout = crate::vr::detect(&name, decoder.info().video.as_ref());
    let open_ms = timing.total_ms();
    let mut playback = Playback::start(decoder, layout, options.resume.unwrap_or(0.0), 0.0);
    let first_frame_ms = wait_shown(&mut playback, requested).0;
    let start = report_of(&playback);
    let duration = playback.duration;
    let mut jumps = Vec::new();
    let mut play = None;
    if let Some(seconds) = options.play.filter(|_| first_frame_ms.is_some()) {
        if options.resume.is_none() && options.from > 0.0 {
            playback.seek(options.from.min(duration * 0.5));
            wait_shown(&mut playback, Instant::now());
        }
        // Settle in before measuring.
        play_for(&mut playback, Duration::from_secs(2));
        play = Some(play_steady(
            &mut playback,
            seconds,
            options.hz,
            options.jump_every,
            options.jump_by,
        ));
    } else if first_frame_ms.is_some() && options.resume.is_none() {
        let from = options.from.min(duration * 0.5);
        let mut jump = |playback: &mut Playback, label: &str, target: f64| {
            let from = playback.position();
            let since = Instant::now();
            playback.seek(target);
            let (shown_ms, preview_ms) = wait_shown(playback, since);
            eprintln!(
                "bench-seek: {label}: {from:.1}s -> {target:.1}s shown in {}",
                shown_ms.map_or("(timed out)".into(), |ms| format!("{ms:.0} ms"))
            );
            jumps.push(Jump {
                label: label.to_string(),
                from,
                target,
                shown_ms,
                preview_ms,
                decoder: report_of(playback),
            });
            play_for(playback, Duration::from_millis(1500));
        };
        jump(&mut playback, "to start point", from);
        for (label, delta) in [
            ("+10 s", 10.0),
            ("+10 s again", 10.0),
            ("-10 s", -10.0),
            ("+10 min", 600.0),
            ("-10 min", -600.0),
        ] {
            let target = playback.position() + delta;
            if (0.0..duration - 1.0).contains(&target) {
                jump(&mut playback, label, target);
            }
        }
        for (i, f) in random_fractions(options.random).into_iter().enumerate() {
            jump(
                &mut playback,
                &format!("random {}", i + 1),
                f * (duration - 2.0),
            );
        }
        // D-pad mashed: five +5 s presses 60 ms apart; timed from the last.
        let base = playback.position();
        for i in 1..5 {
            playback.seek(base + 5.0 * i as f64);
            std::thread::sleep(Duration::from_millis(60));
        }
        jump(&mut playback, "5 x +5 s (from last press)", base + 25.0);
    }
    let report = Report {
        name,
        decoder: decoder_name,
        open_ms,
        open_phases: timing.phases(),
        first_frame_ms,
        start,
        jumps,
        play,
        moved_to_cpu: playback.moved_to_cpu,
    };
    crate::xr::app::drop_in_background(playback);
    Ok(report)
}

impl Report {
    /// What went wrong for a viewer: jumps that never showed their picture,
    /// and playback that stopped.
    pub fn failures(&self) -> Vec<String> {
        let mut failures: Vec<String> = self
            .jumps
            .iter()
            .filter(|j| j.shown_ms.is_none())
            .map(|j| format!("{}: never shown", j.label))
            .collect();
        if self.first_frame_ms.is_none() {
            failures.push("no first frame".into());
        }
        if self.moved_to_cpu {
            failures.push("hardware decoder failed at a jump; moved to the CPU".into());
        }
        if let Some(e) = self.play.as_ref().and_then(|p| p.error.as_ref()) {
            failures.push(format!("playback stopped: {e}"));
        }
        failures
    }
}

/// The report as an aligned table.
pub fn summary(r: &Report) -> String {
    let mut out = format!(
        "{} ({})\nopen {:.0} ms, first frame on screen {} after the request\n",
        r.name,
        r.decoder,
        r.open_ms,
        r.first_frame_ms
            .map_or("never".into(), |ms| format!("{ms:.0} ms"))
    );
    if let Some(p) = &r.play {
        out += &format!(
            "played {:.1} s at {:.0} Hz: {} of {} frames shown, {} skipped, {} of {} ticks behind, {} over budget\n\
             copy {:.1} ms mean / {:.1} max ({:.0} MB/s), advance {:.2} ms mean / {:.1} max\n",
            p.seconds,
            p.hz,
            p.shown,
            p.due,
            p.skipped,
            p.behind_ticks,
            p.ticks,
            p.over_budget_ticks,
            p.copy_ms_mean,
            p.copy_ms_max,
            p.copy_mb_per_s,
            p.advance_ms_mean,
            p.advance_ms_max,
        );
        if p.jumps > 0 {
            out += &format!("{} jumps of +5 s while playing\n", p.jumps);
        }
        if let Some(e) = &p.error {
            out += &format!("stopped: {e}\n");
        }
        return out;
    }
    out += "jump                         target   shown preview  how       keyframe gap  discarded  read\n";
    for j in &r.jumps {
        let d = j.decoder.as_ref();
        out += &format!(
            "{:<26} {:>8.1}s {:>7} {:>7}  {:<9} {:>12} {:>10}  {}\n",
            j.label,
            j.target,
            j.shown_ms.map_or("-".into(), |ms| format!("{ms:.0}ms")),
            j.preview_ms.map_or("-".into(), |ms| format!("{ms:.0}ms")),
            d.map_or("-", |d| d.how),
            d.and_then(|d| d.first_decoded.map(|k| format!("{:.2}s", d.target - k)))
                .unwrap_or("-".into()),
            d.map_or(0, |d| d.discarded),
            d.map_or(String::new(), |d| d.read.to_string()),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jump(label: &str, shown_ms: Option<f64>) -> Jump {
        Jump {
            label: label.into(),
            from: 0.0,
            target: 10.0,
            shown_ms,
            preview_ms: None,
            decoder: None,
        }
    }

    #[test]
    fn jumps_without_a_picture_fail_the_run() {
        let mut r = Report {
            name: "v.mp4".into(),
            decoder: "h264_v4l2m2m".into(),
            open_ms: 100.0,
            open_phases: Vec::new(),
            first_frame_ms: Some(150.0),
            start: None,
            jumps: vec![jump("+10 s", Some(120.0)), jump("-10 s", Some(80.0))],
            play: None,
            moved_to_cpu: false,
        };
        assert!(r.failures().is_empty());
        r.moved_to_cpu = true;
        assert_eq!(
            r.failures(),
            ["hardware decoder failed at a jump; moved to the CPU"]
        );
        r.moved_to_cpu = false;
        r.jumps.push(jump("+10 min", None));
        r.play = Some(PlayReport {
            error: Some("Decoding failed".into()),
            ..Default::default()
        });
        assert_eq!(
            r.failures(),
            ["+10 min: never shown", "playback stopped: Decoding failed"]
        );
    }
}
