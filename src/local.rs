//! The headset's own storage, offered in the browser as a built-in server
//! ("This headset") next to the SMB ones. Its "shares" are root folders:
//! Videos, Downloads and the home folder, plus every SD card or USB drive
//! mounted under /run/media. Paths below a root are the same `Vec<String>`
//! the SMB browser uses, so navigation, probing and playback work unchanged.

use crate::config::Server;
use crate::smb::Entry;
use anyhow::{Context, bail};
use std::{fs, io::BufReader, path::PathBuf};

/// The pseudo-URL identifying local storage (never a valid `smb://` URL).
pub const URL: &str = "local:";

pub fn server() -> Server {
    Server {
        name: "This headset".into(),
        url: URL.into(),
        // The headset's own files: renaming and deleting are allowed (in
        // edit mode, as on a writable server).
        writable: true,
    }
}

pub fn is_local(server: &Server) -> bool {
    server.url == URL
}

/// Root folders by name, existing ones only: Videos, Downloads, Home, then
/// removable drives (`/run/media/<user>/<label>`, or `/run/media/<label>`).
pub fn roots() -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for name in ["Videos", "Downloads"] {
            let dir = home.join(name);
            if dir.is_dir() {
                roots.push((name.to_string(), dir));
            }
        }
        roots.push(("Home".to_string(), home));
    }
    let user = std::env::var("USER").unwrap_or_default();
    let mut drives = Vec::new();
    for (dir, nested) in [
        (PathBuf::from("/run/media").join(&user), false),
        (PathBuf::from("/run/media"), true),
    ] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // /run/media/<user> holds drives itself; it isn't one.
            if (nested && name == user) || !path.is_dir() {
                continue;
            }
            drives.push((name, path));
        }
    }
    drives.sort();
    for (name, path) in drives {
        // Keep root names unique: they are what paths start from.
        let mut unique = name.clone();
        let mut n = 2;
        while roots.iter().any(|(r, _)| *r == unique) {
            unique = format!("{name} ({n})");
            n += 1;
        }
        roots.push((unique, path));
    }
    roots
}

pub fn shares() -> Vec<String> {
    roots().into_iter().map(|(name, _)| name).collect()
}

/// The file or folder `path` below the root `share`. Every step must be a
/// plain name: no separators, `.` or `..`, so a path never leaves its root.
pub fn resolve(share: &str, path: &[String]) -> anyhow::Result<PathBuf> {
    let (_, mut full) = roots()
        .into_iter()
        .find(|(name, _)| name == share)
        .with_context(|| format!("{share} is not available"))?;
    for step in path {
        check_name(step)?;
        full.push(step);
    }
    Ok(full)
}

fn check_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        bail!("Invalid name: {name:?}");
    }
    Ok(())
}

/// Folder contents, folders first, then by name (like the SMB listing).
/// Symlinks are followed; entries that can't be read are left out.
pub fn list(share: &str, path: &[String]) -> anyhow::Result<Vec<Entry>> {
    let dir = resolve(share, path)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("Can't open {}", dir.display()))? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = fs::metadata(entry.path()) else {
            continue;
        };
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: filetime(&meta),
        });
    }
    entries.sort_by(|a, b| {
        (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase()))
    });
    Ok(entries)
}

/// Last write time in the units SMB listings use (100 ns since 1601), so
/// caches keyed on it treat local files like remote ones. 0 if unknown.
fn filetime(meta: &fs::Metadata) -> u64 {
    const UNIX_TO_1601: u64 = 116_444_736_000_000_000;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| UNIX_TO_1601 + d.as_secs() * 10_000_000 + u64::from(d.subsec_nanos() / 100))
}

pub fn open(share: &str, path: &[String]) -> anyhow::Result<BufReader<fs::File>> {
    let file = resolve(share, path)?;
    let opened = fs::File::open(&file).with_context(|| format!("Can't open {}", file.display()))?;
    Ok(BufReader::with_capacity(1 << 20, opened))
}

/// Renames within the same folder.
pub fn rename(share: &str, path: &[String], new_name: &str) -> anyhow::Result<()> {
    check_name(new_name)?;
    let from = resolve(share, path)?;
    let to = from.with_file_name(new_name);
    if to.exists() {
        bail!("{new_name} already exists");
    }
    fs::rename(&from, &to).with_context(|| format!("Can't rename {}", from.display()))
}

/// Deletes a file, or an empty folder (like SMB's delete; never recursive).
pub fn delete(share: &str, path: &[String]) -> anyhow::Result<()> {
    if path.is_empty() {
        bail!("A root folder can't be deleted");
    }
    let target = resolve(share, path)?;
    let meta = fs::symlink_metadata(&target)?;
    if meta.is_dir() {
        fs::remove_dir(&target)
            .with_context(|| format!("Can't delete {} (is it empty?)", target.display()))
    } else {
        fs::remove_file(&target).with_context(|| format!("Can't delete {}", target.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_never_leave_the_root() {
        for bad in ["", ".", "..", "a/b", "x\0"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name("clip_180_LR.mp4").is_ok());
        assert!(check_name("..hidden").is_ok());
    }

    #[test]
    fn local_server_is_recognised() {
        assert!(is_local(&server()));
        assert!(!is_local(&Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
            writable: false,
        }));
    }
}
