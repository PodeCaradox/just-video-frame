//! Background SMB work for the headset browser. Network calls never run on the
//! XR frame loop: requests go to worker threads and results come back on a
//! channel. Navigation and opening use one worker, playability probes another,
//! so a slow probe never delays browsing.

use crate::config::{self, LayoutOverride, Server};
use crate::media::{Media, VideoDecoder};
use crate::playability::{self, Assessment, Platform};
use crate::readahead::ReadAhead;
use crate::smb::{Entry, SmbSession, SmbUrl};
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
    Probe {
        generation: u64,
        server: Server,
        share: String,
        path: Path,
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
    /// Checks the login (and that shares can be listed), then saves the server.
    AddServer {
        id: u64,
        server: Server,
        password: String,
    },
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

/// Opens `path` on its own `session` for playback: subtitles next to it,
/// the file, its streams, then the decoder.
pub fn open_video(
    session: &Arc<SmbSession>,
    share: &str,
    path: &Path,
    key: String,
    hw: Option<&str>,
    timing: &mut OpenTiming,
) -> Result<Box<Opened>, String> {
    let err = |e: anyhow::Error| format!("{e:#}");
    let name = path.last().cloned().unwrap_or_default();
    let external_subtitles = load_sidecars(session, share, path);
    timing.lap("sidecars");
    let reader = session
        .open_owned(share, &smb_path(path), ReadAhead::default())
        .map_err(err)?;
    timing.lap("open file");
    let media = Media::open(&name, reader).map_err(err)?;
    timing.lap("probe");
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
    },
    Opened {
        id: u64,
        result: Result<Box<Opened>, String>,
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
}

/// What a connection is used for. Browsing and playability probes keep one
/// connection each per server; every playing video gets its own (see Open).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Purpose {
    Browse,
    Probe,
}

type Sessions = Arc<Mutex<HashMap<(String, Purpose), Arc<SmbSession>>>>;

/// Key of a file's saved layout override.
pub fn file_key(server: &Server, share: &str, path: &Path) -> String {
    format!("{}/{share}/{}", server.url, path.join("/"))
}

fn connect(server: &Server, password: Option<String>) -> Result<Arc<SmbSession>, String> {
    let url: SmbUrl = server.url.parse().map_err(|e| format!("{e:#}"))?;
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
    let key = (server.url.clone(), purpose);
    if let Some(s) = sessions.lock().expect("sessions").get(&key) {
        return Ok(s.clone());
    }
    let session = connect(server, None)?;
    sessions
        .lock()
        .expect("sessions")
        .insert(key, session.clone());
    Ok(session)
}

