//! Saved servers (`~/.config/just-video/servers.json`) and their passwords
//! (`credentials.json`, mode 0600 like a mount.cifs credentials file).
//! SteamOS's game-mode session has no reachable Secret Service, so there is no
//! encrypted keystore to use; the file is readable only by its owner.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// `smb://[domain;]user@host[:port]`, no password.
    pub url: String,
    /// Renaming and deleting files on it are allowed (off: read only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writable: bool,
}

#[cfg(test)]
thread_local! {
    /// Per-test config directory, so tests never touch the real one (and
    /// don't race each other through `XDG_CONFIG_HOME`).
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Points this test thread's config at a fresh temporary directory.
#[cfg(test)]
pub(crate) fn temp_config(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jv-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    TEST_DIR.with(|d| *d.borrow_mut() = Some(dir.clone()));
    dir
}

pub fn dir() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = TEST_DIR.with(|d| d.borrow().clone()) {
        return Ok(dir);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("No home directory")?;
    Ok(base.join("just-video"))
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> anyhow::Result<T> {
    let path = dir()?.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("Parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("Read {}", path.display())),
    }
}

/// Writes atomically; `private` files are created with mode 0600.
fn write_json<T: Serialize>(name: &str, value: &T, private: bool) -> anyhow::Result<()> {
    write_file(name, &serde_json::to_vec_pretty(value)?, private)
}

/// Writes a cache file (already serialized) atomically, next to the settings.
pub(crate) fn write_cache(name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    write_file(name, bytes, false)
}

fn write_file(name: &str, bytes: &[u8], private: bool) -> anyhow::Result<()> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if private { 0o600 } else { 0o644 })
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

pub fn servers() -> anyhow::Result<Vec<Server>> {
    read_json("servers.json")
}

pub fn password(url: &str) -> anyhow::Result<Option<String>> {
    let credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    Ok(credentials.get(url).cloned())
}

/// Adds or replaces a server (matched by URL, keeping its place in the list)
/// and stores its password.
pub fn save_server(server: Server, password: &str) -> anyhow::Result<()> {
    let mut list = servers()?;
    let url = server.url.clone();
    match list.iter_mut().find(|s| s.url == url) {
        Some(saved) => *saved = server,
        None => list.push(server),
    }
    write_json("servers.json", &list, false)?;
    let mut credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    credentials.insert(url, password.to_string());
    write_json("credentials.json", &credentials, true)
}

/// After an edit changed a server's address (saved under `to` by
/// [`save_server`]): the new entry takes the old one's place, the old one and
/// its password go, and remembered layouts and resume points follow the files.
pub fn move_server(from: &str, to: &str) -> anyhow::Result<()> {
    let mut list = servers()?;
    if list.iter().any(|s| s.url == from)
        && let Some(new) = list.iter().position(|s| s.url == to)
    {
        let entry = list.remove(new);
        let old = list.iter().position(|s| s.url == from).expect("checked");
        list[old] = entry;
        write_json("servers.json", &list, false)?;
    }
    let mut credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    if credentials.remove(from).is_some() {
        write_json("credentials.json", &credentials, true)?;
    }
    // A trailing slash so `smb://u@nas` doesn't also match `smb://u@nas2`.
    let (from, to) = (format!("{from}/"), format!("{to}/"));
    move_keys::<LayoutOverride>("layouts.json", &from, &to)?;
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    move_keys::<Resume>("resume.json", &from, &to)
}

/// Renames every key of a JSON map that starts with `from` to start with `to`.
fn move_keys<T>(file: &str, from: &str, to: &str) -> anyhow::Result<()>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let mut all: BTreeMap<String, T> = read_json(file)?;
    let keys: Vec<String> = all
        .keys()
        .filter(|k| k.starts_with(from))
        .cloned()
        .collect();
    if keys.is_empty() {
        return Ok(());
    }
    for key in keys {
        let value = all.remove(&key).expect("listed");
        all.insert(format!("{to}{}", &key[from.len()..]), value);
    }
    write_json(file, &all, false)
}

