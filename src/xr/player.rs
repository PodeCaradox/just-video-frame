//! Video playback state: a decode thread feeding a small queue, and frame
//! selection by timestamp against the runtime's predicted display time.

use super::renderer::EyeParams;
use crate::media::{Frame, VideoDecoder};
use crate::subtitles::Cues;
use crate::vr::{Layout, Projection, Stereo};
use openxr as xr;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Presentation settings shared by all videos.
#[derive(Clone)]
pub struct ViewOptions {
    /// Fisheye lens field of view in degrees.
    pub fisheye_fov: f32,
    /// Flat screen width and distance in metres.
    pub screen_width: f32,
    pub screen_distance: f32,
    /// Shader debug view: 0 off, 1 projection UV, 2 raw YUV samples.
    pub debug_view: u32,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            fisheye_fov: 180.0,
            screen_width: 3.2,
            screen_distance: 3.0,
            debug_view: 0,
        }
    }
}

/// Where the video is placed around the viewer (changed by dragging).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Rotation around the viewer: yaw (left/right) and pitch (up/down), radians.
    pub yaw: f32,
    pub pitch: f32,
    /// Flat/curved screen distance in metres.
    pub distance: f32,
    pub curved: bool,
    /// Size multiplier: screen width for flat/curved, magnification for VR180/360.
    pub zoom: f32,
}

impl Placement {
    pub fn new(options: &ViewOptions) -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            distance: options.screen_distance,
            curved: false,
            zoom: 1.0,
        }
    }

    /// Rotation matrix (columns) of the placement: yaw about +Y, then pitch about +X.
    pub fn rotation(&self) -> [[f32; 3]; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        // R = Ry(yaw) * Rx(pitch)
        [
            [cy, 0.0, -sy],
            [sy * sp, cp, cy * sp],
            [sy * cp, -sp, cy * cp],
        ]
    }
}

