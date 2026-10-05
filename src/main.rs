use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use just_video::{
    media::{Media, Source},
    playability::{self, Assessment, Platform, Verdict},
    readahead::ReadAhead,
    smb::{SmbSession, SmbUrl},
    vr,
};
use serde_json::json;
use std::{io::Read, time::Instant};

/// Just Video: VR playback straight from SMB shares.
///
/// Inputs are `smb://[domain;]user@host[:port]/share/path` or local paths. The
/// SMB password comes from JUST_VIDEO_SMB_PASSWORD or an interactive prompt.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Judge playability for this device instead of the one we run on.
    #[arg(long, value_enum, global = true)]
    platform: Option<Platform>,
    /// Without a command, the headset app starts (browser, then playback).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Save a server for the headset browser. Asks for the password and checks it.
    AddServer {
        /// `smb://[domain;]user@host[:port]`
        url: String,
        /// Display name (default: the host).
        #[arg(long)]
        name: Option<String>,
    },
    /// List saved servers.
    Servers,
    /// Forget a saved server (by name or URL) and its password.
    RemoveServer { server: String },
    /// List a share directory.
    Ls {
        url: String,
        /// Probe each video file and show whether (and why not) it will play.
        #[arg(long)]
        check: bool,
    },
    /// Show container, codec and detected VR layout.
    Info { input: String },
    /// Play a video in the headset (OpenXR). Ctrl-C stops.
    Play {
        input: String,
        #[arg(long, value_enum, default_value_t = Hw::Auto)]
        hw: Hw,
        /// Override the detected projection.
        #[arg(long, value_enum)]
        projection: Option<ProjectionArg>,
        /// Override the detected stereo packing.
        #[arg(long, value_enum)]
        stereo: Option<StereoArg>,
        /// Swap left and right eye images.
        #[arg(long)]
        swap_eyes: bool,
        /// Fisheye lens field of view in degrees.
        #[arg(long, default_value_t = 180.0)]
        fisheye_fov: f32,
        /// Flat screen width in metres.
        #[arg(long, default_value_t = 3.2)]
        screen_width: f32,
        /// Flat screen distance in metres.
        #[arg(long, default_value_t = 3.0)]
        screen_distance: f32,
        /// Save the left eye as PNG at `--screenshot-at` seconds (for testing).
        #[arg(long)]
        screenshot: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = 3.0)]
        screenshot_at: f64,
        /// Stop after this many seconds.
        #[arg(long)]
        duration: Option<f64>,
        /// Start playback at this position (seconds).
        #[arg(long, default_value_t = 0.0)]
        start: f64,
        /// Play even if the playability check says it will not play smoothly.
        #[arg(long)]
        force: bool,
        /// Shader debug view: 1 = projection UV colours, 2 = raw YUV samples.
        #[arg(long, default_value_t = 0, hide = true)]
        debug_view: u32,
        #[command(flatten)]
        read_ahead: ReadAheadArgs,
    },
    /// Write the list-view thumbnail of a video as a PNG (software decode).
    Thumbnail {
        input: String,
        out: std::path::PathBuf,
        /// Override the detected projection.
        #[arg(long, value_enum)]
        projection: Option<ProjectionArg>,
        /// Override the detected stereo packing.
        #[arg(long, value_enum)]
        stereo: Option<StereoArg>,
        /// Swap left and right eye images.
        #[arg(long)]
        swap_eyes: bool,
        /// Position in the video, as a fraction of its duration.
        #[arg(long, default_value_t = 0.1)]
        at: f64,
        #[arg(long, default_value_t = 220)]
        width: u32,
        #[arg(long, default_value_t = 124)]
        height: u32,
    },
    /// Create an OpenXR session and report the headset, GPU and swapchain formats.
    XrProbe,
    /// Exercise the headset library workers without XR: open, play briefly,
    /// stop, repeat across a folder, listing in between (hang hunting).
    #[command(hide = true)]
    LibraryStress {
        /// Saved server name.
        server: String,
        share: String,
        /// Folder inside the share, `/`-separated.
        #[arg(default_value = "")]
        folder: String,
        #[arg(long, default_value_t = 20)]
        rounds: usize,
        /// Seeks per round while playing.
        #[arg(long, default_value_t = 0)]
        seeks: usize,
        /// Forget what earlier runs probed (the probe cache) first.
        #[arg(long)]
        cold: bool,
        /// Connect for playing ahead of time, as the browser does.
        #[arg(long)]
        warm: bool,
        /// Open the first video while the folder is still being probed.
        #[arg(long)]
        click_at_once: bool,
    },
    /// Print the subtitles decoded while playing through a stretch of video.
    #[command(hide = true)]
    Subtitles {
        input: String,
        /// Subtitle track (index into `info`'s subtitles).
        #[arg(long, default_value_t = 0)]
        track: usize,
        /// Start here (seconds).
        #[arg(long, default_value_t = 0.0)]
        start: f64,
        /// Media seconds to decode.
        #[arg(long, default_value_t = 60.0)]
        seconds: f64,
        /// Save the first picture subtitle as rendered in the headset.
        #[arg(long)]
        png: Option<std::path::PathBuf>,
    },
    /// Decode and play a file's audio for a few seconds (no XR), checking timestamps.
    #[command(hide = true)]
    AudioTest {
        input: String,
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        #[arg(long, default_value_t = 0.3)]
        volume: f32,
        /// Halfway through, switch to this audio track (as the player does).
        #[arg(long)]
        switch_to: Option<usize>,
    },
    /// Render sample browser screens to PNG files (UI development).
    #[command(hide = true)]
    UiPreview { dir: std::path::PathBuf },
    /// Measure raw sequential SMB read throughput through the read-ahead reader.
    ReadBench {
        url: String,
        /// Stop after this many MiB (default: whole file).
        #[arg(long)]
        mib: Option<u64>,
        #[command(flatten)]
        read_ahead: ReadAheadArgs,
    },
    /// Time opening a video and jumping around in it (+10 s, +10 min, random),
    /// through the player's own open and seek code, without the headset view.
    BenchSeek {
        /// `smb://user@host/share/path/video.mp4` or a local file.
        input: String,
        #[arg(long, value_enum, default_value_t = Hw::Auto)]
        hw: Hw,
        /// Open as if continuing from here (seconds) and only time that.
        #[arg(long)]
        resume: Option<f64>,
        /// Start the jumps here (seconds).
        #[arg(long, default_value_t = 120.0)]
        from: f64,
        /// Random jumps after the fixed ones.
        #[arg(long, default_value_t = 4)]
        random: usize,
        /// Local files only: simulate a network link of this speed (Mbit/s)...
        #[arg(long)]
        link_mbps: Option<f64>,
        /// ...and this round trip (ms).
        #[arg(long, default_value_t = 3.0)]
        rtt_ms: f64,
        /// Instead of jumping, play this many seconds from `--from` like the
        /// headset's frame loop and report dropped frames and copy times.
        #[arg(long)]
        play: Option<f64>,
        /// Frame loop rate for `--play`.
        #[arg(long, default_value_t = 90.0)]
        hz: f64,
        /// With `--play`: jump +5 s this often (seconds), like D-pad presses.
        #[arg(long)]
        jump_every: Option<f64>,
        /// ...by this much instead (seconds; negative jumps back).
        #[arg(long, default_value_t = 5.0, allow_negative_numbers = true)]
        jump_by: f64,
        /// Print the full report as JSON instead of a table.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        read_ahead: ReadAheadArgs,
    },
    /// Decode video frames from the input and report throughput vs. real time.
    Bench {
        input: String,
        #[arg(long, default_value_t = 600)]
        frames: u32,
        #[arg(long, value_enum, default_value_t = Hw::Auto)]
        hw: Hw,
        /// Fail instead of silently decoding in software.
        #[arg(long)]
        hw_only: bool,
        /// FFmpeg decoder option, repeatable: --decoder-opt threads=8 --decoder-opt thread_type=frame
        #[arg(long = "decoder-opt", value_name = "KEY=VALUE")]
        decoder_opts: Vec<String>,
        #[command(flatten)]
        read_ahead: ReadAheadArgs,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Hw {
    /// V4L2 (Qualcomm iris) on ARM64 / Steam Frame, Vulkan video elsewhere.
    Auto,
    Vulkan,
    /// V4L2 stateful decoder (Steam Frame's Qualcomm iris).
    V4l2,
    Vaapi,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProjectionArg {
    Flat,
    #[value(name = "180")]
    Vr180,
    #[value(name = "360")]
    Vr360,
    Fisheye,
}

#[derive(Clone, Copy, ValueEnum)]
enum StereoArg {
    Mono,
    Sbs,
    Tb,
}

fn override_layout(
    layout: &mut vr::Layout,
    projection: Option<ProjectionArg>,
    stereo: Option<StereoArg>,
    swap_eyes: bool,
) {
    if let Some(p) = projection {
        layout.projection = match p {
            ProjectionArg::Flat => vr::Projection::Flat,
            ProjectionArg::Vr180 => vr::Projection::Equirect180,
            ProjectionArg::Vr360 => vr::Projection::Equirect360,
            ProjectionArg::Fisheye => vr::Projection::Fisheye180,
        };
    }
    if let Some(s) = stereo {
        layout.stereo = match s {
            StereoArg::Mono => vr::Stereo::Mono,
            StereoArg::Sbs => vr::Stereo::SideBySide,
            StereoArg::Tb => vr::Stereo::TopBottom,
        };
    }
    layout.swap_eyes ^= swap_eyes;
}

fn hw_backend(hw: Hw) -> Option<&'static str> {
    match hw {
        Hw::Auto if cfg!(target_arch = "aarch64") => Some("v4l2m2m"),
        Hw::Auto | Hw::Vulkan => Some("vulkan"),
        Hw::V4l2 => Some("v4l2m2m"),
        Hw::Vaapi => Some("vaapi"),
        Hw::None => None,
    }
}

#[derive(clap::Args)]
struct ReadAheadArgs {
    /// SMB read size in KiB.
    #[arg(long, default_value_t = 1024)]
    block_kib: usize,
    /// Reads kept in flight ahead of the demuxer.
    #[arg(long, default_value_t = 32)]
    blocks_ahead: usize,
    /// SMB connections the file is read over (as when playing).
    #[arg(long, default_value_t = just_video::library::VIDEO_CONNECTIONS)]
    connections: usize,
}

/// `read_ahead.connections` sessions to the server in `url` (at least one).
fn connect_lanes(
    url: &str,
    read_ahead: &ReadAheadArgs,
) -> anyhow::Result<Vec<std::sync::Arc<SmbSession>>> {
    (0..read_ahead.connections.max(1))
        .map(|_| connect(url).map(std::sync::Arc::new))
        .collect()
}

impl ReadAheadArgs {
    fn get(&self) -> ReadAhead {
        ReadAhead {
            block_size: self.block_kib.clamp(64, 8192) * 1024,
            blocks_ahead: self.blocks_ahead.clamp(1, 256),
        }
    }
}

fn password() -> anyhow::Result<String> {
    if let Ok(password) = std::env::var("JUST_VIDEO_SMB_PASSWORD") {
        return Ok(password);
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Ok(rpassword::prompt_password("SMB password: ")?);
    }
    Ok(String::new())
}

fn connect(url: &str) -> anyhow::Result<SmbSession> {
    let url: SmbUrl = url.parse()?;
    // A saved server's password is used unless one is given explicitly.
    let password = match std::env::var("JUST_VIDEO_SMB_PASSWORD") {
        Ok(p) => p,
        Err(_) => match just_video::config::password(&url.server_url())? {
            Some(p) => p,
            None => password()?,
        },
    };
    SmbSession::connect(url, password)
}

/// Opens an SMB URL or local path; returns the source and a name for detection.
fn open_input(
    input: &str,
    read_ahead: ReadAhead,
) -> anyhow::Result<(Box<dyn Source>, Option<SmbSession>)> {
    if input.starts_with("smb://") {
        let session = connect(input)?;
        let path = session.url().path.clone();
        let reader = session.open(&path, read_ahead)?;
        Ok((Box::new(reader), Some(session)))
    } else {
        let file = std::fs::File::open(input).with_context(|| format!("Open {input}"))?;
        Ok((
            Box::new(std::io::BufReader::with_capacity(1 << 20, file)),
            None,
        ))
    }
}

fn file_name(input: &str) -> &str {
    input.rsplit(['/', '\\']).next().unwrap_or(input)
}

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mkv", "mov", "webm", "avi", "ts", "m2ts"];

fn is_video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn badge(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Hardware => "✅",
        Verdict::Software => "🟡",
        Verdict::SoftwareMarginal => "⚠️",
        Verdict::Unplayable => "⛔",
    }
}