/// Removes a server by name or URL, with its password. Returns whether it existed.
pub fn remove_server(name_or_url: &str) -> anyhow::Result<bool> {
    let mut list = servers()?;
    let Some(index) = list
        .iter()
        .position(|s| s.name == name_or_url || s.url == name_or_url)
    else {
        return Ok(false);
    };
    let removed = list.remove(index);
    write_json("servers.json", &list, false)?;
    let mut credentials: BTreeMap<String, String> = read_json("credentials.json")?;
    credentials.remove(&removed.url);
    write_json("credentials.json", &credentials, true)?;
    Ok(true)
}

/// A user's choice of how to show one file (when metadata and name are wrong).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct LayoutOverride {
    pub projection: crate::vr::Projection,
    pub stereo: crate::vr::Stereo,
    pub swap_eyes: bool,
    /// Picture corrections (older saves have none).
    #[serde(default)]
    pub image: ImageAdjust,
}

/// Picture corrections for one file.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageAdjust {
    /// Added to every channel (-0.5..0.5; 0 = unchanged).
    pub brightness: f32,
    /// Around mid grey (0.5..2; 1 = unchanged).
    pub contrast: f32,
    /// 0 = grey, 1 = unchanged, 2 = double.
    pub saturation: f32,
    /// Quarter turns clockwise (0..=3).
    pub rotation: u8,
}

impl Default for ImageAdjust {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            rotation: 0,
        }
    }
}

/// Every saved override, by file key.
pub fn layout_overrides() -> anyhow::Result<BTreeMap<String, LayoutOverride>> {
    read_json("layouts.json")
}

pub fn layout_override(key: &str) -> anyhow::Result<Option<LayoutOverride>> {
    let all: BTreeMap<String, LayoutOverride> = read_json("layouts.json")?;
    Ok(all.get(key).copied())
}

/// Saves (or with `None`, forgets) the override for a file.
pub fn save_layout_override(key: &str, layout: Option<LayoutOverride>) -> anyhow::Result<()> {
    let mut all: BTreeMap<String, LayoutOverride> = read_json("layouts.json")?;
    let changed = match layout {
        Some(l) => all.insert(key.to_string(), l) != Some(l),
        None => all.remove(key).is_some(),
    };
    if changed {
        write_json("layouts.json", &all, false)?;
    }
    Ok(())
}

pub fn move_layout_override(from: &str, to: &str) -> anyhow::Result<()> {
    if let Some(l) = layout_override(from)? {
        save_layout_override(from, None)?;
        save_layout_override(to, Some(l))?;
    }
    Ok(())
}

/// Where a video was left, to continue from there.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
struct Resume {
    seconds: f64,
    /// Unix time of the save; the oldest are forgotten first.
    saved: u64,
}

/// Videos whose place is remembered; older ones are forgotten.
const MAX_RESUMES: usize = 1000;

/// Serialises writes of `resume.json` (saved from background threads).
static RESUME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where to continue a file from, if it was left part way.
pub fn resume_position(key: &str) -> Option<f64> {
    let all: BTreeMap<String, Resume> = read_json("resume.json").ok()?;
    all.get(key).map(|r| r.seconds)
}

/// Remembers (or with `None`, forgets) where a file was left.
pub fn save_resume_position(key: &str, seconds: Option<f64>) -> anyhow::Result<()> {
    let _guard = RESUME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all: BTreeMap<String, Resume> = read_json("resume.json")?;
    match seconds {
        Some(seconds) => {
            let saved = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            all.insert(key.to_string(), Resume { seconds, saved });
            while all.len() > MAX_RESUMES {
                let oldest = all
                    .iter()
                    .min_by_key(|(_, r)| r.saved)
                    .map(|(k, _)| k.clone())
                    .expect("not empty");
                all.remove(&oldest);
            }
        }
        None if all.remove(key).is_none() => return Ok(()),
        None => {}
    }
    write_json("resume.json", &all, false)
}

pub fn move_resume_position(from: &str, to: &str) -> anyhow::Result<()> {
    if let Some(seconds) = resume_position(from) {
        save_resume_position(from, None)?;
        save_resume_position(to, Some(seconds))?;
    }
    Ok(())
}

/// The place worth remembering for a video stopped at `position`: none near
/// its start (nothing to skip) or its end (watched).
pub fn resume_point(position: f64, duration: f64) -> Option<f64> {
    let near_end = duration > 0.0 && (duration - position < 30.0 || position > duration * 0.95);
    (position >= 10.0 && !near_end).then_some(position)
}