/// Command-line playback extras (testing aids).
#[derive(Clone, Default)]
pub struct PlayOptions {
    /// Write the left eye as PNG once playback reaches this media time.
    pub screenshot: Option<(PathBuf, f64)>,
    /// Stop at this media time (seconds).
    pub duration: Option<f64>,
    /// Frames before this media time (after a seek to the previous keyframe) are skipped.
    pub start: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
        [0, 1, 2].map(|i| (0..3).map(|k| m[k][i] * v[k]).sum())
    }

    fn situation(target: f64, from: f64) -> SeekSituation {
        SeekSituation {
            target,
            from,
            ..Default::default()
        }
    }

    #[test]
    fn short_jumps_are_exact() {
        let mut s = situation(110.0, 100.0);
        s.key_before = Some(108.0);
        s.key_after = Some(112.0);
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        s.target = 90.0;
        s.key_before = Some(89.0);
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        // Nothing known about keyframes.
        assert_eq!(plan_seek(&situation(110.0, 100.0)), SeekPlan::Exact);
    }

    #[test]
    fn short_jumps_far_from_a_keyframe_snap_but_keep_their_direction() {
        // +10 s with the keyframe 6 s before the target: the one after is nearer.
        let mut s = situation(110.0, 100.0);
        s.key_before = Some(104.0);
        s.key_after = Some(112.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(112.0));
        // Nearest would be behind where we are: take the other one.
        s.key_before = Some(99.0);
        s.key_after = Some(130.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(130.0));
        // -10 s: the nearest keyframe is past where we are, so the one before.
        let mut s = situation(90.0, 100.0);
        s.key_before = Some(80.0);
        s.key_after = Some(101.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(80.0));
        // No keyframe after the target known: exact.
        let mut s = situation(110.0, 100.0);
        s.key_before = Some(95.0);
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
    }

    #[test]
    fn no_keyframe_in_between_keeps_decoding() {
        // Decoded up to 101 s, next keyframe after 110 s: just go on.
        let mut s = situation(103.0, 100.5);
        s.decoded = Some(101.0);
        s.key_before = Some(98.0);
        s.key_after = Some(110.0);
        assert_eq!(plan_seek(&s), SeekPlan::Continue);
        // Too far to decode on quickly (and to the keyframe before): snap.
        s.target = 105.0;
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(110.0));
        s.target = 103.0;
        // An index that knows nothing past the target may be incomplete.
        s.key_after = None;
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        s.key_after = Some(110.0);
        // A keyframe past the decoder: jumping there is cheaper.
        s.key_before = Some(103.0);
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        // Backwards always jumps.
        s.target = 99.0;
        s.key_before = Some(98.0);
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        // Without an index, nothing is known: jump.
        s.target = 105.0;
        s.key_before = None;
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        // Very long GOPs: jump rather than decode on for a long time.
        let mut s = situation(130.0, 100.0);
        s.decoded = Some(101.0);
        s.key_before = Some(98.0);
        s.key_after = Some(140.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(140.0));
    }

    #[test]
    fn long_jumps_land_on_the_nearest_keyframe() {
        let mut s = situation(700.0, 100.0);
        s.key_before = Some(691.0);
        s.key_after = Some(702.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(702.0));
        s.key_after = Some(712.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(691.0));
        // Backwards too.
        let mut s = situation(100.0, 700.0);
        s.key_before = Some(90.0);
        s.key_after = Some(101.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(101.0));
        // An index that only knows what was read so far (Matroska before its
        // first seek) has nothing after the target: exact, not back to 0.
        let mut s = situation(700.0, 100.0);
        s.key_before = Some(2.0);
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
        assert_eq!(plan_seek(&situation(700.0, 100.0)), SeekPlan::Exact);
    }

    #[test]
    fn resume_lands_on_the_keyframe_before() {
        let mut s = situation(1200.0, 0.0);
        s.resume = true;
        s.key_before = Some(1195.0);
        s.key_after = Some(1201.0);
        assert_eq!(plan_seek(&s), SeekPlan::Keyframe(1195.0));
        // Only the start indexed so far (Matroska): exact.
        s.key_before = Some(0.0);
        s.key_after = None;
        assert_eq!(plan_seek(&s), SeekPlan::Exact);
    }

    #[test]
    fn placement_turns_the_screen_direction() {
        let mut p = Placement {
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            distance: 3.0,
            curved: false,
            zoom: 1.0,
        };
        // Yaw 90° (left) moves the screen's forward (-Z) to -X.
        let f = apply(p.rotation(), [0.0, 0.0, -1.0]);
        assert!((f[0] + 1.0).abs() < 1e-5 && f[2].abs() < 1e-5, "{f:?}");
        p.yaw = 0.0;
        p.pitch = 0.3;
        let f = apply(p.rotation(), [0.0, 0.0, -1.0]);
        assert!(f[1] > 0.2, "pitch up raises the screen: {f:?}");
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct PlayStats {
    pub displayed_frames: u64,
    pub uploaded_frames: u64,
    pub skipped_frames: u64,
    pub xr_frames: u64,
    /// XR frames the runtime asked us to render (false while not visible).
    pub rendered_xr_frames: u64,
    pub media_seconds: f64,
    pub screenshot: Option<String>,
}

enum Decoded {
    Frame(u64, Frame),
    /// The keyframe a seek landed on, shown while decoding on to the target.
    Preview(u64, Frame),
    End(u64),
    Failed(u64, String),
}

struct AudioChunk {
    generation: u64,
    /// Time of the first sample, seconds from the start of the video.
    pts: f64,
    samples: Vec<f32>,
}

/// State shared with the audio thread.
struct AudioShared {
    generation: AtomicU64,
    paused: AtomicBool,
    /// Volume 0..=1 as f32 bits.
    volume: AtomicU32,
    /// Media time heard at an instant, for the current generation.
    clock: Mutex<Option<(u64, f64, Instant)>>,
}

impl AudioShared {
    fn heard_now(&self) -> Option<(u64, f64)> {
        let clock = *self.clock.lock().expect("audio clock");
        clock.map(|(generation, position, at)| (generation, position + at.elapsed().as_secs_f64()))
    }
}

fn spawn_audio(
    name: String,
    chunks: mpsc::Receiver<AudioChunk>,
    shared: Arc<AudioShared>,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut output = match crate::audio::Output::open(&name) {
                Ok(output) => Some(output),
                Err(e) => {
                    eprintln!("Audio disabled: {e:#}");
                    None
                }
            };
            let channels = crate::audio::CHANNELS as usize;
            let rate = crate::audio::RATE as f64;
            let mut played_generation = 0;
            let gain = crate::audio::Gain;
            while !stop.load(Ordering::Relaxed) {
                let chunk = match chunks.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => chunk,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
                let Some(out) = output.as_mut() else { continue };
                if chunk.generation != shared.generation.load(Ordering::Relaxed) {
                    continue; // from before a seek
                }
                if chunk.generation != played_generation {
                    out.flush();
                    played_generation = chunk.generation;
                }
                // ~10 ms slices keep pause and seek responsive.
                let slice = (rate as usize / 100) * channels;
                let mut offset = 0;
                while offset < chunk.samples.len() {
                    if stop.load(Ordering::Relaxed)
                        || chunk.generation != shared.generation.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    if shared.paused.load(Ordering::Relaxed) {
                        *shared.clock.lock().expect("audio clock") = None;
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    let end = (offset + slice).min(chunk.samples.len());
                    let level = f32::from_bits(shared.volume.load(Ordering::Relaxed));
                    let scaled = gain.apply(&chunk.samples[offset..end], level);
                    if let Err(e) = out.write(&scaled) {
                        eprintln!("{e:#}");
                        return;
                    }
                    offset = end;
                    let written_until = chunk.pts + (offset / channels) as f64 / rate;
                    let heard = written_until - out.latency();
                    *shared.clock.lock().expect("audio clock") =
                        Some((chunk.generation, heard, Instant::now()));
                }
            }
        })
        .expect("spawn audio thread");
}

/// A jump requested by the viewer.
struct SeekRequest {
    generation: u64,
    target: f64,
    /// The position shown when it was asked for.
    from: f64,
    /// Continuing where the viewer left the video last time.
    resume: bool,
    requested: Instant,
}

/// How the decoder reaches a seek target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeekPlan {
    /// No keyframe between the decoder and the target: decode on to it.
    /// Cheaper than jumping back to the same keyframe (or an earlier one).
    Continue,
    /// Jump to the keyframe before the target and decode up to it.
    Exact,
    /// Jump to this keyframe and play from there: after a long jump nobody
    /// misses a few seconds, but decoding them can take seconds with long
    /// GOPs (10 s in some films) on the headset's CPU.
    Keyframe(f64),
}

/// What [`plan_seek`] decides from.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeekSituation {
    pub target: f64,
    /// The position shown when the jump was asked for.
    pub from: f64,
    /// Time of the last frame decoded since the last jump, if any.
    pub decoded: Option<f64>,
    /// Keyframes around the target from the file's index, if it has one.
    pub key_before: Option<f64>,
    pub key_after: Option<f64>,
    pub resume: bool,
}

