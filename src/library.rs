//! Background SMB work for the headset browser. Network calls never run on the
//! XR frame loop: requests go to worker threads and results come back on a
//! channel. Navigation and opening use one worker; playability probes have
//! their own (a dispatcher answering from the probe cache, and a few workers
//! reading headers), so a slow probe never delays browsing.

use crate::config::{self, LayoutOverride, Server};
use crate::media::{Media, VideoDecoder, VideoInfo};
use crate::playability::{self, Assessment, Platform};
use crate::probe_cache::ProbeCache;
use crate::readahead::ReadAhead;
use crate::smb::{Entry, SmbSession, SmbUrl};
use crate::thumb_cache::{self, THUMB_H, THUMB_W, ThumbCache};
use crate::vr::{self, Layout};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

pub type Path = Vec<String>;

pub enum Request {
    Shares {
        id: u64,
        server: Server,
    },
    List {
        id: u64,
        server: Server,
        share: String,
        path: Path,
    },
    /// Marks a folder's videos: from the probe cache at once, the others by
    /// reading their headers, a few at a time, top row first.
    ProbeFolder {
        generation: u64,
        server: Server,
        share: String,
        folder: Path,
        videos: Vec<ProbeVideo>,
    },
    /// Makes list thumbnails for a folder's videos, nearest `first_visible`
    /// first, on a low-priority worker that waits for probes and playback.
    /// Sent again with the same generation, it adds videos and updates the
    /// layout of known ones (merged by name); a new generation (see
    /// [`Library::set_thumbnail_generation`]) replaces the queue.
    ThumbnailFolder {
        generation: u64,
        server: Server,
        share: String,
        folder: Path,
        videos: Vec<ThumbVideo>,
        first_visible: usize,
    },
    /// The list scrolled: pending thumbnails are reordered around this video
    /// (an index as in [`ThumbVideo::index`]). Ignored for other generations.
    ThumbnailFocus {
        generation: u64,
        first_visible: usize,
    },
    Open {
        id: u64,
        server: Server,
        share: String,
        path: Path,
    },
    Rename {
        id: u64,
        server: Server,
        share: String,
        path: Path,
        new_name: String,
    },
    Delete {
        id: u64,
        server: Server,
        share: String,
        path: Path,
    },
    /// Signs in connections for playing a video from `share` ahead of time
    /// (see [`Standby`]): a folder with videos is on screen.
    Warm {
        server: Server,
        share: String,
    },
    /// Checks the login (and that shares can be listed), then saves the server.
    AddServer {
        id: u64,
        server: Server,
        password: String,
    },
}

/// A video to mark, as the folder listing describes it.
#[derive(Clone, Debug)]
pub struct ProbeVideo {
    pub name: String,
    pub size: u64,
    /// Last write time from the listing (see [`crate::smb::Entry`]).
    pub modified: u64,
}

/// A video to make a thumbnail for (see [`Request::ThumbnailFolder`]).
#[derive(Clone, Debug)]
pub struct ThumbVideo {
    /// Position among the folder's videos, for the order of work.
    pub index: usize,
    pub name: String,
    pub size: u64,
    /// Last write time from the listing (see [`crate::smb::Entry`]).
    pub modified: u64,
    /// As probed, with the user's override: decides the crop.
    pub layout: Layout,
}

pub struct Opened {
    pub decoder: VideoDecoder,
    pub layout: Layout,
    pub assessment: Assessment,
    pub name: String,
    /// Identifies the file for its saved layout and resume point.
    pub key: String,
    /// Subtitle files next to the video (`movie.srt`, `movie.en.srt`).
    pub external_subtitles: Vec<ExternalSubtitles>,
    /// Saved picture corrections for this file.
    pub image: config::ImageAdjust,
    /// Where this file was left last time (seconds), to continue from.
    pub resume: Option<f64>,
}

pub struct ExternalSubtitles {
    /// File name, e.g. `movie.en.srt`.
    pub name: String,
    pub cues: Vec<crate::subtitles::Cue>,
}

/// Loads the .srt files next to `path`. Missing or unreadable ones are
/// skipped: subtitles must never stop a video from playing.
fn load_sidecars(session: &SmbSession, share: &str, path: &Path) -> Vec<ExternalSubtitles> {
    use std::io::Read;
    let Some((video, folder)) = path.split_last() else {
        return Vec::new();
    };
    let entries = match session.list_in(share, &smb_path(&folder.to_vec())) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Subtitles: can't list the folder: {e:#}");
            return Vec::new();
        }
    };
    let mut names: Vec<String> = entries
        .into_iter()
        .filter(|e| !e.is_dir && e.size <= 8 << 20 && crate::subtitles::is_sidecar(video, &e.name))
        .map(|e| e.name)
        .collect();
    // `movie.srt` first, then language variants.
    names.sort_by_key(|n| (n.len(), n.clone()));
    let small = ReadAhead {
        block_size: 256 * 1024,
        blocks_ahead: 4,
    };
    names
        .into_iter()
        .filter_map(|name| {
            let mut file = folder.to_vec();
            file.push(name.clone());
            let mut bytes = Vec::new();
            let read = session
                .open_in(share, &smb_path(&file), small)
                .and_then(|mut r| Ok(r.read_to_end(&mut bytes)?));
            if let Err(e) = read {
                eprintln!("Subtitles: can't read {name}: {e:#}");
                return None;
            }
            let cues = crate::subtitles::parse_srt(&crate::subtitles::decode_text(&bytes));
            eprintln!("Subtitles: {name}: {} cues", cues.len());
            (!cues.is_empty()).then_some(ExternalSubtitles { name, cues })
        })
        .collect()
}