/// Forgets a server connection after a failure so the next request reconnects;
/// a broken connection must never wedge browsing until the app restarts.
fn evict(sessions: &Sessions, server: &Server, purpose: Purpose) {
    // Dropping a wedged session can block on its logoff; never here.
    if let Some(session) = sessions
        .lock()
        .expect("sessions")
        .remove(&(server.url.clone(), purpose))
    {
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
    hw: Option<&str>,
) -> Response {
    let started = std::time::Instant::now();
    let (server, what) = match &request {
        Request::Shares { server, .. } => (server.clone(), "shares"),
        Request::List { server, .. } => (server.clone(), "list"),
        Request::Probe { server, .. } => (server.clone(), "probe"),
        Request::Open { server, .. } => (server.clone(), "open"),
        Request::Rename { server, .. } => (server.clone(), "rename"),
        Request::Delete { server, .. } => (server.clone(), "delete"),
        Request::AddServer { server, .. } => (server.clone(), "add server"),
    };
    let response = run(request, requested, sessions, hw);
    let failure = match &response {
        Response::Shares { result: Err(e), .. }
        | Response::List { result: Err(e), .. }
        | Response::Opened { result: Err(e), .. }
        | Response::Changed { result: Err(e), .. }
        | Response::ServerAdded { result: Err(e), .. } => Some(e.clone()),
        // A bad file is not a connection problem; a stalled server is.
        Response::Probe { result: Err(e), .. }
            if e.contains("stopped sending") || e.contains("did not answer") =>
        {
            Some(e.clone())
        }
        _ => None,
    };
    if let Some(e) = failure {
        eprintln!(
            "Library: {what} failed after {:.1}s: {e}",
            started.elapsed().as_secs_f64()
        );
        // Only unanswered requests mean the connection itself is broken.
        if e.contains("did not answer") || e.contains("stopped sending") {
            match what {
                "probe" => evict(sessions, &server, Purpose::Probe),
                // A video has its own connection, which closes with it.
                "open" | "add server" => {}
                _ => evict(sessions, &server, Purpose::Browse),
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

fn run(
    request: Request,
    requested: std::time::Instant,
    sessions: &Sessions,
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
        Request::Probe {
            generation,
            server,
            share,
            path,
        } => {
            // Header probes read little: small blocks, shallow read-ahead.
            let probe = ReadAhead {
                block_size: 256 * 1024,
                blocks_ahead: 4,
            };
            let name = path.last().cloned().unwrap_or_default();
            let key = file_key(&server, &share, &path);
            let result = session(sessions, &server, Purpose::Probe).and_then(|s| {
                let reader = s.open_in(&share, &smb_path(&path), probe).map_err(err)?;
                let media = Media::open(&name, reader).map_err(err)?;
                let video = media.info().video.as_ref();
                // Marked as it will play: a format the user picked wins.
                let mut layout = vr::detect(&name, video);
                if let Some(saved) = config::layout_override(&key).ok().flatten() {
                    saved.apply(&mut layout);
                }
                Ok(Probed {
                    assessment: playability::assess(Platform::current(), video),
                    layout,
                })
            });
            Response::Probe {
                generation,
                name,
                result,
            }
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
            let result = connect(&server, None).and_then(|s| {
                timing.lap("connect");
                let key = file_key(&server, &share, &path);
                open_video(&s, &share, &path, key, hw, &mut timing)
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

impl Library {
    /// `hw` is the preferred hardware backend for playback (see `media::default_hw_backend`).
    pub fn start(hw: Option<&'static str>) -> Self {
        let sessions: Sessions = Default::default();
        let probe_generation = Arc::new(AtomicU64::new(0));
        let (response_tx, responses) = mpsc::channel();
        let mut senders = Vec::new();
        for name in ["library", "probe"] {
            let (tx, rx) = mpsc::channel::<(Request, std::time::Instant)>();
            let (sessions, out) = (sessions.clone(), response_tx.clone());
            let current = probe_generation.clone();
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    for (request, requested) in rx {
                        if let Request::Probe { generation, .. } = &request
                            && *generation != current.load(Ordering::Relaxed)
                        {
                            continue;
                        }
                        if out.send(handle(request, requested, &sessions, hw)).is_err() {
                            return;
                        }
                    }
                })
                .expect("spawn library worker");
            senders.push(tx);
        }
        let probes = senders.pop().expect("probe worker");
        let main = senders.pop().expect("main worker");
        Self {
            main,
            probes,
            responses,
            probe_generation,
        }
    }

    /// Only probes tagged with this generation will run from now on.
    pub fn set_probe_generation(&self, generation: u64) {
        self.probe_generation.store(generation, Ordering::Relaxed);
    }

    pub fn send(&self, request: Request) {
        let worker = if matches!(request, Request::Probe { .. }) {
            &self.probes
        } else {
            &self.main
        };
        let _ = worker.send((request, std::time::Instant::now()));
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.responses.try_recv().ok()
    }
}

impl LayoutOverride {
    pub fn apply(&self, layout: &mut Layout) {
        layout.projection = self.projection;
        layout.stereo = self.stereo;
        layout.swap_eyes = self.swap_eyes;
    }
}
