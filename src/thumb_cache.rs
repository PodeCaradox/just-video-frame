//! List thumbnails kept across runs: `thumbnails/<hash>.png` next to the
//! settings, each at display size (about 110 KB raw, far less as PNG). The
//! name hashes everything the picture depends on (file, size, modification
//! time, layout, thumbnail size), so a changed file or layout just misses and
//! the stale file ages out. Errors are never kept: a broken read may work
//! next time.

use crate::media::Thumb;
use crate::vr::Layout;
use std::path::{Path, PathBuf};

/// Size of every thumbnail; rows are laid out around it.
pub const THUMB_W: u32 = 220;
pub const THUMB_H: u32 = 124;

/// Files beyond this (oldest modification time first) are deleted.
const MAX_FILES: usize = 5000;
/// Pruning runs again after this many stores.
const PRUNE_EVERY: usize = 200;

/// FNV-1a, 64 bit. Written out because `DefaultHasher` may change between
/// Rust releases, which would orphan every saved thumbnail.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The cache name for a file's thumbnail (16 hex digits); `file_key` is
/// `library::file_key`. Only what changes the picture is in it.
pub fn key(file_key: &str, size: u64, modified: u64, layout: &Layout) -> String {
    let text = format!(
        "{file_key}\0{size}\0{modified}\0{:?}/{:?}/{}\0{THUMB_W}x{THUMB_H}",
        layout.projection, layout.stereo, layout.swap_eyes
    );
    format!("{:016x}", fnv1a(text.as_bytes()))
}

pub struct ThumbCache {
    dir: PathBuf,
    stores: usize,
}

impl ThumbCache {
    /// The cache in the config directory (created when first stored to).
    pub fn open() -> Option<Self> {
        let dir = crate::config::dir().ok()?.join("thumbnails");
        Some(Self::at(dir))
    }

    /// A cache in `dir` (for tests).
    pub fn at(dir: PathBuf) -> Self {
        Self { dir, stores: 0 }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.png"))
    }

    /// The stored thumbnail, if there is a readable one of the right size.
    pub fn load(&self, key: &str) -> Option<Thumb> {
        let file = std::io::BufReader::new(std::fs::File::open(self.path(key)).ok()?);
        let mut reader = png::Decoder::new(file).read_info().ok()?;
        let mut rgba = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut rgba).ok()?;
        let right = (info.width, info.height) == (THUMB_W, THUMB_H)
            && info.color_type == png::ColorType::Rgba
            && info.bit_depth == png::BitDepth::Eight;
        right.then(|| {
            rgba.truncate(info.buffer_size());
            Thumb {
                width: info.width,
                height: info.height,
                rgba,
            }
        })
    }

    /// Saves a thumbnail (written aside, then renamed, so a reader never
    /// sees half a file); prunes now and then. A failure only costs making
    /// it again.
    pub fn store(&mut self, key: &str, thumb: &Thumb) {
        if let Err(e) = self.write(key, thumb) {
            eprintln!("Thumbnails: can't save: {e:#}");
            return;
        }
        self.stores += 1;
        if self.stores.is_multiple_of(PRUNE_EVERY) {
            self.prune(MAX_FILES);
        }
    }

    fn write(&self, key: &str, thumb: &Thumb) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{key}.tmp"));
        let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut encoder = png::Encoder::new(file, thumb.width, thumb.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&thumb.rgba)?;
        std::fs::rename(&tmp, self.path(key))?;
        Ok(())
    }

    /// Keeps the newest [`MAX_FILES`] files.
    pub fn prune_old(&self) {
        self.prune(MAX_FILES);
    }

    /// Deletes the oldest files beyond `keep`.
    pub fn prune(&self, keep: usize) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                Path::new(&e.file_name())
                    .extension()
                    .is_some_and(|x| x == "png")
            })
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        if files.len() <= keep {
            return;
        }
        files.sort();
        let excess = files.len() - keep;
        for (_, path) in files.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vr::{Evidence, Projection, Stereo};

    fn layout() -> Layout {
        Layout {
            projection: Projection::Flat,
            stereo: Stereo::Mono,
            swap_eyes: false,
            projection_from: Evidence::Aspect,
            stereo_from: Evidence::Aspect,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jv-thumbs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn picture(seed: u8) -> Thumb {
        let rgba = (0..THUMB_W * THUMB_H * 4)
            .map(|i| (i as u8).wrapping_mul(seed))
            .collect();
        Thumb {
            width: THUMB_W,
            height: THUMB_H,
            rgba,
        }
    }

    #[test]
    fn fnv_is_stable() {
        // Published FNV-1a 64 test vectors: names on disk must never change.
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn key_follows_what_changes_the_picture() {
        let base = key("smb://nas/media/a.mp4", 100, 7, &layout());
        assert_eq!(base.len(), 16);
        assert_eq!(base, key("smb://nas/media/a.mp4", 100, 7, &layout()));
        assert_ne!(base, key("smb://nas/media/b.mp4", 100, 7, &layout()));
        assert_ne!(base, key("smb://nas/media/a.mp4", 101, 7, &layout()));
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 8, &layout()));
        let mut other = layout();
        other.projection = Projection::Equirect180;
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
        let mut other = layout();
        other.stereo = Stereo::SideBySide;
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
        let mut other = layout();
        other.swap_eyes = true;
        assert_ne!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
        // How sure the detection was doesn't change the picture.
        let mut other = layout();
        other.projection_from = Evidence::Filename;
        assert_eq!(base, key("smb://nas/media/a.mp4", 100, 7, &other));
    }

    #[test]
    fn stored_thumbnails_load_back() {
        let dir = temp("round-trip");
        let mut cache = ThumbCache::at(dir.clone());
        assert!(cache.load("0123456789abcdef").is_none());
        let thumb = picture(7);
        cache.store("0123456789abcdef", &thumb);
        let loaded = cache.load("0123456789abcdef").unwrap();
        assert_eq!(loaded.rgba, thumb.rgba);
        assert_eq!((loaded.width, loaded.height), (THUMB_W, THUMB_H));
        // No temporary file is left, and junk is a miss, not a crash.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::write(dir.join("bad.png"), b"not a png").unwrap();
        assert!(cache.load("bad").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest() {
        let dir = temp("prune");
        let mut cache = ThumbCache::at(dir.clone());
        for n in 0..5 {
            cache.store(&format!("{n:016x}"), &picture(3));
            let age = std::time::Duration::from_secs(1000 - n * 100);
            let file = std::fs::File::options()
                .write(true)
                .open(cache.path(&format!("{n:016x}")))
                .unwrap();
            file.set_modified(std::time::SystemTime::now() - age)
                .unwrap();
        }
        cache.prune(3);
        for n in 0..5 {
            assert_eq!(cache.load(&format!("{n:016x}")).is_some(), n >= 2, "{n}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