/// Phase times of opening a video, logged as one `Timing:` line.
pub struct OpenTiming {
    requested: std::time::Instant,
    last: std::time::Instant,
    phases: Vec<(&'static str, f64)>,
    notes: Vec<String>,
}

impl OpenTiming {
    pub fn new(requested: std::time::Instant) -> Self {
        Self {
            requested,
            last: std::time::Instant::now(),
            phases: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Ends the phase that started at the last lap.
    pub fn lap(&mut self, phase: &'static str) {
        let now = std::time::Instant::now();
        self.phases
            .push((phase, (now - self.last).as_secs_f64() * 1e3));
        self.last = now;
    }

    pub fn note(&mut self, note: String) {
        self.notes.push(note);
    }

    /// Milliseconds spent in `phase`.
    pub fn phase_ms(&self, phase: &str) -> Option<f64> {
        self.phases.iter().find(|p| p.0 == phase).map(|p| p.1)
    }

    pub fn phases(&self) -> Vec<(String, f64)> {
        self.phases
            .iter()
            .map(|(p, ms)| (p.to_string(), *ms))
            .collect()
    }

    pub fn total_ms(&self) -> f64 {
        self.requested.elapsed().as_secs_f64() * 1e3
    }

    pub fn log(&self, name: &str) {
        let phases: Vec<String> = self
            .phases
            .iter()
            .map(|(p, ms)| format!("{p} {ms:.0}"))
            .chain(self.notes.iter().cloned())
            .collect();
        eprintln!(
            "Timing: open {name}: {:.0} ms ({})",
            self.total_ms(),
            phases.join(", ")
        );
    }
}

/// Opens `path` for playback on its own `sessions` (the file is read over
/// all of them; see [`SmbSession::open_striped`]): subtitles next to it,
/// the file, its streams, then the decoder.
pub fn open_video(
    sessions: &[Arc<SmbSession>],
    share: &str,
    path: &Path,
    key: String,
    hw: Option<&str>,
    timing: &mut OpenTiming,
) -> Result<Box<Opened>, String> {
    let err = |e: anyhow::Error| format!("{e:#}");
    let name = path.last().cloned().unwrap_or_default();
    let session = sessions.first().ok_or("No connection")?;
    // Subtitle files load alongside: their folder listing and reads are
    // round trips the video doesn't need to wait for.
    let (external_subtitles, media) = std::thread::scope(|scope| {
        let sidecars = scope.spawn(|| {
            let started = std::time::Instant::now();
            let found = load_sidecars(session, share, path);
            (found, started.elapsed().as_secs_f64() * 1e3)
        });
        let media =
            SmbSession::open_striped(sessions, share, &smb_path(path), ReadAhead::default())
                .map_err(err)
                .and_then(|reader| {
                    timing.lap("open file");
                    Media::open(&name, reader).map_err(err)
                });
        timing.lap("probe");
        let (found, ms) = sidecars.join().unwrap_or_default();
        timing.lap("rest of sidecars");
        timing.note(format!("sidecars {ms:.0} alongside"));
        (found, media)
    });
    let media = media?;
    timing.note(format!("probe read {}", media.io()));
    let video = media.info().video.clone();
    let assessment = playability::assess(Platform::current(), video.as_ref());
    let mut layout = vr::detect(&name, video.as_ref());
    let saved = config::layout_override(&key).ok().flatten();
    if let Some(saved) = saved {
        saved.apply(&mut layout);
    }
    let image = saved.map(|s| s.image).unwrap_or_default();
    // The last video's decoder closes in the background; wait for
    // it, or the hardware decoder is still busy.
    if !crate::media::wait_for_decoders_closed(std::time::Duration::from_secs(10)) {
        eprintln!("Library: the previous video's decoder is still closing");
    }
    timing.lap("wait for last decoder");
    let mut decoder = media.into_decoder(hw, true, "").map_err(err)?;
    timing.lap("open decoder");
    decoder.requested_at = Some(timing.requested);
    let resume = config::resume_position(&key);
    timing.log(&name);
    Ok(Box::new(Opened {
        decoder,
        layout,
        assessment,
        name,
        key,
        external_subtitles,
        image,
        resume,
    }))
}

pub enum Response {
    Shares {
        id: u64,
        result: Result<Vec<String>, String>,
    },
    List {
        id: u64,
        result: Result<Vec<Entry>, String>,
    },
    Probe {
        generation: u64,
        name: String,
        result: Result<Probed, String>,
        /// Answered from the probe cache, without reading the file.
        cached: bool,
    },
    Opened {
        id: u64,
        result: Result<Box<Opened>, String>,
    },
    /// One finished thumbnail (see [`Request::ThumbnailFolder`]), sent as
    /// each is ready. A video that can't be read gets none.
    Thumbnail {
        generation: u64,
        name: String,
        image: Arc<crate::media::Thumb>,
    },
    /// Rename or delete finished.
    Changed { id: u64, result: Result<(), String> },
    ServerAdded {
        id: u64,
        result: Result<Server, String>,
    },
}

/// What a folder listing shows about a video before it is opened.
#[derive(Clone, Debug)]
pub struct Probed {
    pub assessment: Assessment,
    /// Detected, or as the user last set it for this file.
    pub layout: Layout,
}

pub struct Library {
    main: mpsc::Sender<(Request, std::time::Instant)>,
    probes: mpsc::Sender<(Request, std::time::Instant)>,
    responses: mpsc::Receiver<Response>,
    /// Probes for other generations (folders left behind) are skipped.
    probe_generation: Arc<AtomicU64>,
    /// Videos being opened (requested and not yet answered).
    opening: Arc<AtomicU64>,
    warmer: mpsc::Sender<(Request, std::time::Instant)>,
    thumbnails: mpsc::Sender<Request>,
    /// Thumbnails for other generations are dropped. Separate from the
    /// probes', so switching them off doesn't cancel probing.
    thumbnail_generation: Arc<AtomicU64>,
}

/// Headers read at once. Probing waits mostly on the network (~250 ms per
/// video on the headset, ~90 ms of it CPU), so a few in parallel mark a
/// folder several times faster.
const PROBE_WORKERS: usize = 3;

/// One video to probe (see [`Request::ProbeFolder`]).
struct ProbeJob {
    generation: u64,
    server: Server,
    share: String,
    path: Path,
    size: u64,
    modified: u64,
}

/// What a connection is used for. Browsing, playability probes and thumbnails
/// keep one connection each per server; every playing video gets its own (see
/// Open). Thumbnails read megabytes per video: on the probes' connection they
/// would hold up header reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Purpose {
    Browse,
    Probe,
    Thumbnail,
}

/// One connection per server and purpose. Each has its own lock, so probe
/// workers wait for one connect instead of each making their own, while
/// browsing never waits on a probe connection.
type Sessions = Arc<Mutex<HashMap<(String, Purpose), Arc<Mutex<Option<Arc<SmbSession>>>>>>>;

/// Key of a file's saved layout override.
pub fn file_key(server: &Server, share: &str, path: &Path) -> String {
    format!("{}/{share}/{}", server.url, path.join("/"))
}

/// Connections a playing video reads over (see [`SmbSession::open_striped`]).
pub const VIDEO_CONNECTIONS: usize = 2;

/// Connects [`VIDEO_CONNECTIONS`] sessions at once for one video, signed in
/// straight to `share` (not to the server's IPC$ first: a round trip less);
/// only the first must succeed.
fn connect_lanes(server: &Server, share: &str) -> Result<Vec<Arc<SmbSession>>, String> {
    std::thread::scope(|scope| {
        let extra: Vec<_> = (1..VIDEO_CONNECTIONS)
            .map(|_| scope.spawn(|| connect_to(server, Some(share), None)))
            .collect();
        let mut sessions = vec![connect_to(server, Some(share), None)?];
        for handle in extra {
            match handle.join() {
                Ok(Ok(session)) => sessions.push(session),
                Ok(Err(e)) => eprintln!("Library: playing over one connection less: {e}"),
                Err(_) => {}
            }
        }
        Ok(sessions)
    })
}

/// Connections for the next video, signed in to a share while its folder is
/// on screen, so opening a video skips connecting (25-115 ms on the headset).
pub struct Standby {
    server: String,
    share: String,
    made: std::time::Instant,
    sessions: Vec<Arc<SmbSession>>,
}

/// Standby connections older than this aren't used: servers drop idle
/// sessions (Windows after 15 minutes).
const STANDBY_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// Showing a folder again replaces standby connections older than this.
const STANDBY_REFRESH: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// How long standby connections may take to open a video before a fresh
/// connection is made instead.
const STANDBY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// The standby connections, if they are for this share and fresh.
fn take_standby(
    standby: &Mutex<Option<Standby>>,
    server: &Server,
    share: &str,
) -> Option<Vec<Arc<SmbSession>>> {
    let taken = standby.lock().expect("standby").take()?;
    if taken.server == server.url && taken.share == share && taken.made.elapsed() < STANDBY_MAX_AGE
    {
        Some(taken.sessions)
    } else {
        crate::xr::app::drop_in_background(taken);
        None
    }
}

/// Makes standby connections for `share`, unless fresh ones are there.
fn warm(standby: &Mutex<Option<Standby>>, server: &Server, share: &str) {
    if let Some(s) = &*standby.lock().expect("standby")
        && s.server == server.url
        && s.share == share
        && s.made.elapsed() < STANDBY_REFRESH
    {
        return;
    }
    match connect_lanes(server, share) {
        Ok(sessions) => {
            let fresh = Standby {
                server: server.url.clone(),
                share: share.to_string(),
                made: std::time::Instant::now(),
                sessions,
            };
            if let Some(old) = standby.lock().expect("standby").replace(fresh) {
                crate::xr::app::drop_in_background(old);
            }
        }
        Err(e) => eprintln!("Library: no standby connections: {e}"),
    }
}

fn connect(server: &Server, password: Option<String>) -> Result<Arc<SmbSession>, String> {
    connect_to(server, None, password)
}

/// Signs in to `server`, to `share` if given (see [`SmbSession::connect`]).
fn connect_to(
    server: &Server,
    share: Option<&str>,
    password: Option<String>,
) -> Result<Arc<SmbSession>, String> {
    let mut url: SmbUrl = server.url.parse().map_err(|e| format!("{e:#}"))?;
    if let Some(share) = share {
        url.share = share.to_string();
    }
    let password = match password {
        Some(p) => p,
        None => config::password(&server.url)
            .map_err(|e| format!("{e:#}"))?
            .unwrap_or_default(),
    };
    Ok(Arc::new(SmbSession::connect(url, password).map_err(
        |e| format!("Can't connect to {}: {e:#}", server.name),
    )?))
}

fn session(
    sessions: &Sessions,
    server: &Server,
    purpose: Purpose,
) -> Result<Arc<SmbSession>, String> {
    let slot = sessions
        .lock()
        .expect("sessions")
        .entry((server.url.clone(), purpose))
        .or_default()
        .clone();
    let mut slot = slot.lock().expect("session slot");
    if let Some(s) = &*slot {
        return Ok(s.clone());
    }
    let session = connect(server, None)?;
    *slot = Some(session.clone());
    Ok(session)
}

/// Forgets a server connection after a failure so the next request reconnects;
/// a broken connection must never wedge browsing until the app restarts.
/// With `failed`, only if that is still the connection (another worker may
/// have replaced it already).
fn evict(sessions: &Sessions, server: &Server, purpose: Purpose, failed: Option<&Arc<SmbSession>>) {
    let slot = sessions
        .lock()
        .expect("sessions")
        .get(&(server.url.clone(), purpose))
        .cloned();
    let Some(slot) = slot else { return };
    let mut slot = slot.lock().expect("session slot");
    if failed.is_some_and(|f| !slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, f))) {
        return;
    }
    // Dropping a wedged session can block on its logoff; never here.
    if let Some(session) = slot.take() {
        eprintln!(
            "Library: reconnecting to {} ({purpose:?}) on next request",
            server.name
        );
        crate::xr::app::drop_in_background(session);
    }
}