fn print_assessment(name: &str, a: &Assessment) {
    println!("{} {name} — {}", badge(a.verdict), a.title);
    for line in [&a.detail, &a.hint].into_iter().flatten() {
        println!("     {line}");
    }
}

fn quit_on_ctrl_c() -> anyhow::Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = quit.clone();
    ctrlc::set_handler(move || flag.store(true, std::sync::atomic::Ordering::Relaxed))?;
    Ok(quit)
}

/// When launched from Steam there is no terminal: send diagnostics to
/// ~/.local/state/just-video/log.txt instead.
fn log_to_file_without_terminal() {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home).join(".local/state/just-video");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("log.txt");
    // Keep one previous log for comparison.
    let _ = std::fs::rename(&path, dir.join("log.previous.txt"));
    if let Ok(file) = std::fs::File::create(&path) {
        use std::os::fd::AsRawFd;
        // SAFETY: dup2 onto stderr with a valid, open descriptor.
        unsafe { libc::dup2(file.as_raw_fd(), 2) };
    }
}

fn run_app(quit: std::sync::Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<()> {
    log_to_file_without_terminal();
    eprintln!(
        "Just Video {} ({}) starting",
        env!("CARGO_PKG_VERSION"),
        just_video::ui::browser::BUILD
    );
    let library = just_video::library::Library::start(just_video::media::default_hw_backend());
    let mut navigator = just_video::ui::navigator::Navigator::new(library);
    navigator.set_preferences(just_video::config::preferences());
    just_video::xr::app::run(
        Some(navigator),
        None,
        just_video::xr::app::AppOptions {
            view: Default::default(),
            play: Default::default(),
            quit,
        },
    )?;
    eprintln!("Just Video stopped");
    Ok(())
}

fn print(value: serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let platform = cli.platform.unwrap_or_else(Platform::current);
    let Some(command) = cli.command else {
        let result = run_app(quit_on_ctrl_c()?);
        if let Err(e) = &result {
            eprintln!("Error: {e:#}");
        }
        return result;
    };
    match command {
        Command::AddServer { url, name } => {
            let parsed: SmbUrl = url.parse()?;
            anyhow::ensure!(
                parsed.share.is_empty(),
                "Give the server only, e.g. smb://user@host (shares are browsed in the headset)"
            );
            let password = password()?;
            let session = SmbSession::connect(parsed.clone(), password.clone())?;
            let shares = session.shares()?;
            just_video::config::save_server(
                just_video::config::Server {
                    name: name.unwrap_or_else(|| parsed.host.clone()),
                    url: parsed.server_url(),
                    // Saving again keeps whether files may be changed.
                    writable: just_video::config::servers()
                        .unwrap_or_default()
                        .iter()
                        .any(|s| s.url == parsed.server_url() && s.writable),
                },
                &password,
            )?;
            println!(
                "Saved {} ({} shares: {})",
                parsed.server_url(),
                shares.len(),
                shares.join(", ")
            );
        }
        Command::Servers => {
            for server in just_video::config::servers()? {
                println!("{}\t{}", server.name, server.url);
            }
        }
        Command::RemoveServer { server } => {
            if !just_video::config::remove_server(&server)? {
                anyhow::bail!("No saved server named {server}");
            }
        }
        Command::Ls { url, check } => {
            let session = connect(&url)?;
            if session.url().share.is_empty() {
                for share in session.shares()? {
                    println!("{share}/");
                }
                return Ok(());
            }
            let path = session.url().path.clone();
            // Header probes read little: small blocks, shallow read-ahead.
            let probe = ReadAhead {
                block_size: 256 * 1024,
                blocks_ahead: 4,
            };
            for entry in session.list(&path)? {
                if entry.is_dir {
                    println!("{}/", entry.name);
                } else if check && is_video(&entry.name) {
                    let file = if path.is_empty() {
                        entry.name.clone()
                    } else {
                        format!("{path}\\{}", entry.name)
                    };
                    // A broken or unreadable file is reported, never fatal for the listing.
                    let assessment = session
                        .open(&file, probe)
                        .and_then(|reader| Media::open(&entry.name, reader))
                        .map(|media| playability::assess(platform, media.info().video.as_ref()));
                    match assessment {
                        Ok(a) => print_assessment(&entry.name, &a),
                        Err(e) => println!(
                            "⛔ {} — Can't read this file\n     It may be damaged or not a video ({e:#}).",
                            entry.name
                        ),
                    }
                } else {
                    println!("{}\t{}", entry.name, entry.size);
                }
            }
        }
        Command::Info { input } => {
            let (source, _session) = open_input(&input, ReadAhead::default())?;
            let media = Media::open(file_name(&input), source)?;
            let video = media.info().video.as_ref();
            print(json!({
                "media": media.info(),
                "layout": vr::detect(&input, video),
                "playability": playability::assess(platform, video),
                // What probing read: bytes, and jumps (each a round trip).
                "read": media.io(),
            }))?;
        }
        Command::Play {
            input,
            hw,
            projection,
            stereo,
            swap_eyes,
            fisheye_fov,
            screen_width,
            screen_distance,
            screenshot,
            screenshot_at,
            duration,
            start,
            force,
            debug_view,
            read_ahead,
        } => {
            let (source, _session) = open_input(&input, read_ahead.get())?;
            let media = Media::open(file_name(&input), source)?;
            let video = media.info().video.clone();
            let assessment = playability::assess(platform, video.as_ref());
            print_assessment(file_name(&input), &assessment);
            if assessment.verdict == Verdict::Unplayable && !force {
                anyhow::bail!("Not playing: {}", assessment.title);
            }
            let mut layout = vr::detect(&input, video.as_ref());
            override_layout(&mut layout, projection, stereo, swap_eyes);
            eprintln!(
                "Layout: {:?} / {:?}{}",
                layout.projection,
                layout.stereo,
                if layout.swap_eyes {
                    " (eyes swapped)"
                } else {
                    ""
                }
            );
            let decoder = media.into_decoder(hw_backend(hw), true, "")?;
            if let Some(note) = &decoder.stats().note {
                eprintln!("Decoder: {} ({note})", decoder.stats().decoder);
            } else {
                eprintln!("Decoder: {}", decoder.stats().decoder);
            }
            let playback = just_video::xr::player::Playback::start(decoder, layout, start, 0.7);
            let stats = just_video::xr::app::run(
                None,
                Some(playback),
                just_video::xr::app::AppOptions {
                    view: just_video::xr::player::ViewOptions {
                        fisheye_fov,
                        screen_width,
                        screen_distance,
                        debug_view,
                    },
                    play: just_video::xr::player::PlayOptions {
                        screenshot: screenshot.map(|p| (p, start + screenshot_at)),
                        duration: duration.map(|d| start + d),
                        start,
                    },
                    quit: quit_on_ctrl_c()?,
                },
            )?;
            print(json!(stats))?;
        }
        Command::LibraryStress {
            server,
            share,
            folder,
            rounds,
            seeks,
            cold,
            warm,
            click_at_once,
        } => {
            use just_video::library::{Library, Request, Response};
            use std::time::{Duration, Instant};
            let index = just_video::config::servers()?
                .into_iter()
                .find(|s| s.name == server)
                .ok_or_else(|| anyhow::anyhow!("No saved server {server}"))?;
            if cold {
                let _ = std::fs::remove_file(just_video::config::dir()?.join("probe-cache.json"));
            }
            let library = Library::start(just_video::media::default_hw_backend());
            let path: Vec<String> = folder
                .split('/')
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect();
            let wait = |what: &str| -> anyhow::Result<Response> {
                let started = Instant::now();
                loop {
                    if let Some(r) = library.try_recv() {
                        eprintln!("  {what}: {:.2}s", started.elapsed().as_secs_f64());
                        return Ok(r);
                    }
                    if started.elapsed() > Duration::from_secs(30) {
                        eprintln!(
                            "HANG: {what} took over 30 s (pid {}); waiting for a debugger",
                            std::process::id()
                        );
                        std::thread::sleep(Duration::from_secs(600));
                        anyhow::bail!("HANG: {what}");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            library.send(Request::List {
                id: 1,
                server: index.clone(),
                share: share.clone(),
                path: path.clone(),
            });
            let Response::List { result, .. } = wait("list")? else {
                anyhow::bail!("unexpected")
            };
            let listed: Vec<just_video::library::ProbeVideo> = result
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .filter(|e| !e.is_dir && just_video::ui::navigator::is_video(&e.name))
                .map(|e| just_video::library::ProbeVideo {
                    name: e.name,
                    size: e.size,
                    modified: e.modified,
                })
                .collect();
            let videos: Vec<String> = listed.iter().map(|v| v.name.clone()).collect();
            anyhow::ensure!(!videos.is_empty(), "No videos in that folder");
            for round in 0..rounds {
                let name = &videos[round % videos.len()];
                eprintln!("round {round}: {name}");
                library.set_probe_generation(round as u64);
                if warm {
                    library.send(Request::Warm {
                        server: index.clone(),
                        share: share.clone(),
                    });
                    // Unanswered; connecting takes ~25-115 ms.
                    std::thread::sleep(Duration::from_secs(1));
                }
                library.send(Request::ProbeFolder {
                    generation: round as u64,
                    server: index.clone(),
                    share: share.clone(),
                    folder: path.clone(),
                    videos: listed.clone(),
                });
                if round == 0 && !click_at_once {
                    // Marking the folder alone, as when it is first shown.
                    let started = Instant::now();
                    let (mut marked, mut cached) = (0, 0);
                    while marked < videos.len() {
                        if let Response::Probe { cached: c, .. } = wait("probe")? {
                            marked += 1;
                            cached += c as usize;
                        }
                    }
                    eprintln!(
                        "Marked {marked} videos in {:.0} ms ({cached} from cache)",
                        started.elapsed().as_secs_f64() * 1e3
                    );
                }
                let mut file = path.clone();
                file.push(name.clone());
                library.send(Request::Open {
                    id: 100 + round as u64,
                    server: index.clone(),
                    share: share.clone(),
                    path: file,
                });
                let opened = loop {
                    match wait("response")? {
                        Response::Opened { result, .. } => {
                            break result.map_err(anyhow::Error::msg)?;
                        }
                        _ => continue,
                    }
                };
                let mut playback = just_video::xr::player::Playback::start(
                    opened.decoder,
                    opened.layout,
                    0.0,
                    0.05,
                );
                let started = Instant::now();
                let mut shown = 0;
                let play_for = Duration::from_millis(4000 + 1500 * seeks as u64);
                let mut next_seek = 1;
                let mut last_progress = Instant::now();
                while started.elapsed() < play_for {
                    let now = started.elapsed().as_nanos() as i64 + 1_000_000_000;
                    if playback.advance(now) {
                        shown += 1;
                        last_progress = Instant::now();
                    }
                    if next_seek <= seeks
                        && started.elapsed() > Duration::from_millis(1500 * next_seek as u64)
                    {
                        let target = playback.duration * (next_seek as f64 * 0.37).fract();
                        eprintln!("  seek {next_seek} -> {target:.0}s");
                        playback.seek(target);
                        next_seek += 1;
                    }
                    if last_progress.elapsed() > Duration::from_secs(10) {
                        eprintln!(
                            "HANG: playback made no progress for 10 s (pid {}); waiting for a debugger",
                            std::process::id()
                        );
                        std::thread::sleep(Duration::from_secs(600));
                        anyhow::bail!("HANG: playback stalled");
                    }
                    std::thread::sleep(Duration::from_millis(11));
                }
                eprintln!("  played {shown} frames, stopping");
                just_video::xr::app::drop_in_background(playback);
                if round == 0 && click_at_once {
                    // Probes paused for the video carry on once it closes.
                    let started = Instant::now();
                    let mut marked = 0;
                    while marked < videos.len() && started.elapsed() < Duration::from_secs(30) {
                        match library.try_recv() {
                            Some(Response::Probe { generation: 0, .. }) => marked += 1,
                            Some(_) => {}
                            None => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                    eprintln!(
                        "Marked {marked} of {} videos after playback, in {:.0} ms",
                        videos.len(),
                        started.elapsed().as_secs_f64() * 1e3
                    );
                }
                library.send(Request::List {
                    id: 2,
                    server: index.clone(),
                    share: share.clone(),
                    path: path.clone(),
                });
                loop {
                    if let Response::List { .. } = wait("list after stop")? {
                        break;
                    }
                }
                library.send(Request::Shares {
                    id: 3,
                    server: index.clone(),
                });
                loop {
                    if let Response::Shares { .. } = wait("shares after stop")? {
                        break;
                    }
                }
            }
            eprintln!("no hang in {rounds} rounds");
        }
        Command::Subtitles {
            input,
            track,
            start,
            seconds,
            png,
        } => {
            let (source, _session) = open_input(&input, ReadAhead::default())?;
            let media = Media::open(file_name(&input), source)?;
            let mut decoder = media.into_decoder(None, true, "")?;
            anyhow::ensure!(
                decoder.select_subtitle(Some(track)),
                "Can't decode subtitle track {track}"
            );
            if start > 0.0 {
                decoder.seek(start)?;
            }
            let mut cues = Vec::new();
            while let Some(frame) = decoder.next_frame()? {
                cues.extend(decoder.take_subtitles());
                if frame.pts().is_some_and(|t| t > start + seconds) {
                    break;
                }
            }
            for c in &cues {
                let what = match &c.image {
                    Some(b) => format!(
                        "[picture {}x{} at {},{} in {}x{}]",
                        b.width, b.height, b.x, b.y, b.frame_width, b.frame_height
                    ),
                    None if c.text.is_empty() => "[erase]".to_string(),
                    None => c.text.replace('\n', " / "),
                };
                println!("{:8.2} {:8.2}  {what}", c.start, c.end);
            }
            if let (Some(path), Some(image)) = (png, cues.iter().find_map(|c| c.image.clone())) {
                let mut fonts = just_video::ui::canvas::Fonts::load()?;
                let caption = just_video::subtitles::Caption {
                    text: None,
                    image: Some(image),
                };
                just_video::ui::save_png(
                    &just_video::ui::captions::render(&caption, &mut fonts),
                    &path,
                )?;
            }
            eprintln!("{} cues", cues.len());
        }
        Command::AudioTest {
            input,
            seconds,
            volume,
            switch_to,
        } => {
            use just_video::audio::{CHANNELS, Output, RATE};
            let (source, _session) = open_input(&input, ReadAhead::default())?;
            let media = Media::open(file_name(&input), source)?;
            let mut decoder =
                media.into_decoder(just_video::media::default_hw_backend(), true, "")?;
            anyhow::ensure!(decoder.enable_audio(RATE, CHANNELS), "No audio track");
            let mut output = Output::open("audio-test")?;
            let (mut frames, mut written, mut expected, mut gaps) = (0u64, 0u64, None::<f64>, 0);
            // Zero crossings per second of the left channel, before and after a switch
            // (a pure tone reads as twice its frequency).
            let (mut crossings, mut counted, mut last) = ([0u64; 2], [0u64; 2], 0.0f32);
            let mut switched = false;
            while (written as f64) < seconds * RATE as f64 {
                if let Some(track) = switch_to
                    && !switched
                    && written as f64 >= seconds * RATE as f64 / 2.0
                {
                    switched = true;
                    let at = expected.unwrap_or(0.0);
                    anyhow::ensure!(
                        decoder.select_audio(track),
                        "Can't switch to audio track {track}"
                    );
                    decoder.seek(at)?;
                    expected = None;
                    eprintln!("switched to track {track} at {at:.2}s");
                }
                if decoder.next_frame()?.is_none() {
                    break;
                }
                frames += 1;
                while let Some((samples, pts)) = decoder.take_audio(CHANNELS) {
                    let n = samples.len() as u64 / CHANNELS as u64;
                    if let (Some(e), Some(p)) = (expected, pts)
                        && (p - e).abs() > 0.005
                    {
                        gaps += 1;
                        eprintln!("timestamp jump: expected {e:.3}, got {p:.3}");
                    }
                    expected = pts.map(|p| p + n as f64 / RATE as f64);
                    let half = switched as usize;
                    for frame in samples.chunks_exact(CHANNELS as usize) {
                        if (frame[0] >= 0.0) != (last >= 0.0) {
                            crossings[half] += 1;
                        }
                        last = frame[0];
                        counted[half] += 1;
                    }
                    let scaled: Vec<f32> = samples.iter().map(|s| s * volume * volume).collect();
                    output.write(&scaled)?;
                    written += n;
                }
            }
            print(json!({
                "video_frames": frames,
                "audio_seconds": written as f64 / RATE as f64,
                "last_audio_pts": expected,
                "timestamp_jumps": gaps,
                "crossings_per_second": ([0, 1].map(|h| crossings[h] as f64 * RATE as f64 / counted[h].max(1) as f64)),
                "output_latency_ms": output.latency() * 1000.0,
            }))?;
        }
        Command::UiPreview { dir } => {
            use just_video::ui::browser::{
                Action, Dialog, Icon, Row, Tool, ToolIcon, View, render,
            };
            use just_video::ui::form::{Field, Form};
            let mut fonts = just_video::ui::canvas::Fonts::load()?;
            std::fs::create_dir_all(&dir)?;
            let row = |icon, label: &str, detail: &str, right: &str| Row {
                detail: detail.into(),
                right: right.into(),
                checked: Some(label.starts_with('h')),
                ..Row::new(icon, label)
            };
            let crumbs = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let folder = View {
                crumbs: crumbs(&["Just Video", "NAS", "media", "Videos", "VR"]),
                tools: vec![Tool::text("Cancel", false), Tool::text("Delete 2", true)],
                rows: vec![
                    Row {
                        outlined: true,
                        ..row(Icon::Folder, "Concerts", "", "")
                    },
                    row(
                        Icon::Video(Some(Verdict::Software)),
                        "Documentary.2160p.HDR.mkv",
                        "Plays with CPU decoding (4K 60 fps 10-bit HEVC)",
                        "28.2 GB",
                    ),
                    row(
                        Icon::Video(Some(Verdict::Hardware)),
                        "h264.mp4",
                        "Plays with hardware decoding (1080p 24 fps 8-bit H.264)",
                        "1.8 GB",
                    ),
                    row(Icon::Video(None), "h265.mkv", "Checking…", "88.4 MB"),
                    row(
                        Icon::Video(Some(Verdict::Unplayable)),
                        "concert_8k.mp4",
                        "Can't play smoothly on Steam Frame (8K 60 fps 10-bit HEVC)",
                        "5.1 GB",
                    ),
                    row(Icon::Broken, "damaged.mp4", "Can't read this file", "12 KB"),
                    Row {
                        dimmed: true,
                        checked: None,
                        ..row(Icon::File, "notes.txt", "", "2 KB")
                    },
                    row(
                        Icon::VideoVr(Some(Verdict::Hardware)),
                        "旅行_180_LR.mp4",
                        "VR180 3D  ·  Plays with hardware decoding (5.7K 30 fps 8-bit HEVC)",
                        "3.3 GB",
                    ),
                ],
                ..Default::default()
            };
            just_video::ui::save_png(
                &render(&folder, &mut fonts, Some((700.0, 400.0)), true),
                &dir.join("folder.png"),
            )?;
            // Flat, 3D and VR videos at each verdict, marked as the browser marks them.
            let video = |name: &str, verdict: Option<Verdict>, title: &str, size: &str| {
                let layout = just_video::vr::detect(name, None);
                let detail = match (verdict, layout.short_label()) {
                    (None, _) => "Checking…".to_string(),
                    (Some(_), Some(format)) => format!("{format}  ·  {title}"),
                    (Some(_), None) => title.to_string(),
                };
                // The layout arrives with the probe, like the verdict.
                let icon = Icon::video(verdict, verdict.map(|_| &layout));
                Row {
                    detail,
                    right: size.into(),
                    ..Row::new(icon, name)
                }
            };
            let videos = View {
                crumbs: crumbs(&["Just Video", "NAS", "media", "Mixed"]),
                rows: vec![
                    video(
                        "Movie.2024.2160p.mkv",
                        Some(Verdict::Hardware),
                        "Plays with hardware decoding (4K 24 fps 8-bit HEVC)",
                        "18.3 GB",
                    ),
                    video(
                        "Avatar.2009.3D.HSBS.mkv",
                        Some(Verdict::Hardware),
                        "Plays with hardware decoding (1080p 24 fps 8-bit H.264)",
                        "9.4 GB",
                    ),
                    video(
                        "Trip_180_LR.mp4",
                        Some(Verdict::Hardware),
                        "Plays with hardware decoding (5.7K 30 fps 8-bit HEVC)",
                        "3.3 GB",
                    ),
                    video(
                        "dive_360_TB.mkv",
                        Some(Verdict::Software),
                        "Plays with CPU decoding (6K 30 fps 10-bit HEVC)",
                        "7.0 GB",
                    ),
                    video(
                        "walk_360.mp4",
                        Some(Verdict::SoftwareMarginal),
                        "May stutter (8K 30 fps 10-bit HEVC)",
                        "4.1 GB",
                    ),
                    video(
                        "concert_8k_180_sbs.mp4",
                        Some(Verdict::Unplayable),
                        "Can't play smoothly on Steam Frame (8K 60 fps 10-bit HEVC)",
                        "12.6 GB",
                    ),
                    video(
                        "scene_FISHEYE190_LR.mp4",
                        Some(Verdict::Hardware),
                        "Plays with hardware decoding (4K 60 fps 8-bit HEVC)",
                        "2.2 GB",
                    ),
                    video(
                        "Concert_film_TB.mkv",
                        Some(Verdict::Software),
                        "Plays with CPU decoding (1080p 60 fps 10-bit HEVC)",
                        "2.9 GB",
                    ),
                    video(
                        "A very long name for a VR video shot on the beach at sunset_180_LR.mp4",
                        Some(Verdict::Hardware),
                        "Plays with hardware decoding (5.7K 60 fps 8-bit HEVC)",
                        "6.8 GB",
                    ),
                    video("short_clip.mp4", None, "", "88.4 MB"),
                ],
                ..Default::default()
            };
            just_video::ui::save_png(
                &render(&videos, &mut fonts, Some((700.0, 310.0)), true),
                &dir.join("videos.png"),
            )?;
            // The same folder with thumbnails (synthetic pictures), and the
            // toggle in both states.
            let picture = |hue: f32| {
                let (w, h) = (220u32, 124u32);
                let mut rgba = Vec::new();
                for y in 0..h {
                    for x in 0..w {
                        let t = x as f32 / w as f32;
                        let bright = 1.0 - y as f32 / h as f32 * 0.7;
                        let c = |o: f32| {
                            (((hue + o + t * 0.4).sin() * 0.5 + 0.5) * 255.0 * bright) as u8
                        };
                        rgba.extend_from_slice(&[c(0.0), c(2.1), c(4.2), 255]);
                    }
                }
                std::sync::Arc::new(just_video::media::Thumb {
                    width: w,
                    height: h,
                    rgba,
                })
            };
            let mut thumbs = videos.clone();
            thumbs.thumbnails = true;
            for (i, row) in thumbs.rows.iter_mut().enumerate().take(8) {
                if i != 2 && i != 5 {
                    row.thumbnail = Some(picture(i as f32));
                }
            }
            thumbs.rows[1].outlined = true;
            thumbs.tools = vec![Tool::icon(ToolIcon::Thumbnails, true)];
            just_video::ui::save_png(
                &render(&thumbs, &mut fonts, Some((700.0, 310.0)), true),
                &dir.join("thumbnails.png"),
            )?;
            let mut selecting = thumbs.clone();
            selecting.tools = vec![Tool::text("Cancel", false)];
            for (i, row) in selecting.rows.iter_mut().enumerate() {
                row.checked = Some(i % 2 == 0);
            }
            selecting.scroll = 2.0;
            just_video::ui::save_png(
                &render(&selecting, &mut fonts, None, false),
                &dir.join("thumbnails-select.png"),
            )?;
            let mut off = videos.clone();
            off.tools = vec![
                Tool::icon(ToolIcon::Thumbnails, false),
                Tool::icon(ToolIcon::Edit, false),
            ];
            just_video::ui::save_png(
                &render(&off, &mut fonts, None, false),
                &dir.join("thumbnails-off.png"),
            )?;
            let servers = View {
                crumbs: crumbs(&["Just Video"]),
                rows: vec![
                    Row {
                        detail: "smb://user@192.168.1.10".into(),
                        actions: vec![
                            just_video::ui::browser::Action::Edit,
                            just_video::ui::browser::Action::Remove,
                        ],
                        ..Row::new(Icon::Server, "NAS")
                    },
                    Row {
                        detail: "smb://user@10.0.0.2".into(),
                        actions: vec![
                            just_video::ui::browser::Action::Edit,
                            just_video::ui::browser::Action::Remove,
                        ],
                        ..Row::new(Icon::Server, "PC")
                    },
                    Row {
                        detail: "A Windows PC, NAS or Samba server on your network".into(),
                        ..Row::new(Icon::Add, "Add server")
                    },
                    Row {
                        detail: "Jump lengths, volume, continuing videos".into(),
                        ..Row::new(Icon::Settings, "Settings")
                    },
                ],
                ..Default::default()
            };
            let settings = View {
                crumbs: crumbs(&["Just Video", "Settings"]),
                rows: just_video::ui::settings::rows(&just_video::config::Preferences {
                    long_jump: 600,
                    ..Default::default()
                }),
                ..Default::default()
            };
            just_video::ui::save_png(
                &render(&settings, &mut fonts, Some((700.0, 400.0)), true),
                &dir.join("settings.png"),
            )?;
            // Edit mode: rename/delete on each row.
            let mut editing = folder.clone();
            editing.tools = vec![
                Tool::icon(ToolIcon::Select, false),
                Tool::icon(ToolIcon::Edit, true),
            ];
            for row in &mut editing.rows {
                row.checked = None;
                row.actions = vec![Action::Rename, Action::Delete];
            }
            just_video::ui::save_png(
                &render(&editing, &mut fonts, Some((700.0, 400.0)), true),
                &dir.join("editing.png"),
            )?;
            let mut adding = servers.clone();
            adding.form = Some(Form::new(
                "Add server",
                ["Address", "User", "Password", "Name"]
                    .iter()
                    .zip(["192.168.1.10", "user", "secret", ""])
                    .map(|(label, value)| Field {
                        label: label.to_string(),
                        value: value.into(),
                        secret: *label == "Password",
                        placeholder: "optional".into(),
                    })
                    .collect(),
                "Connect",
            ));
            if let Some(f) = &mut adding.form {
                f.toggle = Some(("Allow changing files".into(), false));
            }
            just_video::ui::save_png(
                &render(&adding, &mut fonts, Some((700.0, 700.0)), true),
                &dir.join("form.png"),
            )?;
            // Editing a saved server: an empty password keeps the saved one.
            let mut editing_server = servers.clone();
            editing_server.form = Some(Form::new(
                "Edit NAS",
                ["Address", "User", "Password", "Name"]
                    .iter()
                    .zip(["192.168.1.10:445", "WORKGROUP;user", "", "NAS"])
                    .map(|(label, value)| Field {
                        label: label.to_string(),
                        value: value.into(),
                        secret: *label == "Password",
                        placeholder: "leave empty to keep the saved one".into(),
                    })
                    .collect(),
                "Save",
            ));
            if let Some(f) = &mut editing_server.form {
                f.toggle = Some(("Allow changing files".into(), true));
            }
            just_video::ui::save_png(
                &render(&editing_server, &mut fonts, None, false),
                &dir.join("form-edit.png"),
            )?;
            just_video::ui::save_png(
                &render(&servers, &mut fonts, None, false),
                &dir.join("servers.png"),
            )?;
            let mut dialog = folder.clone();
            dialog.dialog = Some(Dialog {
                title: "Can't play smoothly on Steam Frame".into(),
                body: vec![
                    "Steam Frame's hardware video decoder only handles 8-bit video in the current SteamOS, and this video is 10-bit. Decoding it on the CPU instead reaches only about 74% of the speed needed, so playback would stutter badly.".into(),
                    "An 8-bit HEVC version or a 4K version of this video would play.".into(),
                ],
                buttons: vec!["OK".into()],
                danger: false,
            });
            just_video::ui::save_png(
                &render(&dialog, &mut fonts, Some((800.0, 790.0)), true),
                &dir.join("dialog.png"),
            )?;
            use just_video::ui::controls;
            let state = controls::State {
                paused: true,
                position: 754.0,
                duration: 5530.0,
                has_previous: true,
                has_next: false,
                curved: Some(true),
                format: controls::FORMATS[3],
                favourites: vec![controls::FORMATS[0], controls::FORMATS[3]],
                swap_eyes: false,
                subtitle_tracks: [
                    "English",
                    "English (Forced)",
                    "English (Commentary)",
                    "Danish",
                    "Danish (Commentary)",
                    "Estonian",
                    "Finnish",
                    "Finnish (Commentary)",
                    "Hindi",
                    "Latvian",
                    "Lithuanian",
                    "Norwegian",
                    "Norwegian (Commentary)",
                    "Russian",
                    "Swedish",
                    "Swedish (Commentary)",
                ]
                .map(String::from)
                .to_vec(),
                subtitle: Some(0),
                audio_tracks: vec!["English 7.1".into(), "English · Commentary".into()],
                audio: Some(0),
                list_page: 0,
                image: just_video::config::ImageAdjust {
                    brightness: 0.1,
                    contrast: 1.2,
                    saturation: 1.0,
                    rotation: 1,
                },
                dialog: None,
                caption_edit: false,
            };
            just_video::ui::save_png(
                &controls::render(&state, &mut fonts, controls::Hit::Seek(0.62)),
                &dir.join("controls.png"),
            )?;
            let editing = controls::State {
                caption_edit: true,
                ..state.clone()
            };
            just_video::ui::save_png(
                &controls::render(&editing, &mut fonts, controls::Hit::CaptionMove(1)),
                &dir.join("controls-captions.png"),
            )?;
            for (dialog, name, hover) in [
                (
                    controls::Dialog::Tracks,
                    "dialog-tracks.png",
                    controls::Hit::SubtitleTrack(Some(3)),
                ),
                (
                    controls::Dialog::Screen,
                    "dialog-screen.png",
                    controls::Hit::Pick(4),
                ),
                (
                    controls::Dialog::Image,
                    "dialog-image.png",
                    controls::Hit::Contrast(1),
                ),
            ] {
                let open = controls::State {
                    dialog: Some(dialog),
                    ..state.clone()
                };
                just_video::ui::save_png(
                    &controls::render_dialog(&open, &mut fonts, hover),
                    &dir.join(name),
                )?;
            }
            let caption = just_video::ui::captions::render(
                &just_video::subtitles::Caption::text(
                    "Good morning, everyone.\nThe train leaves at noon.",
                ),
                &mut fonts,
            );
            just_video::ui::save_png(&caption, &dir.join("caption.png"))?;
        }
        Command::Thumbnail {
            input,
            out,
            projection,
            stereo,
            swap_eyes,
            at,
            width,
            height,
        } => {
            let (source, _session) = open_input(&input, ReadAhead::default())?;
            // Detection needs the probed video, so open once for it.
            let video = Media::open(
                file_name(&input),
                open_input(&input, ReadAhead::default())?.0,
            )?
            .info()
            .video
            .clone();
            let mut layout = vr::detect(&input, video.as_ref());
            override_layout(&mut layout, projection, stereo, swap_eyes);
            let started = std::time::Instant::now();
            let thumb = just_video::media::thumbnail(
                file_name(&input),
                source,
                at,
                &layout,
                width,
                height,
            )?;
            let file = std::fs::File::create(&out)?;
            let mut encoder = png::Encoder::new(file, thumb.width, thumb.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header()?.write_image_data(&thumb.rgba)?;
            eprintln!(
                "Thumbnail {}x{} ({:?} / {:?}) in {:.0} ms",
                thumb.width,
                thumb.height,
                layout.projection,
                layout.stereo,
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        Command::XrProbe => {
            let xr = just_video::xr::context::XrContext::new()?;
            print(json!({
                "system": xr.system_name,
                "gpu": xr.gpu_name(),
                "views": xr.views.iter().map(|v| json!({
                    "recommended": [v.recommended_image_rect_width, v.recommended_image_rect_height],
                    "max": [v.max_image_rect_width, v.max_image_rect_height],
                    "samples": v.recommended_swapchain_sample_count,
                })).collect::<Vec<_>>(),
                "blend_mode": format!("{:?}", xr.blend_mode),
                "swapchain_formats": xr.swapchain_formats()?.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>(),
            }))?;
        }
        Command::ReadBench {
            url,
            mib,
            read_ahead,
        } => {
            let sessions = connect_lanes(&url, &read_ahead)?;
            let session = &sessions[0];
            let path = session.url().path.clone();
            let mut reader =
                SmbSession::open_striped(&sessions, &session.url().share, &path, read_ahead.get())?;
            let limit = mib.map_or(reader.len(), |m| (m << 20).min(reader.len()));
            let mut buffer = vec![0u8; 256 * 1024];
            let mut total = 0u64;
            let start = Instant::now();
            while total < limit {
                let want = buffer.len().min((limit - total) as usize);
                let n = match reader.read(&mut buffer[..want]) {
                    Ok(n) => n,
                    Err(e) => anyhow::bail!(
                        "{e} at {:.2} GB after {:.1}s ({:?})",
                        total as f64 / 1e9,
                        start.elapsed().as_secs_f64(),
                        reader.stats()
                    ),
                };
                if n == 0 {
                    break;
                }
                // Progress every 512 MiB.
                if (total + n as u64) >> 29 != total >> 29 {
                    eprintln!(
                        "{:.1} GB  {:.0} Mbit/s  stalled {:.1}s",
                        (total + n as u64) as f64 / 1e9,
                        (total + n as u64) as f64 * 8.0 / start.elapsed().as_secs_f64() / 1e6,
                        reader.stats().stall_seconds
                    );
                }
                total += n as u64;
            }
            let seconds = start.elapsed().as_secs_f64();
            print(json!({
                "bytes": total,
                "seconds": seconds,
                "mbit_per_second": total as f64 * 8.0 / seconds / 1e6,
                "read_stats": reader.stats(),
            }))?;
        }
        Command::BenchSeek {
            input,
            hw,
            resume,
            from,
            random,
            link_mbps,
            rtt_ms,
            play,
            hz,
            jump_every,
            jump_by,
            json,
            read_ahead,
        } => {
            use just_video::bench;
            let bench_input = if input.starts_with("smb://") {
                let started = Instant::now();
                let sessions = connect_lanes(&input, &read_ahead)?;
                let connect_ms = started.elapsed().as_secs_f64() * 1e3;
                eprintln!("Timing: connect {connect_ms:.0} ms");
                let url = sessions[0].url().clone();
                bench::Input::Smb {
                    sessions,
                    share: url.share.clone(),
                    path: url.path.split('\\').map(str::to_string).collect(),
                    connect_ms,
                }
            } else {
                bench::Input::Local {
                    path: input.clone().into(),
                    link: link_mbps.map(|mbps| bench::Link::new(mbps, rtt_ms)),
                }
            };
            let report = bench::run(
                bench_input,
                &bench::Options {
                    hw: hw_backend(hw),
                    read_ahead: read_ahead.get(),
                    resume,
                    from,
                    random,
                    play,
                    hz,
                    jump_every,
                    jump_by,
                },
            )?;
            if json {
                print(serde_json::to_value(&report)?)?;
            } else {
                print!("{}", bench::summary(&report));
            }
            let failures = report.failures();
            if !failures.is_empty() {
                eprintln!("bench-seek failed: {}", failures.join("; "));
                std::process::exit(3);
            }
        }
        Command::Bench {
            input,
            frames,
            hw,
            hw_only,
            decoder_opts,
            read_ahead,
        } => {
            let (source, _session) = open_input(&input, read_ahead.get())?;
            let mut media = Media::open(file_name(&input), source)?;
            let backend = hw_backend(hw);
            let stats = media.decode(backend, !hw_only, &decoder_opts.join(":"), frames)?;
            let fps = media.info().video.as_ref().map_or(0.0, |v| v.fps);
            let realtime = if fps > 0.0 {
                stats.frames_per_second / fps
            } else {
                0.0
            };
            let failed = stats.error.is_some() || stats.frames == 0;
            print(json!({
                "media": media.info(),
                "layout": vr::detect(&input, media.info().video.as_ref()),
                "playability": playability::assess(platform, media.info().video.as_ref()),
                "decode": stats,
                "realtime_factor": realtime,
                "note": "Decode-only throughput including network reads; excludes rendering and audio.",
            }))?;
            if failed {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}