/// A video format: projection and stereo layout.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Format {
    pub projection: crate::vr::Projection,
    pub stereo: crate::vr::Stereo,
}

#[derive(Default, Serialize, Deserialize)]
struct Settings {
    /// None: never changed (use the defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    favourite_formats: Option<Vec<Format>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    captions: Option<CaptionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    player: Option<Preferences>,
}

/// Choices made on the Settings screen.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Preferences {
    /// D-pad left/right (and a sideways stick flick), seconds.
    pub short_jump: u32,
    /// The same with the grip held, seconds.
    pub long_jump: u32,
    /// D-pad up/down step, percent.
    pub volume_step: u32,
    /// Continue videos where they were left.
    pub resume: bool,
    /// Thumbnails in video lists.
    pub thumbnails: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            short_jump: 5,
            long_jump: 60,
            volume_step: 10,
            resume: true,
            thumbnails: false,
        }
    }
}

impl Preferences {
    /// Seconds a D-pad press jumps (`direction` -1 or +1); the grip makes it long.
    pub fn jump(&self, direction: i32, long: bool) -> f64 {
        let seconds = if long {
            self.long_jump
        } else {
            self.short_jump
        };
        seconds.max(1) as f64 * direction.signum() as f64
    }
}

pub fn preferences() -> Preferences {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.player)
        .unwrap_or_default()
}

/// Changes the saved preferences: read, `change`, write, so other settings
/// (and fields changed elsewhere) are kept. Returns the result.
pub fn update_preferences(change: impl FnOnce(&mut Preferences)) -> anyhow::Result<Preferences> {
    let mut settings: Settings = read_json("settings.json")?;
    let mut prefs = settings.player.unwrap_or_default();
    change(&mut prefs);
    settings.player = Some(prefs);
    write_json("settings.json", &settings, false)?;
    Ok(prefs)
}

/// Subtitle size and position, the same for every video.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct CaptionSettings {
    /// Size multiplier (1 = default).
    pub scale: f32,
    /// Raised by this share of the screen's height (0 = default, negative = lower).
    pub raise: f32,
}

impl Default for CaptionSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            raise: 0.0,
        }
    }
}

pub fn caption_settings() -> CaptionSettings {
    read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.captions)
        .unwrap_or_default()
}

pub fn save_caption_settings(captions: CaptionSettings) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.captions = Some(captions);
    write_json("settings.json", &settings, false)
}

/// Formats the format button cycles through. Default: flat 2D and VR180 3D.
pub fn favourite_formats() -> Vec<Format> {
    use crate::vr::{Projection, Stereo};
    let saved = read_json::<Settings>("settings.json")
        .ok()
        .and_then(|s| s.favourite_formats);
    saved.filter(|f| !f.is_empty()).unwrap_or_else(|| {
        vec![
            Format {
                projection: Projection::Flat,
                stereo: Stereo::Mono,
            },
            Format {
                projection: Projection::Equirect180,
                stereo: Stereo::SideBySide,
            },
        ]
    })
}