fn smb_path(path: &Path) -> String {
    path.join("\\")
}

fn handle(
    request: Request,
    requested: std::time::Instant,
    sessions: &Sessions,
    cache: &Mutex<ProbeCache>,
    standby: &Mutex<Option<Standby>>,
    hw: Option<&str>,
) -> Response {
    let started = std::time::Instant::now();
    let (server, what) = match &request {
        Request::Shares { server, .. } => (server.clone(), "shares"),
        Request::List { server, .. } => (server.clone(), "list"),
        Request::ProbeFolder { server, .. } => (server.clone(), "probe"),
        Request::Open { server, .. } => (server.clone(), "open"),
        Request::Rename { server, .. } => (server.clone(), "rename"),
        Request::Delete { server, .. } => (server.clone(), "delete"),
        Request::AddServer { server, .. } => (server.clone(), "add server"),
        Request::Warm { server, .. } => (server.clone(), "warm"),
        Request::ThumbnailFolder { .. } | Request::ThumbnailFocus { .. } => {
            unreachable!("thumbnails have their own worker")
        }
    };
    let response = run(request, requested, sessions, cache, standby, hw);
    let failure = match &response {
        Response::Shares { result: Err(e), .. }
        | Response::List { result: Err(e), .. }
        | Response::Opened { result: Err(e), .. }
        | Response::Changed { result: Err(e), .. }
        | Response::ServerAdded { result: Err(e), .. } => Some(e.clone()),
        _ => None,
    };
    if let Some(e) = failure {
        eprintln!(
            "Library: {what} failed after {:.1}s: {e}",
            started.elapsed().as_secs_f64()
        );
        if connection_lost(&e) {
            match what {
                // A video has its own connection, which closes with it.
                "open" | "add server" => {}
                _ => evict(sessions, &server, Purpose::Browse, None),
            }
        }
    } else if started.elapsed().as_secs_f64() > 2.0 {
        eprintln!(
            "Library: {what} took {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }
    response
}

/// Whether an error means the connection itself is broken (so it must be
/// replaced), not just this request: unanswered requests, and a connection
/// or session the server closed, e.g. an idle disconnect while the headset
/// slept (Windows drops idle sessions after 15 minutes).
fn connection_lost(error: &str) -> bool {
    [
        "did not answer",
        "stopped sending",
        "connection is stopped",
        "Not connected",
        "IO Error",
        "Network Session Expired",
        "User Session Deleted",
        "Network Name Deleted",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

fn run(
    request: Request,
    requested: std::time::Instant,
    sessions: &Sessions,
    cache: &Mutex<ProbeCache>,
    standby: &Mutex<Option<Standby>>,
    hw: Option<&str>,
) -> Response {
    let err = |e: anyhow::Error| format!("{e:#}");
    match request {
        Request::Shares { id, server } => Response::Shares {
            id,
            result: session(sessions, &server, Purpose::Browse)
                .and_then(|s| s.shares().map_err(err)),
        },
        Request::List {
            id,
            server,
            share,
            path,
        } => Response::List {
            id,
            result: session(sessions, &server, Purpose::Browse)
                .and_then(|s| s.list_in(&share, &smb_path(&path)).map_err(err)),
        },
        Request::ProbeFolder { .. }
        | Request::Warm { .. }
        | Request::ThumbnailFolder { .. }
        | Request::ThumbnailFocus { .. } => {
            unreachable!("probes, warming and thumbnails have their own workers")
        }
        Request::Open {
            id,
            server,
            share,
            path,
        } => {
            // A dedicated connection per video: if it wedges, only this video
            // is affected, and it closes when the video does.
            let mut timing = OpenTiming::new(requested);
            let key = file_key(&server, &share, &path);
            let warm = take_standby(standby, &server, &share);
            let mut result = None;
            if let Some(sessions) = warm {
                timing.lap("connect");
                timing.note("warm connections".into());
                // Idle connections may have died (the server restarted, the
                // headset slept): give up on them quickly.
                for s in &sessions {
                    s.set_deadline(Some(STANDBY_DEADLINE));
                }
                match open_video(&sessions, &share, &path, key.clone(), hw, &mut timing) {
                    Err(e) if connection_lost(&e) => {
                        eprintln!("Library: warm connections failed ({e}); connecting again");
                        crate::xr::app::drop_in_background(sessions);
                    }
                    opened => {
                        for s in &sessions {
                            s.set_deadline(None);
                        }
                        result = Some(opened);
                    }
                }
            }
            let result = result.unwrap_or_else(|| {
                connect_lanes(&server, &share).and_then(|sessions| {
                    timing.lap("connect");
                    open_video(&sessions, &share, &path, key, hw, &mut timing)
                })
            });
            Response::Opened { id, result }
        }
        Request::Rename {
            id,
            server,
            share,
            path,
            new_name,
        } => {
            let result = session(sessions, &server, Purpose::Browse).and_then(|s| {
                s.rename_in(&share, &smb_path(&path), &new_name)
                    .map_err(err)?;
                // Keep a saved VR layout with the file.
                let mut renamed = path.clone();
                if let Some(last) = renamed.last_mut() {
                    *last = new_name.clone();
                }
                let (from, to) = (
                    file_key(&server, &share, &path),
                    file_key(&server, &share, &renamed),
                );
                let _ = config::move_layout_override(&from, &to);
                let _ = config::move_resume_position(&from, &to);
                cache.lock().expect("probe cache").rename(&from, &to);
                ProbeCache::save(cache);
                Ok(())
            });
            Response::Changed { id, result }
        }
        Request::Delete {
            id,
            server,
            share,
            path,
        } => {
            let result = session(sessions, &server, Purpose::Browse).and_then(|s| {
                s.delete_in(&share, &smb_path(&path)).map_err(err)?;
                let key = file_key(&server, &share, &path);
                let _ = config::save_layout_override(&key, None);
                let _ = config::save_resume_position(&key, None);
                cache.lock().expect("probe cache").remove(&key);
                ProbeCache::save(cache);
                Ok(())
            });
            Response::Changed { id, result }
        }
        Request::AddServer {
            id,
            server,
            password,
        } => {
            let result = connect(&server, Some(password.clone())).and_then(|s| {
                s.shares().map_err(err)?;
                config::save_server(server.clone(), &password).map_err(err)?;
                Ok(server)
            });
            Response::ServerAdded { id, result }
        }
    }
}

/// Whether a video is opening or playing: probes read other files over the
/// same link (and disks), slowing it.
fn video_busy(opening: &AtomicU64) -> bool {
    opening.load(Ordering::SeqCst) > 0 || crate::media::open_decoders() > 0
}

/// Holds a probe while a video opens or plays. False when the probe's
/// folder was left meanwhile.
fn wait_for_quiet(current: &AtomicU64, generation: u64, opening: &AtomicU64) -> bool {
    loop {
        if generation != current.load(Ordering::Relaxed) {
            return false;
        }
        if !video_busy(opening) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A probe's reads, which stop as soon as a video starts opening; the
/// probe then runs again later. FFmpeg sees only a read error (and might
/// even succeed with what it read), so `paused` tells the probe.
struct Yielding<R> {
    inner: R,
    opening: Arc<AtomicU64>,
    paused: Arc<std::sync::atomic::AtomicBool>,
}

impl<R: std::io::Read> std::io::Read for Yielding<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if video_busy(&self.opening) {
            self.paused.store(true, Ordering::SeqCst);
            return Err(std::io::Error::other("Probe paused for playback"));
        }
        self.inner.read(buf)
    }
}

impl<R: std::io::Seek> std::io::Seek for Yielding<R> {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(from)
    }
}

/// How a folder marks a video, from what its header says.
fn probed(name: &str, video: Option<&VideoInfo>, saved: Option<&LayoutOverride>) -> Probed {
    // Marked as it will play: a format the user picked wins.
    let mut layout = vr::detect(name, video);
    if let Some(saved) = saved {
        saved.apply(&mut layout);
    }
    Probed {
        assessment: playability::assess(Platform::current(), video),
        layout,
    }
}

/// Answers what the probe cache knows and queues the rest for the workers.
fn dispatch_probes(
    request: Request,
    cache: &Mutex<ProbeCache>,
    jobs: &mpsc::Sender<ProbeJob>,
    queued: &AtomicU64,
    out: &mpsc::Sender<Response>,
) {
    let Request::ProbeFolder {
        generation,
        server,
        share,
        folder,
        videos,
    } = request
    else {
        return;
    };
    let overrides = config::layout_overrides().unwrap_or_default();
    for video in videos {
        let mut path = folder.clone();
        path.push(video.name.clone());
        let key = file_key(&server, &share, &path);
        let hit = cache
            .lock()
            .expect("probe cache")
            .get(&key, video.size, video.modified);
        match hit {
            Some(info) => {
                let result = Ok(probed(&video.name, info.as_ref(), overrides.get(&key)));
                let _ = out.send(Response::Probe {
                    generation,
                    name: video.name,
                    result,
                    cached: true,
                });
            }
            None => {
                queued.fetch_add(1, Ordering::SeqCst);
                let _ = jobs.send(ProbeJob {
                    generation,
                    server: server.clone(),
                    share: share.clone(),
                    path,
                    size: video.size,
                    modified: video.modified,
                });
            }
        }
    }
}

/// Reads one video's header. `None` when the probe paused for playback.
fn probe(
    job: &ProbeJob,
    sessions: &Sessions,
    opening: &Arc<AtomicU64>,
) -> Option<Result<Option<VideoInfo>, String>> {
    let err = |e: anyhow::Error| format!("{e:#}");
    // Header probes read little: small blocks, shallow read-ahead.
    let small = ReadAhead {
        block_size: 256 * 1024,
        blocks_ahead: 4,
    };
    let name = job.path.last().cloned().unwrap_or_default();
    let session = match session(sessions, &job.server, Purpose::Probe) {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = session
        .open_in(&job.share, &smb_path(&job.path), small)
        .map_err(err)
        .and_then(|reader| {
            let reader = Yielding {
                inner: reader,
                opening: opening.clone(),
                paused: paused.clone(),
            };
            Media::open(&name, reader).map_err(err)
        })
        .map(|media| media.info().video.clone());
    if paused.load(Ordering::SeqCst) {
        return None;
    }
    match result {
        Err(e) => {
            // A bad file is not a connection problem; a stalled server is.
            if connection_lost(&e) {
                eprintln!("Library: probe of {name} failed: {e}");
                evict(sessions, &job.server, Purpose::Probe, Some(&session));
            }
            Some(Err(e))
        }
        ok => Some(ok),
    }
}

/// Pending thumbnails for the folder on screen, nearest the view first.
#[derive(Default)]
struct ThumbQueue {
    generation: u64,
    /// Which folder the generation is for (a new one starts over).
    folder: Option<(String, String, Path)>,
    server: Option<Server>,
    first_visible: usize,
    pending: Vec<ThumbPending>,
    /// Made, or failed (never retried), with the layout used.
    done: HashMap<String, Layout>,
}

struct ThumbPending {
    video: ThumbVideo,
    /// Looked up in the disk cache already.
    checked: bool,
}

impl ThumbQueue {
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Takes in a request, unless it is for a generation that has passed.
    fn apply(&mut self, request: Request, current: u64) {
        match request {
            Request::ThumbnailFolder {
                generation,
                server,
                share,
                folder,
                videos,
                first_visible,
            } if generation == current => {
                let id = (server.url.clone(), share, folder);
                if self.generation != generation || self.folder.as_ref() != Some(&id) {
                    self.clear();
                    self.generation = generation;
                    self.folder = Some(id);
                }
                self.server = Some(server);
                self.first_visible = first_visible;
                for video in videos {
                    self.add(video);
                }
            }
            Request::ThumbnailFocus {
                generation,
                first_visible,
            } if generation == self.generation => self.first_visible = first_visible,
            _ => {}
        }
    }

    fn add(&mut self, video: ThumbVideo) {
        match self.done.get(&video.name) {
            Some(layout) if *layout == video.layout => return,
            // The layout changed since: a different picture.
            Some(_) => {
                self.done.remove(&video.name);
            }
            None => {}
        }
        match self.pending.iter_mut().find(|p| p.video.name == video.name) {
            Some(p) => {
                let changed = (p.video.size, p.video.modified, p.video.layout)
                    != (video.size, video.modified, video.layout);
                p.checked &= !changed;
                p.video = video;
            }
            None => self.pending.push(ThumbPending {
                video,
                checked: false,
            }),
        }
    }

    /// Smaller is sooner: by distance from the view, rows below it a little
    /// ahead of rows above it (the way people scroll).
    fn rank(&self, video: &ThumbVideo) -> usize {
        video.index.abs_diff(self.first_visible) * 2 + (video.index < self.first_visible) as usize
    }

    /// Position of the soonest pending video, among those not yet looked up
    /// in the disk cache (`unchecked`) or all.
    fn soonest(&self, unchecked: bool) -> Option<usize> {
        (0..self.pending.len())
            .filter(|&i| !unchecked || !self.pending[i].checked)
            .min_by_key(|&i| self.rank(&self.pending[i].video))
    }

    /// Takes a video off the queue as made or given up on.
    fn finish(&mut self, position: usize) -> ThumbVideo {
        let video = self.pending.remove(position).video;
        self.done.insert(video.name.clone(), video.layout);
        video
    }
}

/// Lowers this thread's priority, so thumbnails (and the decoder threads they
/// start) give way to the app and the video: on Linux, `setpriority` with a
/// thread id affects that thread only.
fn lower_priority() {
    // SAFETY: plain system calls with no pointers.
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

/// Thumbnail reads: a keyframe of an 8K video is several MB, so bigger
/// blocks than a header probe's.
const THUMB_READ_AHEAD: ReadAhead = ReadAhead {
    block_size: 1024 * 1024,
    blocks_ahead: 4,
};
/// How far into the video the picture is taken.
const THUMB_AT: f64 = 0.10;

/// Makes one thumbnail. `Ok(None)` when it stopped, for playback or because
/// the folder was left; the job is then queued again.
fn make_thumbnail(
    video: &ThumbVideo,
    queue: &ThumbQueue,
    sessions: &Sessions,
    opening: &Arc<AtomicU64>,
    current: &AtomicU64,
) -> Result<Option<crate::media::Thumb>, String> {
    let (Some(server), Some((_, share, folder))) = (&queue.server, &queue.folder) else {
        return Ok(None);
    };
    let generation = queue.generation;
    let mut path = folder.clone();
    path.push(video.name.clone());
    let session = session(sessions, server, Purpose::Thumbnail)?;
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = || video_busy(opening) || current.load(Ordering::Relaxed) != generation;
    let result = session
        .open_in(share, &smb_path(&path), THUMB_READ_AHEAD)
        .map_err(|e| format!("{e:#}"))
        .and_then(|reader| {
            let reader = Yielding {
                inner: reader,
                opening: opening.clone(),
                paused: paused.clone(),
            };
            crate::media::thumbnail_unless(
                &video.name,
                reader,
                THUMB_AT,
                &video.layout,
                THUMB_W,
                THUMB_H,
                &stop,
            )
            .map_err(|e| format!("{e:#}"))
        });
    if paused.load(Ordering::SeqCst) {
        return Ok(None);
    }
    if let Err(e) = &result
        && connection_lost(e)
    {
        evict(sessions, server, Purpose::Thumbnail, Some(&session));
    }
    result
}

/// The thumbnail worker: serves disk cache hits as soon as they are asked
/// for (a small local read), and makes the others one at a time, only while
/// no probe is pending and no video is opening or playing.
fn thumbnail_worker(
    requests: mpsc::Receiver<Request>,
    sessions: Sessions,
    out: mpsc::Sender<Response>,
    queued: Arc<AtomicU64>,
    opening: Arc<AtomicU64>,
    current: Arc<AtomicU64>,
) {
    lower_priority();
    let mut cache = ThumbCache::open();
    if let Some(cache) = &cache {
        cache.prune_old();
    }
    let mut queue = ThumbQueue::default();
    let send = |queue: &ThumbQueue, name: String, image: crate::media::Thumb| {
        out.send(Response::Thumbnail {
            generation: queue.generation,
            name,
            image: Arc::new(image),
        })
        .is_ok()
    };
    loop {
        // Scrolling and new folders apply between jobs.
        loop {
            match requests.try_recv() {
                Ok(request) => queue.apply(request, current.load(Ordering::Relaxed)),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if queue.generation != current.load(Ordering::Relaxed) {
            queue.clear();
        }
        let key_of = |queue: &ThumbQueue, video: &ThumbVideo| {
            let (Some(server), Some((_, share, folder))) = (&queue.server, &queue.folder) else {
                return String::new();
            };
            let mut path = folder.clone();
            path.push(video.name.clone());
            thumb_cache::key(
                &file_key(server, share, &path),
                video.size,
                video.modified,
                &video.layout,
            )
        };
        // Cache hits first, nearest the view first, even while probes run.
        if let Some(position) = queue.soonest(true) {
            let key = key_of(&queue, &queue.pending[position].video);
            match cache.as_ref().and_then(|c| c.load(&key)) {
                Some(image) => {
                    let video = queue.finish(position);
                    if !send(&queue, video.name, image) {
                        return;
                    }
                }
                None => queue.pending[position].checked = true,
            }
            continue;
        }
        let waiting = queued.load(Ordering::SeqCst) > 0 || video_busy(&opening);
        let Some(position) = queue.soonest(false).filter(|_| !waiting) else {
            // Nothing to make (or not now): wait for a request, or look again.
            let request = if queue.pending.is_empty() {
                requests.recv().map_err(|_| ())
            } else {
                match requests.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(request) => Ok(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
                }
            };
            match request {
                Ok(request) => queue.apply(request, current.load(Ordering::Relaxed)),
                Err(()) => return,
            }
            continue;
        };
        let key = key_of(&queue, &queue.pending[position].video);
        let video = queue.pending[position].video.clone();
        let started = std::time::Instant::now();
        match make_thumbnail(&video, &queue, &sessions, &opening, &current) {
            Ok(Some(image)) => {
                queue.finish(position);
                if let Some(cache) = &mut cache {
                    cache.store(&key, &image);
                }
                if current.load(Ordering::Relaxed) == queue.generation
                    && !send(&queue, video.name.clone(), image)
                {
                    return;
                }
                eprintln!(
                    "Thumbnails: {} in {:.0} ms",
                    video.name,
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
            // Stopped for a video (or the folder was left): the job stays
            // queued, and runs again once it is quiet (a left folder's queue
            // is dropped at the top of the loop).
            Ok(None) => {
                let stale = queue.generation != current.load(Ordering::Relaxed);
                if stale {
                    eprintln!("Thumbnails: {} stopped, folder left", video.name);
                } else {
                    eprintln!("Thumbnails: {} paused for playback", video.name);
                }
            }
            Err(e) => {
                // A bad file keeps its icon; never retried in this folder.
                eprintln!("Thumbnails: {} failed: {e}", video.name);
                queue.finish(position);
            }
        }
    }
}

impl Library {
    /// `hw` is the preferred hardware backend for playback (see `media::default_hw_backend`).
    pub fn start(hw: Option<&'static str>) -> Self {
        let sessions: Sessions = Default::default();
        let probe_generation = Arc::new(AtomicU64::new(0));
        let opening = Arc::new(AtomicU64::new(0));
        let cache = Arc::new(Mutex::new(ProbeCache::default()));
        let standby: Arc<Mutex<Option<Standby>>> = Default::default();
        let (response_tx, responses) = mpsc::channel();

        let (warmer, rx) = mpsc::channel::<(Request, std::time::Instant)>();
        {
            let standby = standby.clone();
            std::thread::Builder::new()
                .name("standby".into())
                .spawn(move || {
                    for (request, _) in rx {
                        if let Request::Warm { server, share } = request {
                            warm(&standby, &server, &share);
                        }
                    }
                })
                .expect("spawn standby worker");
        }

        let (main, rx) = mpsc::channel::<(Request, std::time::Instant)>();
        {
            let (sessions, out, cache) = (sessions.clone(), response_tx.clone(), cache.clone());
            let (opening, standby) = (opening.clone(), standby.clone());
            std::thread::Builder::new()
                .name("library".into())
                .spawn(move || {
                    for (request, requested) in rx {
                        let open = matches!(request, Request::Open { .. });
                        let response = handle(request, requested, &sessions, &cache, &standby, hw);
                        if open {
                            opening.fetch_sub(1, Ordering::SeqCst);
                        }
                        if out.send(response).is_err() {
                            return;
                        }
                    }
                })
                .expect("spawn library worker");
        }

        let (jobs_tx, jobs_rx) = mpsc::channel::<ProbeJob>();
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        // Probes queued and not finished; the cache is saved when it reaches 0.
        let queued = Arc::new(AtomicU64::new(0));
        let (probes, rx) = mpsc::channel::<(Request, std::time::Instant)>();
        {
            let (out, cache, queued) = (response_tx.clone(), cache.clone(), queued.clone());
            let jobs = jobs_tx.clone();
            std::thread::Builder::new()
                .name("probe".into())
                .spawn(move || {
                    // Off the frame loop: the cache file can be a few MB.
                    *cache.lock().expect("probe cache") = ProbeCache::load();
                    for (request, _) in rx {
                        dispatch_probes(request, &cache, &jobs, &queued, &out);
                    }
                })
                .expect("spawn probe dispatcher");
        }
        // JUST_VIDEO_PROBE_WORKERS: for comparing, e.g. 1 against the default.
        let workers = std::env::var("JUST_VIDEO_PROBE_WORKERS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(PROBE_WORKERS)
            .max(1);
        for n in 0..workers {
            let (sessions, out, cache) = (sessions.clone(), response_tx.clone(), cache.clone());
            let (jobs, requeue, queued) = (jobs_rx.clone(), jobs_tx.clone(), queued.clone());
            let (current, opening) = (probe_generation.clone(), opening.clone());
            std::thread::Builder::new()
                .name(format!("probe-{n}"))
                .spawn(move || {
                    loop {
                        let Ok(job) = jobs.lock().expect("probe jobs").recv() else {
                            return;
                        };
                        if wait_for_quiet(&current, job.generation, &opening) {
                            let Some(result) = probe(&job, &sessions, &opening) else {
                                // Paused: again once the video is closed.
                                let _ = requeue.send(job);
                                continue;
                            };
                            let name = job.path.last().cloned().unwrap_or_default();
                            let key = file_key(&job.server, &job.share, &job.path);
                            if let Ok(video) = &result {
                                cache.lock().expect("probe cache").insert(
                                    key.clone(),
                                    job.size,
                                    job.modified,
                                    video.clone(),
                                );
                            }
                            let result = result.map(|video| {
                                let saved = config::layout_override(&key).ok().flatten();
                                probed(&name, video.as_ref(), saved.as_ref())
                            });
                            let response = Response::Probe {
                                generation: job.generation,
                                name,
                                result,
                                cached: false,
                            };
                            if out.send(response).is_err() {
                                return;
                            }
                        }
                        if queued.fetch_sub(1, Ordering::SeqCst) == 1 {
                            ProbeCache::save(&cache);
                        }
                    }
                })
                .expect("spawn probe worker");
        }
        let thumbnail_generation = Arc::new(AtomicU64::new(0));
        let (thumbnails, rx) = mpsc::channel::<Request>();
        {
            let (sessions, out, queued) = (sessions.clone(), response_tx.clone(), queued.clone());
            let (opening, current) = (opening.clone(), thumbnail_generation.clone());
            std::thread::Builder::new()
                .name("thumbnail".into())
                .spawn(move || thumbnail_worker(rx, sessions, out, queued, opening, current))
                .expect("spawn thumbnail worker");
        }
        Self {
            main,
            probes,
            responses,
            probe_generation,
            opening,
            warmer,
            thumbnails,
            thumbnail_generation,
        }
    }

    /// Only probes tagged with this generation will run from now on.
    pub fn set_probe_generation(&self, generation: u64) {
        self.probe_generation.store(generation, Ordering::Relaxed);
    }

    /// Only thumbnails tagged with this generation will be made from now on
    /// (set it before sending requests for it). Bump it when the folder
    /// changes and when thumbnails are switched off: pending work stops at
    /// once, and what the worker has queued is dropped.
    pub fn set_thumbnail_generation(&self, generation: u64) {
        self.thumbnail_generation
            .store(generation, Ordering::Relaxed);
    }

    pub fn send(&self, request: Request) {
        if matches!(
            request,
            Request::ThumbnailFolder { .. } | Request::ThumbnailFocus { .. }
        ) {
            let _ = self.thumbnails.send(request);
            return;
        }
        let worker = match request {
            Request::ProbeFolder { .. } => &self.probes,
            Request::Warm { .. } => &self.warmer,
            Request::Open { .. } => {
                // Probes pause from now, not only once the worker gets to it.
                self.opening.fetch_add(1, Ordering::SeqCst);
                &self.main
            }
            _ => &self.main,
        };
        let _ = worker.send((request, std::time::Instant::now()));
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.responses.try_recv().ok()
    }

    /// A library without workers: requests go nowhere, and the test sends
    /// the responses.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, mpsc::Sender<Response>) {
        let (tx, responses) = mpsc::channel();
        let library = Self {
            main: mpsc::channel().0,
            probes: mpsc::channel().0,
            warmer: mpsc::channel().0,
            responses,
            probe_generation: Default::default(),
            opening: Default::default(),
            thumbnails: mpsc::channel().0,
            thumbnail_generation: Default::default(),
        };
        (library, tx)
    }
}

impl LayoutOverride {
    pub fn apply(&self, layout: &mut Layout) {
        layout.projection = self.projection;
        layout.stereo = self.stereo;
        layout.swap_eyes = self.swap_eyes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_videos_are_marked_at_once_and_the_rest_queued() {
        crate::config::temp_config("library-probe-dispatch");
        let server = Server {
            name: "nas".into(),
            url: "smb://me@nas".into(),
            writable: false,
        };
        let folder: Path = vec!["vr".into()];
        let video = |name: &str, size| ProbeVideo {
            name: name.into(),
            size,
            modified: 9,
        };
        let cache = Mutex::new(ProbeCache::default());
        let key = file_key(&server, "media", &vec!["vr".into(), "a_180_sbs.mp4".into()]);
        cache.lock().unwrap().insert(
            key,
            100,
            9,
            Some(VideoInfo {
                codec: "hevc".into(),
                profile: Some("Main".into()),
                pixel_format: Some("yuv420p".into()),
                width: 6144,
                height: 3072,
                bit_depth: 8,
                fps: 60.0,
                stereo_mode: None,
                stereo_inverted: false,
                projection: None,
                horizontal_degrees: None,
            }),
        );
        let (jobs_tx, jobs) = mpsc::channel();
        let (out, responses) = mpsc::channel();
        let queued = AtomicU64::new(0);
        let request = Request::ProbeFolder {
            generation: 3,
            server,
            share: "media".into(),
            folder,
            // The second one changed size since it was cached.
            videos: vec![video("a_180_sbs.mp4", 100), video("b.mp4", 5)],
        };
        dispatch_probes(request, &cache, &jobs_tx, &queued, &out);
        let Response::Probe {
            generation,
            name,
            result: Ok(probed),
            cached: true,
        } = responses.try_recv().unwrap()
        else {
            panic!("expected a cached mark");
        };
        assert_eq!((generation, name.as_str()), (3, "a_180_sbs.mp4"));
        // Worked out from the cached facts: name tags and playability.
        assert_eq!(probed.layout.projection, vr::Projection::Equirect180);
        assert_ne!(probed.assessment.verdict, playability::Verdict::Unplayable);
        assert!(responses.try_recv().is_err());
        let job = jobs.try_recv().unwrap();
        assert_eq!(job.path, vec!["vr".to_string(), "b.mp4".to_string()]);
        assert_eq!(queued.load(Ordering::SeqCst), 1);
    }

    fn thumb_video(index: usize, name: &str) -> ThumbVideo {
        ThumbVideo {
            index,
            name: name.into(),
            size: 10,
            modified: 5,
            layout: vr::detect(name, None),
        }
    }

    fn thumb_folder(generation: u64, videos: Vec<ThumbVideo>, first_visible: usize) -> Request {
        Request::ThumbnailFolder {
            generation,
            server: Server {
                name: "nas".into(),
                url: "smb://me@nas".into(),
                writable: false,
            },
            share: "media".into(),
            folder: vec!["vr".into()],
            videos,
            first_visible,
        }
    }

    /// Takes everything off the queue in the order it would be made.
    fn thumb_order(queue: &mut ThumbQueue) -> Vec<String> {
        let mut names = Vec::new();
        while let Some(position) = queue.soonest(false) {
            names.push(queue.finish(position).name);
        }
        names
    }

    #[test]
    fn thumbnails_start_at_the_view_and_follow_scrolling() {
        let videos: Vec<_> = (0..7)
            .map(|i| thumb_video(i, &format!("{i}.mp4")))
            .collect();
        let mut queue = ThumbQueue::default();
        queue.apply(thumb_folder(1, videos.clone(), 3), 1);
        let mut first = ThumbQueue::default();
        first.apply(thumb_folder(1, videos, 3), 1);
        // Outward from row 3, a row below a little ahead of the one above.
        assert_eq!(
            thumb_order(&mut first),
            [
                "3.mp4", "4.mp4", "2.mp4", "5.mp4", "1.mp4", "6.mp4", "0.mp4"
            ]
        );
        queue.apply(
            Request::ThumbnailFocus {
                generation: 1,
                first_visible: 6,
            },
            1,
        );
        assert_eq!(thumb_order(&mut queue)[..3], ["6.mp4", "5.mp4", "4.mp4"]);
    }

    #[test]
    fn thumbnail_requests_of_other_generations_are_ignored() {
        let mut queue = ThumbQueue::default();
        queue.apply(thumb_folder(1, vec![thumb_video(0, "a.mp4")], 0), 2);
        assert!(queue.pending.is_empty());
        queue.apply(thumb_folder(2, vec![thumb_video(0, "a.mp4")], 0), 2);
        assert_eq!(queue.pending.len(), 1);
        // A focus for an old generation changes nothing.
        queue.apply(
            Request::ThumbnailFocus {
                generation: 1,
                first_visible: 9,
            },
            2,
        );
        assert_eq!(queue.first_visible, 0);
        // A new generation starts over.
        queue.apply(thumb_folder(3, vec![thumb_video(0, "b.mp4")], 0), 3);
        assert_eq!(thumb_order(&mut queue), ["b.mp4"]);
    }

    #[test]
    fn sending_a_folder_again_merges_by_name() {
        let mut queue = ThumbQueue::default();
        queue.apply(
            thumb_folder(1, vec![thumb_video(0, "a.mp4"), thumb_video(1, "b.mp4")], 0),
            1,
        );
        let first = queue.soonest(false).unwrap();
        let done = queue.finish(first);
        assert_eq!(done.name, "a.mp4");
        queue.pending[0].checked = true;
        // Again: a.mp4 is made already, b.mp4 is not duplicated, c.mp4 is new.
        let mut b = thumb_video(1, "b.mp4");
        queue.apply(
            thumb_folder(
                1,
                vec![thumb_video(0, "a.mp4"), b.clone(), thumb_video(2, "c.mp4")],
                0,
            ),
            1,
        );
        assert_eq!(queue.pending.len(), 2);
        assert!(queue.pending[0].checked);
        // A changed layout is a new picture: looked up again.
        b.layout.stereo = vr::Stereo::SideBySide;
        queue.apply(thumb_folder(1, vec![b], 0), 1);
        assert!(!queue.pending[0].checked);
        let mut a = thumb_video(0, "a.mp4");
        a.layout.swap_eyes = true;
        queue.apply(thumb_folder(1, vec![a], 0), 1);
        assert_eq!(queue.pending.len(), 3);
    }

    #[test]
    fn closed_connections_are_replaced() {
        for lost in [
            "The server did not answer (list)",
            "Open share \\\\nas\\media: Client connection is stopped",
            "Transport error: Not connected",
            "IO Error: Connection reset by peer (os error 104)",
            "Server returned an error message with status: Network Session Expired.",
            "Server returned an error message with status: User Session Deleted.",
        ] {
            assert!(connection_lost(lost), "{lost}");
        }
        for kept in [
            "Server returned an error message with status: Object Name Not Found.",
            "Server returned an error message with status: Directory Not Empty.",
            "Server returned an error message with status: Access Denied.",
        ] {
            assert!(!connection_lost(kept), "{kept}");
        }
    }
}
