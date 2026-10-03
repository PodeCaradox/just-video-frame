//! What each video's header said (`probe-cache.json`), kept across runs so
//! a folder shows its marks without reading every file again. Probing takes
//! ~250 ms of network and CPU per video on the headset.
//!
//! It keeps facts ([`VideoInfo`]), not verdicts: playability and layout are
//! worked out from them each time, so rule changes apply at once. A file
//! whose size or modification time changed is probed again. Errors are
//! never kept: a broken read may work next time.

use crate::media::VideoInfo;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Mutex};

/// Bump when [`VideoInfo`]'s meaning changes (new fields alone don't need it).
const VERSION: u32 = 1;
/// Least recently used entries beyond this are dropped when saving.
const MAX_ENTRIES: usize = 20_000;
const FILE: &str = "probe-cache.json";

#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    size: u64,
    /// Last write time as the server reports it (100 ns since 1601).
    modified: u64,
    video: Option<VideoInfo>,
    /// When last looked up or stored (see `ProbeCache::clock`).
    used: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    entries: HashMap<String, Cached>,
}

#[derive(Default)]
pub struct ProbeCache {
    entries: HashMap<String, Cached>,
    /// Counts lookups and stores, for least-recently-used order.
    clock: u64,
    /// Changed since the last save.
    dirty: bool,
}

impl ProbeCache {
    /// The saved cache, or an empty one (missing, unreadable or old).
    pub fn load() -> Self {
        let stored: Stored = crate::config::dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join(FILE)).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(|s: &Stored| s.version == VERSION)
            .unwrap_or_default();
        let clock = stored.entries.values().map(|c| c.used).max().unwrap_or(0);
        Self {
            entries: stored.entries,
            clock,
            dirty: false,
        }
    }

    /// The video info stored for `key` (see `library::file_key`), if the
    /// file still has this size and modification time. `Some(None)`: no
    /// video stream.
    pub fn get(&mut self, key: &str, size: u64, modified: u64) -> Option<Option<VideoInfo>> {
        let entry = self.entries.get_mut(key)?;
        if entry.size != size || entry.modified != modified {
            return None;
        }
        self.clock += 1;
        entry.used = self.clock;
        Some(entry.video.clone())
    }

    pub fn insert(&mut self, key: String, size: u64, modified: u64, video: Option<VideoInfo>) {
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(
            key,
            Cached {
                size,
                modified,
                video,
                used,
            },
        );
        self.dirty = true;
    }

    /// A file was renamed: its entry moves along.
    pub fn rename(&mut self, from: &str, to: &str) {
        if let Some(entry) = self.entries.remove(from) {
            self.entries.insert(to.to_string(), entry);
            self.dirty = true;
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.dirty |= self.entries.remove(key).is_some();
    }

    /// Writes the cache if it changed, without holding the lock while
    /// writing. A failure only costs probing again.
    pub fn save(cache: &Mutex<Self>) {
        // One save at a time, in order: they share a temporary file, and an
        // older snapshot must not replace a newer one.
        static SAVING: Mutex<()> = Mutex::new(());
        let _saving = SAVING.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = cache.lock().expect("probe cache").changes();
        if let Some(bytes) = bytes
            && let Err(e) = crate::config::write_cache(FILE, &bytes)
        {
            eprintln!("Probe cache: can't save: {e:#}");
        }
    }

    /// The cache to write, if it changed since last time.
    fn changes(&mut self) -> Option<Vec<u8>> {
        if !self.dirty {
            return None;
        }
        if self.entries.len() > MAX_ENTRIES {
            let mut used: Vec<u64> = self.entries.values().map(|c| c.used).collect();
            used.sort_unstable();
            let cutoff = used[self.entries.len() - MAX_ENTRIES];
            self.entries.retain(|_, c| c.used >= cutoff);
        }
        let stored = Stored {
            version: VERSION,
            entries: std::mem::take(&mut self.entries),
        };
        let bytes = serde_json::to_vec(&stored);
        self.entries = stored.entries;
        self.dirty = false;
        bytes.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(width: u32) -> VideoInfo {
        VideoInfo {
            codec: "hevc".into(),
            profile: Some("Main".into()),
            pixel_format: Some("yuv420p".into()),
            width,
            height: width / 2,
            bit_depth: 8,
            fps: 60.0,
            stereo_mode: None,
            stereo_inverted: false,
            projection: None,
            horizontal_degrees: None,
        }
    }

    #[test]
    fn survives_a_restart_and_notices_changed_files() {
        crate::config::temp_config("probe-cache");
        let cache = Mutex::new(ProbeCache::load());
        let mut c = cache.lock().unwrap();
        c.insert("smb://a/s/x.mp4".into(), 100, 7, Some(video(6144)));
        c.insert("smb://a/s/audio.mp4".into(), 50, 7, None);
        drop(c);
        ProbeCache::save(&cache);

        let mut cache = ProbeCache::load();
        let hit = cache.get("smb://a/s/x.mp4", 100, 7).unwrap().unwrap();
        assert_eq!(hit.width, 6144);
        assert!(cache.get("smb://a/s/audio.mp4", 50, 7).unwrap().is_none());
        // Rewritten or replaced files are probed again.
        assert!(cache.get("smb://a/s/x.mp4", 101, 7).is_none());
        assert!(cache.get("smb://a/s/x.mp4", 100, 8).is_none());

        cache.rename("smb://a/s/x.mp4", "smb://a/s/y.mp4");
        assert!(cache.get("smb://a/s/x.mp4", 100, 7).is_none());
        assert!(cache.get("smb://a/s/y.mp4", 100, 7).is_some());
        cache.remove("smb://a/s/y.mp4");
        let cache = Mutex::new(cache);
        ProbeCache::save(&cache);
        assert!(ProbeCache::load().get("smb://a/s/y.mp4", 100, 7).is_none());
    }

    #[test]
    fn keeps_the_most_recently_used() {
        crate::config::temp_config("probe-cache-lru");
        let mut cache = ProbeCache::load();
        for i in 0..MAX_ENTRIES + 10 {
            cache.insert(format!("k{i}"), 1, 1, None);
        }
        // The oldest entry, used again, stays.
        assert!(cache.get("k0", 1, 1).is_some());
        ProbeCache::save(&Mutex::new(cache));
        let mut cache = ProbeCache::load();
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get("k0", 1, 1).is_some());
        assert!(cache.get("k1", 1, 1).is_none());
        assert!(cache.get(&format!("k{}", MAX_ENTRIES + 9), 1, 1).is_some());
    }
}