/// Jumps at least this long land on the nearest keyframe (see `SeekPlan::Keyframe`).
pub const KEYFRAME_SEEK_FROM: f64 = 60.0;
/// Shorter jumps are exact unless their keyframe is further than this before
/// the target: then decoding up to it would take too long on the headset's
/// CPU (a 10 s GOP of 4K HEVC is 240 frames).
const EXACT_GAP: f64 = 3.0;

pub fn plan_seek(s: &SeekSituation) -> SeekPlan {
    // An index still being built while reading (Matroska before its first
    // seek) knows no keyframe after the target, and may miss some before it.
    // Decoding on is as slow as an exact jump beyond `EXACT_GAP`.
    if let (Some(decoded), Some(key), Some(_)) = (s.decoded, s.key_before, s.key_after)
        && decoded < s.target
        && s.target - decoded <= EXACT_GAP
        && key <= decoded
    {
        return SeekPlan::Continue;
    }
    let jump = s.target - s.from;
    let exact_is_quick = s.key_before.is_none_or(|key| s.target - key <= EXACT_GAP);
    if !s.resume && jump.abs() < KEYFRAME_SEEK_FROM && exact_is_quick {
        return SeekPlan::Exact;
    }
    // Only an index with keyframes on both sides is trusted to be complete here.
    let (Some(before), Some(after)) = (s.key_before, s.key_after) else {
        return SeekPlan::Exact;
    };
    if s.resume {
        // Never past the point left: it is already rewound a little.
        return SeekPlan::Keyframe(before);
    }
    // The nearest keyframe that still moves the way the viewer asked.
    let nearest = if after - s.target < s.target - before {
        [after, before]
    } else {
        [before, after]
    };
    nearest
        .into_iter()
        .find(|&key| {
            if jump >= 0.0 {
                key > s.from
            } else {
                key < s.from
            }
        })
        .map_or(SeekPlan::Exact, SeekPlan::Keyframe)
}

/// A seek whose keyframe is at least this far before the target shows the
/// keyframe while decoding on.
const PREVIEW_GAP: f64 = 0.5;

/// How the last seek (or the start) went, for logs and `bench-seek`.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SeekReport {
    #[serde(skip)]
    pub generation: u64,
    pub target: f64,
    /// How the decoder got there: "start", "resume", or the `SeekPlan`.
    pub how: &'static str,
    /// Time of the first frame decoded after the jump (the keyframe).
    pub first_decoded: Option<f64>,
    /// Frames decoded before the target, thrown away.
    pub discarded: u32,
    /// Requests merged into this one while it waited.
    pub coalesced: u32,
    /// The demuxer's seek and decoder flush.
    pub seek_ms: f64,
    /// From the request to the target frame leaving the decoder.
    pub ready_ms: f64,
    pub read: crate::media::IoCount,
}

/// Per-jump bookkeeping in the decode thread.
struct Catchup {
    report: SeekReport,
    requested: Instant,
    io_before: crate::media::IoCount,
}

impl Catchup {
    fn new(
        generation: u64,
        target: f64,
        how: &'static str,
        requested: Instant,
        io: crate::media::IoCount,
    ) -> Self {
        Self {
            report: SeekReport {
                generation,
                target,
                how,
                ..Default::default()
            },
            requested,
            io_before: io,
        }
    }

    /// The first frame at the target is on its way: log how it went.
    fn finish(mut self, io: crate::media::IoCount, shared: &Mutex<Option<SeekReport>>) {
        let r = &mut self.report;
        r.ready_ms = self.requested.elapsed().as_secs_f64() * 1e3;
        r.read = io.since(self.io_before);
        eprintln!(
            "Timing: seek to {:.1}s ({}): ready in {:.0} ms (seek {:.0} ms); keyframe {}, \
             {} frames discarded, {} merged, read {}",
            r.target,
            r.how,
            r.ready_ms,
            r.seek_ms,
            r.first_decoded.map_or("?".into(), |k| format!(
                "{k:.2}s ({:.2}s before)",
                r.target - k
            )),
            r.discarded,
            r.coalesced,
            r.read,
        );
        *shared.lock().expect("seek report") = Some(self.report);
    }
}

struct DecodeThread {
    frames: mpsc::Receiver<Decoded>,
    control: mpsc::Sender<SeekRequest>,
    /// The last finished seek (or start).
    report: Arc<Mutex<Option<SeekReport>>>,
    requested: Arc<AtomicU64>,
    /// Which embedded subtitle track to decode (None: none).
    subtitle_track: mpsc::Sender<Option<usize>>,
    /// Which audio track to play (a seek follows).
    audio_track: mpsc::Sender<usize>,
}