pub fn save_favourite_formats(formats: &[Format]) -> anyhow::Result<()> {
    let mut settings: Settings = read_json("settings.json")?;
    settings.favourite_formats = Some(formats.to_vec());
    write_json("settings.json", &settings, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_files_still_parse() {
        let old = r#"{"captions":{"scale":1.2,"raise":0.1}}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.player, None);
        assert_eq!(s.captions.unwrap().scale, 1.2);
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.player.unwrap_or_default(), Preferences::default());
        // A partial object (fields added later) fills in defaults.
        // Fields no longer used ("volume") are ignored.
        let s: Settings =
            serde_json::from_str(r#"{"player":{"volume":70,"volume_step":5}}"#).unwrap();
        let p = s.player.unwrap();
        assert_eq!(p.volume_step, 5);
        assert_eq!(p.short_jump, 5);
        assert!(p.resume);
        assert!(!p.thumbnails);
        let s: Settings = serde_json::from_str(r#"{"player":{"thumbnails":true}}"#).unwrap();
        assert!(s.player.unwrap().thumbnails);
    }

    #[test]
    fn preferences_round_trip_keeping_other_settings() {
        let dir = temp_config("prefs");
        assert_eq!(preferences(), Preferences::default());
        save_caption_settings(CaptionSettings {
            scale: 1.5,
            raise: 0.0,
        })
        .unwrap();
        update_preferences(|p| p.long_jump = 600).unwrap();
        update_preferences(|p| p.volume_step = 20).unwrap();
        let p = preferences();
        assert_eq!((p.long_jump, p.volume_step), (600, 20));
        assert_eq!(caption_settings().scale, 1.5);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jump_lengths() {
        let p = Preferences::default();
        assert_eq!(p.jump(-1, false), -5.0);
        assert_eq!(p.jump(1, true), 60.0);
    }

    #[test]
    fn resume_skips_start_and_end() {
        assert_eq!(resume_point(5.0, 3600.0), None);
        assert_eq!(resume_point(600.0, 3600.0), Some(600.0));
        assert_eq!(resume_point(3580.0, 3600.0), None, "credits");
        assert_eq!(resume_point(110.0, 120.0), None);
        assert_eq!(resume_point(600.0, 0.0), Some(600.0), "unknown length");
    }

    #[test]
    fn round_trip_with_private_credentials() {
        // Not through XDG_CONFIG_HOME: tests on other threads read it.
        let dir = temp_config("config");
        let server = Server {
            name: "PC".into(),
            url: "smb://alice@192.168.1.10".into(),
            writable: false,
        };
        save_server(server.clone(), "secret").unwrap();
        assert_eq!(servers().unwrap(), vec![server.clone()]);
        assert_eq!(password(&server.url).unwrap().as_deref(), Some("secret"));
        let mode = std::fs::metadata(dir.join("credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let key = "smb://alice@192.168.1.10/media/VR/clip.mp4";
        assert_eq!(layout_override(key).unwrap(), None);
        let l = LayoutOverride {
            projection: crate::vr::Projection::Equirect180,
            stereo: crate::vr::Stereo::SideBySide,
            swap_eyes: false,
            image: ImageAdjust::default(),
        };
        save_layout_override(key, Some(l)).unwrap();
        move_layout_override(key, "smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap();
        assert_eq!(layout_override(key).unwrap(), None);
        assert_eq!(
            layout_override("smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap(),
            Some(l)
        );
        assert_eq!(resume_position(key), None);
        save_resume_position(key, Some(754.0)).unwrap();
        move_resume_position(key, "smb://alice@192.168.1.10/media/VR/renamed.mp4").unwrap();
        assert_eq!(resume_position(key), None);
        assert_eq!(
            resume_position("smb://alice@192.168.1.10/media/VR/renamed.mp4"),
            Some(754.0)
        );
        // Editing keeps the list order; a new address takes the old one's place,
        // and what was remembered about its files moves with it.
        let nas = Server {
            name: "NAS".into(),
            url: "smb://bob@nas".into(),
            writable: false,
        };
        save_server(nas.clone(), "pw").unwrap();
        let renamed = Server {
            name: "Office PC".into(),
            ..server.clone()
        };
        save_server(renamed.clone(), "secret").unwrap();
        assert_eq!(servers().unwrap(), vec![renamed.clone(), nas.clone()]);
        let moved = Server {
            name: "Office PC".into(),
            url: "smb://alice@192.168.1.20".into(),
            writable: false,
        };
        save_server(moved.clone(), "secret").unwrap();
        save_resume_position("smb://alice@192.168.1.100/x.mp4", Some(60.0)).unwrap();
        move_server(&server.url, &moved.url).unwrap();
        assert_eq!(servers().unwrap(), vec![moved.clone(), nas.clone()]);
        assert_eq!(password(&server.url).unwrap(), None);
        assert_eq!(password(&moved.url).unwrap().as_deref(), Some("secret"));
        let renamed_key = "smb://alice@192.168.1.20/media/VR/renamed.mp4";
        assert_eq!(layout_override(renamed_key).unwrap(), Some(l));
        assert_eq!(resume_position(renamed_key), Some(754.0));
        assert_eq!(
            resume_position("smb://alice@192.168.1.100/x.mp4"),
            Some(60.0),
            "another server whose address starts the same"
        );
        assert!(remove_server("NAS").unwrap());
        let server = moved;
        assert!(remove_server("Office PC").unwrap());
        assert!(servers().unwrap().is_empty());
        assert_eq!(password(&server.url).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
