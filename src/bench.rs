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
        session: Arc<SmbSession>,
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
}

/// Plays until the picture from the latest jump is on screen (None after
/// 60 s); also when a preview (its keyframe) showed before that.
fn wait_shown(playback: &mut Playback, since: Instant) -> (Option<f64>, Option<f64>) {
    let clock = Instant::now();
    let mut preview = None;
    let ms = || since.elapsed().as_secs_f64() * 1e3;
    while since.elapsed() < Duration::from_secs(60) {
        let changed = playback.advance(clock.elapsed().as_nanos() as i64);
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
        playback.advance(clock.elapsed().as_nanos() as i64 + 1_000_000_000);
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
            session,
            share,
            path,
            connect_ms,
        } => {
            timing.note(format!("connect {connect_ms:.0} (before)"));
            let name = path.last().cloned().unwrap_or_default();
            let key = format!("bench/{share}/{}", path.join("/"));
            let opened = library::open_video(&session, &share, &path, key, options.hw, &mut timing)
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
    if first_frame_ms.is_some() && options.resume.is_none() {
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
    };
    crate::xr::app::drop_in_background(playback);
    Ok(report)
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