fn spawn_decoder(
    mut decoder: VideoDecoder,
    start: f64,
    audio: Option<mpsc::Sender<AudioChunk>>,
    stop: Arc<AtomicBool>,
    embedded_cues: Arc<Mutex<Cues>>,
) -> DecodeThread {
    let (subtitle_track, subtitle_changes) = mpsc::channel::<Option<usize>>();
    let (audio_track, audio_changes) = mpsc::channel::<usize>();
    // A few frames of slack absorb decode jitter; more would only cost memory.
    let (tx, frames) = mpsc::sync_channel(4);
    let (control, commands) = mpsc::channel::<SeekRequest>();
    let requested = Arc::new(AtomicU64::new(0));
    let pending = requested.clone();
    let report = Arc::new(Mutex::new(None));
    let shared_report = report.clone();
    std::thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let (mut generation, mut start) = (0u64, start);
            let opened = decoder.requested_at;
            let mut catchup = Some(Catchup::new(
                0,
                start,
                "start",
                Instant::now(),
                decoder.io(),
            ));
            // Continuing a video: jump there here, off the frame loop.
            let mut resume = (start > 0.0).then(|| SeekRequest {
                generation: 0,
                target: start,
                from: 0.0,
                resume: true,
                requested: Instant::now(),
            });
            // Time of the last frame decoded since the last jump.
            let mut decoded: Option<f64> = None;
            let mut preview_pending = false;
            'decode: loop {
                // Before seeks: the seek that follows a switch restarts the new track.
                while let Ok(track) = audio_changes.try_recv() {
                    if !decoder.select_audio(track) {
                        eprintln!("Audio: can't decode track {track}");
                    }
                }
                // Only the latest of several queued jumps matters (D-pad held or mashed).
                let mut latest = resume.take();
                let mut merged = 0;
                while let Ok(request) = commands.try_recv() {
                    merged += latest.is_some() as u32;
                    latest = Some(request);
                }
                if let Some(request) = latest {
                    let plan = plan_seek(&SeekSituation {
                        target: request.target,
                        from: request.from,
                        decoded,
                        key_before: decoder.keyframe(request.target, false),
                        key_after: decoder.keyframe(request.target, true),
                        resume: request.resume,
                    });
                    let how = match plan {
                        _ if request.resume => "resume",
                        SeekPlan::Continue => "continue",
                        SeekPlan::Exact => "exact",
                        SeekPlan::Keyframe(_) => "keyframe",
                    };
                    let mut next = Catchup::new(
                        request.generation,
                        request.target,
                        how,
                        request.requested,
                        decoder.io(),
                    );
                    // A jump still decoding towards its target was merged too.
                    next.report.coalesced = merged
                        + catchup
                            .take()
                            .filter(|c| c.report.generation != request.generation)
                            .map_or(0, |c| c.report.coalesced + 1);
                    let to = match plan {
                        // Index times may be decode times (MP4), off from the
                        // presentation times a seek takes by a frame or two:
                        // aim between this keyframe and the next, and take
                        // whatever frame comes first.
                        SeekPlan::Keyframe(key) => decoder
                            .keyframe(key + 1e-3, true)
                            .map_or(key + 0.5, |next| (key + next) / 2.0),
                        _ => request.target,
                    };
                    if plan != SeekPlan::Continue {
                        let began = Instant::now();
                        if let Err(e) = decoder.seek(to) {
                            eprintln!("Seek to {to:.1}s failed: {e:#}");
                        }
                        next.report.seek_ms = began.elapsed().as_secs_f64() * 1e3;
                        decoded = None;
                    }
                    let keyframe = matches!(plan, SeekPlan::Keyframe(_));
                    // Frames before the target are thrown away; those no
                    // other frame needs are not even decoded.
                    decoder.skip_nonref_until(if keyframe { 0.0 } else { to });
                    preview_pending = plan == SeekPlan::Exact;
                    catchup = Some(next);
                    generation = request.generation;
                    // A keyframe jump starts at the first frame, whatever its time.
                    start = if keyframe { f64::NEG_INFINITY } else { to };
                }
                while let Ok(track) = subtitle_changes.try_recv() {
                    embedded_cues.lock().expect("cues").clear();
                    if !decoder.select_subtitle(track) {
                        eprintln!("Subtitles: can't decode track {track:?}");
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let mut message = match decoder.next_frame() {
                    Ok(Some(frame)) => Decoded::Frame(generation, frame),
                    Ok(None) => Decoded::End(generation),
                    Err(e) => Decoded::Failed(generation, format!("{e:#}")),
                };
                if let Decoded::Frame(_, frame) = &message {
                    decoded = frame.pts().or(decoded);
                    if let Some(c) = &mut catchup
                        && c.report.first_decoded.is_none()
                    {
                        c.report.first_decoded = frame.pts();
                    }
                    if start == f64::NEG_INFINITY {
                        start = frame.pts().map_or(0.0, |t| t - 1e-3);
                    }
                }
                let cues = decoder.take_subtitles();
                if !cues.is_empty() {
                    let mut shared = embedded_cues.lock().expect("cues");
                    for cue in cues {
                        shared.insert(cue);
                    }
                }
                if let Some(audio) = &audio {
                    while let Some((samples, pts)) = decoder.take_audio(crate::audio::CHANNELS) {
                        let pts = pts.unwrap_or(start);
                        // After a seek, skip sound from before the target.
                        let skip = (((start - pts) * crate::audio::RATE as f64).max(0.0) as usize
                            * crate::audio::CHANNELS as usize)
                            .min(samples.len());
                        if skip < samples.len() {
                            let pts = pts
                                + (skip / crate::audio::CHANNELS as usize) as f64
                                    / crate::audio::RATE as f64;
                            let _ = audio.send(AudioChunk {
                                generation,
                                pts,
                                samples: samples[skip..].to_vec(),
                            });
                        }
                    }
                }
                // Seeking lands on the keyframe before `start`; decode through to it.
                if let Decoded::Frame(_, frame) = &message
                    && let Some(pts) = frame.pts().filter(|&t| t < start)
                {
                    if let Some(c) = &mut catchup {
                        c.report.discarded += 1;
                    }
                    // The keyframe, if the target is far: something to look at.
                    if !std::mem::take(&mut preview_pending) || pts > start - PREVIEW_GAP {
                        continue;
                    }
                    let Decoded::Frame(g, frame) = message else {
                        unreachable!()
                    };
                    message = Decoded::Preview(g, frame);
                } else {
                    preview_pending = false;
                }
                let preview = matches!(message, Decoded::Preview(..));
                let finished = !matches!(message, Decoded::Frame(..) | Decoded::Preview(..));
                let mut message = Some(message);
                while let Some(m) = message.take() {
                    match tx.try_send(m) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(m)) => {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            if pending.load(Ordering::Relaxed) != generation {
                                continue 'decode; // a seek is waiting; drop this frame
                            }
                            message = Some(m);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                }
                if !preview && let Some(c) = catchup.take() {
                    if matches!(c.report.how, "start" | "resume")
                        && let Some(at) = opened
                    {
                        eprintln!(
                            "Timing: first frame ready {:.0} ms after the video was chosen",
                            at.elapsed().as_secs_f64() * 1e3
                        );
                    }
                    c.finish(decoder.io(), &shared_report);
                }
                if finished {
                    // Wait for a seek (e.g. back from the end) or shutdown.
                    while pending.load(Ordering::Relaxed) == generation {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        })
        .expect("spawn decode thread");
    DecodeThread {
        frames,
        control,
        report,
        requested,
        subtitle_track,
        audio_track,
    }
}

/// "Korean · Surround", "English 5.1"…
fn audio_label(index: usize, t: &crate::media::AudioTrackInfo) -> String {
    let mut label = t
        .language
        .as_deref()
        .map(language_name)
        .unwrap_or_else(|| format!("Track {}", index + 1));
    match t.title.as_deref() {
        Some(title) if !title.eq_ignore_ascii_case(&label) => label = format!("{label} · {title}"),
        _ => {
            let channels = match t.channels {
                1 => "mono".to_string(),
                2 => "stereo".to_string(),
                6 => "5.1".to_string(),
                8 => "7.1".to_string(),
                0 => String::new(),
                n => format!("{n} ch"),
            };
            if !channels.is_empty() {
                label = format!("{label} {channels}");
            }
        }
    }
    label
}

/// A subtitle track the viewer can pick.
enum SubtitleSource {
    /// Index into the file's subtitle streams; cues arrive while decoding.
    Embedded(usize),
    /// A .srt file next to the video.
    External(Cues),
}

struct SubtitleTrack {
    label: String,
    source: SubtitleSource,
}

/// "English", "Svenska"… for common ISO 639-2 codes; the code otherwise.
fn language_name(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "eng" | "en" => "English",
        "swe" | "sv" => "Swedish",
        "nor" | "nob" | "no" => "Norwegian",
        "dan" | "da" => "Danish",
        "fin" | "fi" => "Finnish",
        "ger" | "deu" | "de" => "German",
        "fre" | "fra" | "fr" => "French",
        "spa" | "es" => "Spanish",
        "ita" | "it" => "Italian",
        "por" | "pt" => "Portuguese",
        "dut" | "nld" | "nl" => "Dutch",
        "pol" | "pl" => "Polish",
        "rus" | "ru" => "Russian",
        "jpn" | "ja" => "Japanese",
        "kor" | "ko" => "Korean",
        "chi" | "zho" | "zh" => "Chinese",
        "ara" | "ar" => "Arabic",
        "est" | "et" => "Estonian",
        "lav" | "lv" => "Latvian",
        "lit" | "lt" => "Lithuanian",
        "ukr" | "uk" => "Ukrainian",
        "hin" | "hi" => "Hindi",
        "tam" | "ta" => "Tamil",
        "tel" | "te" => "Telugu",
        "cze" | "ces" | "cs" => "Czech",
        "slo" | "slk" | "sk" => "Slovak",
        "slv" | "sl" => "Slovenian",
        "hun" | "hu" => "Hungarian",
        "rum" | "ron" | "ro" => "Romanian",
        "bul" | "bg" => "Bulgarian",
        "hrv" | "hr" => "Croatian",
        "srp" | "sr" => "Serbian",
        "gre" | "ell" | "el" => "Greek",
        "tur" | "tr" => "Turkish",
        "heb" | "he" => "Hebrew",
        "tha" | "th" => "Thai",
        "vie" | "vi" => "Vietnamese",
        "ind" | "id" => "Indonesian",
        "may" | "msa" | "ms" => "Malay",
        "ice" | "isl" | "is" => "Icelandic",
        "cat" | "ca" => "Catalan",
        "per" | "fas" | "fa" => "Persian",
        _ => return code.to_string(),
    };
    name.to_string()
}

pub struct Playback {
    pub layout: Layout,
    /// Picture corrections and rotation.
    pub image: crate::config::ImageAdjust,
    /// The track CC turns back on.
    last_subtitle: usize,
    subtitle_tracks: Vec<SubtitleTrack>,
    subtitle: Option<usize>,
    embedded_cues: Arc<Mutex<Cues>>,
    /// Briefly shown instead of subtitles ("Subtitles: English"), until then.
    subtitle_notice: Option<(String, Instant)>,
    audio_labels: Vec<String>,
    audio_track: Option<usize>,
    decode: DecodeThread,
    audio: Option<Arc<AudioShared>>,
    stop: Arc<AtomicBool>,
    generation: u64,
    current: Option<Frame>,
    next: Option<Frame>,
    ended: bool,
    pub error: Option<String>,
    fps: f64,
    pub duration: f64,
    last_pts: f64,
    /// Display time (ns) corresponding to media time 0.
    clock_start: Option<i64>,
    paused_at: Option<i64>,
    pub stats: PlayStats,
}

impl Playback {
    /// Starts decoding (and sound, when the file has audio) at `start` seconds
    /// (the keyframe at or before it).
    pub fn start(mut decoder: VideoDecoder, layout: Layout, start: f64, volume: f32) -> Self {
        // Embedded text subtitles; the file's default track is on from the start.
        let embedded: Vec<(usize, &crate::media::SubtitleTrackInfo)> = decoder
            .info()
            .subtitles
            .iter()
            .enumerate()
            .filter(|(_, t)| t.supported)
            .collect();
        let subtitle_tracks: Vec<SubtitleTrack> = embedded
            .iter()
            .map(|(i, t)| {
                let mut label = t
                    .language
                    .as_deref()
                    .map(language_name)
                    .unwrap_or_else(|| format!("Track {}", i + 1));
                if let Some(title) = t
                    .title
                    .as_deref()
                    .filter(|title| !title.eq_ignore_ascii_case(&label))
                {
                    label = format!("{label} ({title})");
                }
                SubtitleTrack {
                    label,
                    source: SubtitleSource::Embedded(*i),
                }
            })
            .collect();
        let subtitle = embedded
            .iter()
            .position(|(_, t)| t.default)
            .or((!embedded.is_empty()).then_some(0));
        if let Some(SubtitleSource::Embedded(track)) = subtitle.map(|i| &subtitle_tracks[i].source)
        {
            decoder.select_subtitle(Some(*track));
        }
        let embedded_cues = Arc::new(Mutex::new(Cues::default()));
        let info = decoder.info();
        let fps = info.video.as_ref().map_or(30.0, |v| v.fps.max(1.0));
        let duration = info.duration_seconds;
        let stop = Arc::new(AtomicBool::new(false));
        let has_audio = decoder.enable_audio(crate::audio::RATE, crate::audio::CHANNELS);
        let audio_labels: Vec<String> = decoder
            .info()
            .audio_tracks
            .iter()
            .enumerate()
            .map(|(i, t)| audio_label(i, t))
            .collect();
        let audio_track = decoder.info().audio_track.filter(|_| has_audio);
        let audio = has_audio.then(|| {
            Arc::new(AudioShared {
                generation: AtomicU64::new(0),
                paused: AtomicBool::new(false),
                volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
                clock: Mutex::new(None),
            })
        });
        let audio_tx = audio.as_ref().map(|shared| {
            let (tx, rx) = mpsc::channel();
            spawn_audio("Video".into(), rx, shared.clone(), stop.clone());
            tx
        });
        Self {
            layout,
            image: Default::default(),
            last_subtitle: subtitle.unwrap_or(0),
            decode: spawn_decoder(
                decoder,
                start,
                audio_tx,
                stop.clone(),
                embedded_cues.clone(),
            ),
            subtitle_tracks,
            subtitle,
            embedded_cues,
            subtitle_notice: None,
            audio_labels,
            audio_track,
            audio,
            stop,
            generation: 0,
            current: None,
            next: None,
            ended: false,
            error: None,
            fps,
            duration,
            last_pts: start - 1.0 / fps,
            clock_start: None,
            paused_at: None,
            stats: PlayStats::default(),
        }
    }

    /// Adds subtitle files found next to the video; the first one is shown
    /// (a file placed next to a video is usually there to be used).
    pub fn add_external_subtitles(&mut self, files: Vec<crate::library::ExternalSubtitles>) {
        if files.is_empty() {
            return;
        }
        let count = files.len();
        let external = files.into_iter().map(|f| SubtitleTrack {
            label: f.name.clone(),
            source: SubtitleSource::External(Cues::new(f.cues)),
        });
        self.subtitle_tracks.splice(0..0, external);
        self.subtitle = self.subtitle.map(|i| i + count);
        self.select_subtitle(Some(0), false);
    }

    pub fn has_subtitles(&self) -> bool {
        !self.subtitle_tracks.is_empty()
    }

    pub fn subtitles_on(&self) -> bool {
        self.subtitle.is_some()
    }

    /// CC: subtitles off, or back on with the last track shown.
    pub fn toggle_subtitles(&mut self) {
        if self.subtitle_tracks.is_empty() {
            return;
        }
        let next = match self.subtitle {
            Some(_) => None,
            None => Some(self.last_subtitle.min(self.subtitle_tracks.len() - 1)),
        };
        self.select_subtitle(next, true);
    }

    fn select_subtitle(&mut self, index: Option<usize>, announce: bool) {
        if let Some(i) = index {
            self.last_subtitle = i;
        }
        let embedded =
            |i: Option<usize>, tracks: &[SubtitleTrack]| match i.map(|i| &tracks[i].source) {
                Some(SubtitleSource::Embedded(t)) => Some(*t),
                _ => None,
            };
        let (before, after) = (
            embedded(self.subtitle, &self.subtitle_tracks),
            embedded(index, &self.subtitle_tracks),
        );
        if before != after {
            let _ = self.decode.subtitle_track.send(after);
        }
        self.subtitle = index;
        if announce {
            let text = match index {
                Some(i) => format!("Subtitles: {}", self.subtitle_tracks[i].label),
                None => "Subtitles off".to_string(),
            };
            self.notice(text, Duration::from_millis(1800));
        }
    }

    /// Shows `text` where subtitles go for a while.
    pub fn notice(&mut self, text: String, duration: Duration) {
        self.subtitle_notice = Some((text, Instant::now() + duration));
    }

    pub fn audio_labels(&self) -> &[String] {
        &self.audio_labels
    }

    pub fn audio_index(&self) -> Option<usize> {
        self.audio_track
    }

    /// Switches to audio track `index`, continuing from the current picture.
    pub fn set_audio_track(&mut self, index: usize) {
        if self.audio_track.is_none()
            || self.audio_track == Some(index)
            || index >= self.audio_labels.len()
        {
            return;
        }
        let _ = self.decode.audio_track.send(index);
        self.audio_track = Some(index);
        self.notice(
            format!("Audio: {}", self.audio_labels[index]),
            Duration::from_millis(1800),
        );
        self.seek(self.position());
    }

    pub fn subtitle_labels(&self) -> Vec<String> {
        self.subtitle_tracks
            .iter()
            .map(|t| t.label.clone())
            .collect()
    }

    pub fn subtitle_index(&self) -> Option<usize> {
        self.subtitle
    }

    /// Shows track `index` (None: off), with a short notice.
    pub fn set_subtitle(&mut self, index: Option<usize>) {
        if index.is_none_or(|i| i < self.subtitle_tracks.len()) {
            self.select_subtitle(index, true);
        }
    }

    /// Off → each track in turn → off.
    pub fn cycle_subtitles(&mut self) {
        if self.subtitle_tracks.is_empty() {
            return;
        }
        let next = match self.subtitle {
            None => Some(0),
            Some(i) if i + 1 < self.subtitle_tracks.len() => Some(i + 1),
            Some(_) => None,
        };
        self.select_subtitle(next, true);
    }

    /// The subtitle for the frame on screen (or a short notice after switching).
    pub fn caption(&self) -> Option<crate::subtitles::Caption> {
        if let Some((text, until)) = &self.subtitle_notice
            && Instant::now() < *until
        {
            return Some(crate::subtitles::Caption::text(text.clone()));
        }
        let time = self.position();
        match &self.subtitle_tracks.get(self.subtitle?)?.source {
            SubtitleSource::External(cues) => cues.caption(time),
            SubtitleSource::Embedded(_) => self.embedded_cues.lock().expect("cues").caption(time),
        }
    }

    pub fn current(&self) -> Option<&Frame> {
        self.current.as_ref()
    }

    pub fn paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Media time shown at display time `now` (ns), once the clock started.
    pub fn media_time(&self, now: i64) -> Option<f64> {
        let now = self.paused_at.unwrap_or(now);
        self.clock_start.map(|start| (now - start) as f64 / 1e9)
    }

    /// Current position for the UI: the shown frame's time.
    pub fn position(&self) -> f64 {
        self.last_pts.max(0.0)
    }

    pub fn toggle_pause(&mut self, now: i64) {
        match self.paused_at.take() {
            // Shift the clock so playback continues where it stopped.
            Some(at) => {
                if let Some(start) = &mut self.clock_start {
                    *start += now - at;
                }
            }
            None => self.paused_at = Some(now),
        }
        if let Some(audio) = &self.audio {
            audio.paused.store(self.paused(), Ordering::Relaxed);
        }
    }

    /// Jumps to `seconds`; the current picture stays until the new one arrives.
    pub fn seek(&mut self, seconds: f64) {
        let target = seconds.clamp(0.0, (self.duration - 0.5).max(0.0));
        let from = self.position();
        self.generation += 1;
        self.decode
            .requested
            .store(self.generation, Ordering::Relaxed);
        if let Some(audio) = &self.audio {
            audio.generation.store(self.generation, Ordering::Relaxed);
            *audio.clock.lock().expect("audio clock") = None;
        }
        let _ = self.decode.control.send(SeekRequest {
            generation: self.generation,
            target,
            from,
            resume: false,
            requested: Instant::now(),
        });
        self.next = None;
        self.ended = false;
        self.error = None;
        self.clock_start = None;
        self.last_pts = target;
        // When paused, playback stays paused and shows the new position
        // (see `advance`).
    }

    /// Moves to the newest decoded frame due at `now`; true if it changed.
    pub fn advance(&mut self, now: i64) -> bool {
        if self.paused() && self.clock_start.is_some() {
            return false;
        }
        self.sync_to_audio(now);
        let mut media_time = self.media_time(now);
        let mut changed = false;
        loop {
            if self.next.is_none() && !self.ended {
                match self.decode.frames.try_recv() {
                    Ok(Decoded::Frame(g, frame)) if g == self.generation => self.next = Some(frame),
                    // Until the target frame starts the clock.
                    Ok(Decoded::Preview(g, frame)) if g == self.generation => {
                        if self.clock_start.is_none() {
                            self.current = Some(frame);
                            changed = true;
                        }
                        continue;
                    }
                    Ok(Decoded::End(g)) if g == self.generation => self.ended = true,
                    Ok(Decoded::Failed(g, e)) if g == self.generation => {
                        eprintln!("Decoding stopped: {e}");
                        self.error = Some(e);
                        self.ended = true;
                    }
                    Ok(_) => continue, // from before a seek
                    Err(mpsc::TryRecvError::Disconnected) => self.ended = true,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            let Some(candidate) = self.next.as_ref() else {
                break;
            };
            let pts = candidate.pts().unwrap_or(self.last_pts + 1.0 / self.fps);
            if media_time.is_some_and(|t| pts > t) {
                break;
            }
            if self.clock_start.is_none() {
                // The first frame starts the clock: it is due exactly now.
                self.clock_start = Some(now - (pts * 1e9) as i64);
                if let Some(at) = &mut self.paused_at {
                    *at = now;
                }
                media_time = Some(pts);
            }
            if changed {
                self.stats.skipped_frames += 1;
            }
            self.last_pts = pts;
            self.current = self.next.take();
            changed = true;
            if self.paused() {
                break; // paused after a seek: show just the target frame
            }
        }
        if let Some(t) = media_time {
            self.stats.media_seconds = t;
        }
        changed
    }

    /// Keeps the video clock on the audio clock (what is actually heard).
    fn sync_to_audio(&mut self, now: i64) {
        let (Some(audio), Some(start)) = (&self.audio, self.clock_start.as_mut()) else {
            return;
        };
        if self.paused_at.is_some() {
            return;
        }
        let Some((generation, heard)) = audio.heard_now() else {
            return;
        };
        if generation != self.generation {
            return;
        }
        // Frames are shown at the predicted display time, slightly after now.
        const DISPLAY_LEAD: f64 = 0.02;
        let video = (now - *start) as f64 / 1e9;
        let error = video - (heard + DISPLAY_LEAD);
        let correction = if error.abs() > 0.25 {
            error
        } else {
            error * 0.1
        };
        *start += (correction * 1e9) as i64;
    }

    /// How the last seek (or the start) went, once its frame was decoded.
    pub fn seek_report(&self) -> Option<SeekReport> {
        let report = self.decode.report.lock().expect("seek report");
        report
            .as_ref()
            .filter(|r| r.generation == self.generation)
            .cloned()
    }

    /// True once a frame from the latest seek (or the start) is on screen.
    pub fn settled(&self) -> bool {
        self.clock_start.is_some()
    }

    /// True once the stream ended and its last frame has been shown for a second.
    pub fn finished(&self, now: i64) -> bool {
        self.ended
            && self.next.is_none()
            && self.media_time(now).is_none_or(|t| t > self.last_pts + 1.0)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn quat_to_columns(q: xr::Quaternionf) -> [[f32; 4]; 3] {
    let (x, y, z, w) = (q.x, q.y, q.z, q.w);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + w * z),
            2.0 * (x * z - w * y),
            0.0,
        ],
        [
            2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x),
            0.0,
        ],
        [
            2.0 * (x * z + w * y),
            2.0 * (y * z - w * x),
            1.0 - 2.0 * (x * x + y * y),
            0.0,
        ],
    ]
}

fn mat_mul_t(a: [[f32; 3]; 3], b: [[f32; 4]; 3]) -> [[f32; 4]; 3] {
    // Aᵀ·B with A, B given as columns.
    let mut out = [[0.0; 4]; 3];
    for (j, col) in b.iter().enumerate() {
        for (i, a_col) in a.iter().enumerate() {
            out[j][i] = (0..3).map(|k| a_col[k] * col[k]).sum();
        }
    }
    out
}

/// Shader parameters for one eye.
pub fn eye_params(
    view: &xr::View,
    eye: usize,
    layout: &Layout,
    tex: (u32, u32),
    options: &ViewOptions,
    placement: &Placement,
    quarter_turns: u8,
) -> EyeParams {
    let (w, h) = (tex.0 as f32, tex.1 as f32);
    // Flat screen aspect from one eye's part of the frame (turned on its side
    // when the picture is rotated a quarter).
    let mut aspect = match layout.stereo {
        Stereo::SideBySide => w / 2.0 / h,
        Stereo::TopBottom => w / (h / 2.0),
        Stereo::Mono => w / h,
    };
    if quarter_turns % 2 == 1 {
        aspect = 1.0 / aspect.max(0.01);
    }
    // Express the eye in the placement's frame: the shader's screen/sphere is
    // fixed there, so rotating the placement moves the video around the viewer.
    let placed = placement.rotation();
    let p = view.pose.position;
    let origin = mat_mul_t(placed, [[p.x, p.y, p.z, 0.0], [0.0; 4], [0.0; 4]])[0];
    let curved = layout.projection == Projection::Flat && placement.curved;
    EyeParams {
        rot: mat_mul_t(placed, quat_to_columns(view.pose.orientation)),
        tan: [
            view.fov.angle_left.tan(),
            view.fov.angle_right.tan(),
            view.fov.angle_down.tan(),
            view.fov.angle_up.tan(),
        ],
        origin: [origin[0], origin[1], origin[2], eye as f32],
        mode: [
            match layout.projection {
                Projection::Flat if curved => 4.0,
                Projection::Flat => 0.0,
                Projection::Equirect180 => 1.0,
                Projection::Equirect360 => 2.0,
                Projection::Fisheye180 => 3.0,
            },
            match layout.stereo {
                Stereo::Mono => 0.0,
                Stereo::SideBySide => 1.0,
                Stereo::TopBottom => 2.0,
            },
            if layout.swap_eyes { 1.0 } else { 0.0 },
            options.fisheye_fov.to_radians(),
        ],
        screen: [
            options.screen_width * placement.zoom,
            options.screen_width * placement.zoom / aspect.max(0.1),
            placement.distance,
            0.0,
        ],
        tex: [w, h, options.debug_view as f32, placement.zoom],
    }
}
