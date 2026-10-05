//! Blocking `Read + Seek` with pipelined read-ahead over any async block source.
//!
//! The demuxer reads synchronously; behind it, a window of large block reads is
//! kept in flight so network latency overlaps decode instead of adding to it.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, task::JoinHandle};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Random-access byte source, e.g. an open SMB file.
pub trait BlockSource: Send + Sync + 'static {
    /// Reads exactly `len` bytes at `offset` (the reader never asks past the end).
    fn fetch(self: Arc<Self>, offset: u64, len: usize) -> BoxFuture<'static, io::Result<Vec<u8>>>;

    /// Releases the remote resource; called once when the reader drops.
    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Read-ahead policy. Defaults target high-bitrate 8K VR over Wi-Fi: 1 MiB
/// reads (within every SMB2.1+ server's max read size) with a 32 MiB window.
#[derive(Clone, Copy, Debug)]
pub struct ReadAhead {
    pub block_size: usize,
    pub blocks_ahead: usize,
}

impl Default for ReadAhead {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
            blocks_ahead: 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ReadStats {
    pub bytes_delivered: u64,
    pub bytes_fetched: u64,
    pub requests: u64,
    /// Time the demuxer spent blocked waiting on the network.
    pub stall_seconds: f64,
    pub discarded_blocks: u64,
}

enum Block {
    Pending(JoinHandle<io::Result<Vec<u8>>>),
    Ready(Vec<u8>),
}

/// How often a read waiting for its block logs (the request itself keeps
/// running: cancelling it would leak SMB credits).
const BLOCK_WAIT: Duration = Duration::from_secs(10);
/// How long a read may wait in total before failing.
const STALL_LIMIT: Duration = Duration::from_secs(45);
/// Failed block reads are retried this many times.
const READ_RETRIES: u32 = 3;
/// How long closing waits for outstanding reads.
const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// Blocks requested right after a jump; the window doubles with each block
/// read straight on. Reads can't be cancelled (see `detached`), so a full
/// window requested just before the demuxer jumps elsewhere (probing a file's
/// index at its end, a seek) would delay the new position until all of it
/// arrived: ~0.25 s for 32 MiB over the headset's link.
const INITIAL_WINDOW: usize = 4;
/// Bytes of finished blocks kept after the reader moved away: for demuxers
/// that come back (an MP4 index at the end of the file, then the start of the
/// data), and for jumps back into what was just played (~15 s of 6K60).
const KEPT_BYTES: usize = 192 << 20;
/// Reads in flight at most, however large the window: they can't be
/// cancelled, so after a jump the new position's data arrives only after
/// them. Over the headset's Wi-Fi 3 keep 8K (~50 Mbit/s) fed with no stall,
/// and a jump waits about half as long as with 6 (8K: ~145 vs ~260 ms);
/// 32 made a jump right after another wait up to 1.5 s.
const MAX_IN_FLIGHT: usize = 3;

pub struct ReadAheadReader<S: BlockSource> {
    runtime: Arc<Runtime>,
    source: Arc<S>,
    len: u64,
    pos: u64,
    options: ReadAhead,
    blocks: BTreeMap<u64, Block>,
    /// Recently left finished blocks, oldest first (see `KEPT_BYTES`).
    cache: VecDeque<(u64, Vec<u8>)>,
    /// Blocks to keep requested ahead now (grows to `blocks_ahead`).
    window: usize,
    /// The block the last read came from.
    last_index: Option<u64>,
    /// Reads no longer needed, left to finish. In-flight requests are never
    /// aborted: smb-rs returns a request's credits only when its response is
    /// received, so cancelled reads would leak credits until the connection
    /// can send nothing at all.
    detached: Vec<JoinHandle<io::Result<Vec<u8>>>>,
    in_flight: Arc<AtomicUsize>,
    stats: ReadStats,
    /// Asked before a read and while it waits for a block: true makes the
    /// read fail at once (see [`ReadAheadReader::with_cancel`]).
    cancel: Option<Cancel>,
    /// Set when a read failed for `cancel`: no more requests are issued, and
    /// dropping does not wait for those in flight.
    cancelled: bool,
    max_in_flight: usize,
}

/// See [`ReadAheadReader::with_cancel`].
pub type Cancel = Arc<dyn Fn() -> bool + Send + Sync>;

/// How often a read waiting for a block looks at `cancel`.
const CANCEL_POLL: Duration = Duration::from_millis(10);

impl<S: BlockSource> ReadAheadReader<S> {
    pub fn new(runtime: Arc<Runtime>, source: S, len: u64, options: ReadAhead) -> Self {
        assert!(options.block_size > 0 && options.blocks_ahead > 0);
        Self {
            runtime,
            source: Arc::new(source),
            len,
            pos: 0,
            options,
            blocks: BTreeMap::new(),
            cache: VecDeque::new(),
            window: INITIAL_WINDOW.min(options.blocks_ahead),
            last_index: None,
            detached: Vec::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            stats: ReadStats::default(),
            cancel: None,
            cancelled: false,
            max_in_flight: MAX_IN_FLIGHT,
        }
    }

    /// Reads fail with `ErrorKind::Interrupted` as soon as `cancel` says so,
    /// also while waiting for data, and no read is requested after that.
    /// (Requests already sent can't be taken back, see `detached`; dropping
    /// the reader then leaves them to finish in the background.)
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Most reads in flight at a time (default [`MAX_IN_FLIGHT`]): less data
    /// still on the link when the reader is abandoned.
    pub fn with_max_in_flight(mut self, reads: usize) -> Self {
        self.max_in_flight = reads.max(1);
        self
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn stats(&self) -> ReadStats {
        self.stats
    }

    fn kept_blocks(&self) -> usize {
        (KEPT_BYTES / self.options.block_size).max(1)
    }

    fn spawn(&mut self, index: u64) {
        if let Some(at) = self.cache.iter().position(|(i, _)| *i == index) {
            let (_, data) = self.cache.remove(at).expect("cached block");
            self.blocks.insert(index, Block::Ready(data));
            return;
        }
        let size = self.options.block_size as u64;
        let offset = index * size;
        let length = size.min(self.len - offset) as usize;
        self.stats.requests += 1;
        self.stats.bytes_fetched += length as u64;
        let fetch = self.source.clone().fetch(offset, length);
        let in_flight = self.in_flight.clone();
        in_flight.fetch_add(1, Ordering::SeqCst);
        let task = self.runtime.spawn(async move {
            let result = fetch.await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        });
        self.blocks.insert(index, Block::Pending(task));
    }

    /// Keeps `[current, current + window)` requested and drops the rest,
    /// except one block behind, which absorbs small backward seeks by demuxers.
    fn schedule(&mut self, current: u64) {
        self.window = next_window(
            self.window,
            self.last_index,
            current,
            // Kept from before counts: the reader came back to it.
            self.blocks.contains_key(&current) || self.cache.iter().any(|(i, _)| *i == current),
            self.options.blocks_ahead,
        );
        self.last_index = Some(current);
        let block_count = self.len.div_ceil(self.options.block_size as u64);
        let end = (current + self.window as u64).min(block_count);
        let keep_from = current.saturating_sub(1);
        // Only blocks beyond the full window are dropped ahead: a window that
        // is still growing keeps what it already asked for.
        let keep_until = (current + self.options.blocks_ahead as u64).min(block_count);
        let stale: Vec<u64> = self
            .blocks
            .keys()
            .copied()
            .filter(|&i| i < keep_from || i >= keep_until)
            .collect();
        for index in stale {
            match self.blocks.remove(&index) {
                Some(Block::Pending(handle)) => self.detached.push(handle),
                Some(Block::Ready(data)) => {
                    if self.cache.len() >= self.kept_blocks() {
                        self.cache.pop_front();
                    }
                    self.cache.push_back((index, data));
                }
                None => {}
            }
            self.stats.discarded_blocks += 1;
        }
        self.detached.retain(|h| !h.is_finished());
        // Detached reads count against the window, so seeking around cannot
        // pile up requests; the block needed now is always requested.
        for index in current..end {
            if !self.blocks.contains_key(&index) {
                let limit = self.window.min(self.max_in_flight);
                if index != current && self.in_flight.load(Ordering::SeqCst) >= limit {
                    break;
                }
                self.spawn(index);
            }
        }
    }
}

/// The read-ahead window for a read from block `current` (see `INITIAL_WINDOW`):
/// it doubles as reading moves on, stays while reading back within what is
/// already requested (demuxers going between audio and video), and starts
/// small again after a jump to blocks not requested.
fn next_window(
    window: usize,
    last: Option<u64>,
    current: u64,
    requested: bool,
    max: usize,
) -> usize {
    let initial = INITIAL_WINDOW.min(max);
    match last {
        Some(last) if current == last => window,
        Some(last) if current > last && requested => (window * 2).min(max),
        Some(_) if requested => window,
        _ => initial,
    }
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Read cancelled")
}

impl<S: BlockSource> Read for ReadAheadReader<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        if self.cancelled || self.cancel.as_ref().is_some_and(|c| c()) {
            self.cancelled = true;
            return Err(interrupted());
        }
        let size = self.options.block_size as u64;
        let index = self.pos / size;
        self.schedule(index);
        let mut retries = 0;
        let mut waited = Duration::ZERO;
        let mut logged = Duration::ZERO;
        loop {
            let block = self.blocks.get_mut(&index).expect("scheduled block");
            let Block::Pending(handle) = block else { break };
            let started = Instant::now();
            // Short slices while cancellable, to look at `cancel` between.
            let slice = if self.cancel.is_some() {
                CANCEL_POLL
            } else {
                BLOCK_WAIT
            };
            let result = self
                .runtime
                .block_on(async { tokio::time::timeout(slice, &mut *handle).await });
            waited += started.elapsed();
            self.stats.stall_seconds += started.elapsed().as_secs_f64();
            if result.is_err() {
                if self.cancel.as_ref().is_some_and(|c| c()) {
                    self.cancelled = true;
                    return Err(interrupted());
                }
                if waited < logged + BLOCK_WAIT && waited < STALL_LIMIT {
                    continue;
                }
                logged = waited;
            }
            match result {
                Ok(Ok(Ok(data))) => *block = Block::Ready(data),
                // Keep the request running (see `detached`) and keep waiting:
                // a slow server is better than a broken video.
                Err(_) if waited < STALL_LIMIT => {
                    eprintln!(
                        "Read-ahead: waited {:.0}s for data at {}",
                        waited.as_secs_f64(),
                        self.pos
                    );
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "The server stopped sending data",
                    ));
                }
                Ok(failed) => {
                    self.blocks.remove(&index);
                    let error = match failed {
                        Ok(e) => e.err().unwrap_or_else(|| io::Error::other("read failed")),
                        Err(e) => io::Error::other(e),
                    };
                    // A failed request is retried; a truncated packet would
                    // corrupt or stop the video.
                    if retries >= READ_RETRIES {
                        return Err(error);
                    }
                    retries += 1;
                    eprintln!("Read-ahead: retrying read at {} ({error})", self.pos);
                    std::thread::sleep(Duration::from_millis(200 * retries as u64));
                    self.spawn(index);
                }
            }
        }
        let block = self.blocks.get_mut(&index).expect("scheduled block");
        let Block::Ready(data) = block else {
            unreachable!()
        };
        let start = (self.pos - index * size) as usize;
        let n = out.len().min(data.len() - start);
        out[..n].copy_from_slice(&data[start..start + n]);
        self.pos += n as u64;
        self.stats.bytes_delivered += n as u64;
        Ok(n)
    }
}

impl<S: BlockSource> Seek for ReadAheadReader<S> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

impl<S: BlockSource> Drop for ReadAheadReader<S> {
    fn drop(&mut self) {
        let runtime = self.runtime.clone();
        // Sources may spawn cleanup work on drop (smb-rs does).
        let _guard = runtime.enter();
        let mut pending = std::mem::take(&mut self.detached);
        for (_, block) in std::mem::take(&mut self.blocks) {
            if let Block::Pending(handle) = block {
                pending.push(handle);
            }
        }
        if self.cancelled {
            // Abandoned: whoever dropped us goes on; the reads finish (never
            // aborted; see `detached`) and the file closes behind them.
            let source = self.source.clone();
            runtime.spawn(async move {
                let _ = tokio::time::timeout(CLOSE_WAIT, async {
                    for handle in pending {
                        let _ = handle.await;
                    }
                })
                .await;
                source.close().await;
            });
            return;
        }
        // Let outstanding reads finish (never abort them; see `detached`).
        runtime.block_on(async {
            let _ = tokio::time::timeout(CLOSE_WAIT, async {
                for handle in pending {
                    let _ = handle.await;
                }
            })
            .await;
        });
        runtime.block_on(self.source.close());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// In-memory source with fixed per-request latency, like a network round trip.
    struct Slow {
        data: Vec<u8>,
        latency: Duration,
        completed: Arc<AtomicUsize>,
    }

    impl BlockSource for Slow {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            Box::pin(async move {
                tokio::time::sleep(self.latency).await;
                self.completed.fetch_add(1, Ordering::SeqCst);
                let start = offset as usize;
                Ok(self.data[start..start + len].to_vec())
            })
        }
    }

    fn reader(len: usize, latency_ms: u64, options: ReadAhead) -> (ReadAheadReader<Slow>, Vec<u8>) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_time()
                .build()
                .unwrap(),
        );
        let source = Slow {
            data: data.clone(),
            latency: Duration::from_millis(latency_ms),
            completed: Arc::new(AtomicUsize::new(0)),
        };
        (
            ReadAheadReader::new(runtime, source, len as u64, options),
            data,
        )
    }

    #[test]
    fn cancel_fails_a_read_waiting_for_data_and_requests_nothing_more() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 8,
        };
        let (r, _) = reader(100_000, 400, options);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let mut r = r.with_cancel(Arc::new(move || flag.load(Ordering::SeqCst)));
        let completed = r.source.completed.clone();
        let mut buf = [0u8; 10];
        let flag = stop.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let error = r.read(&mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_millis(250));
        stopper.join().unwrap();
        let requests = r.stats().requests;
        // Failed for good, with nothing new requested, also after a seek.
        r.seek(SeekFrom::Start(50_000)).unwrap();
        assert!(r.read(&mut buf).is_err());
        assert_eq!(r.stats().requests, requests);
        // Dropping doesn't wait for the reads in flight, which still finish.
        let _runtime = r.runtime.clone();
        let dropping = Instant::now();
        drop(r);
        assert!(dropping.elapsed() < Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(completed.load(Ordering::SeqCst) as u64, requests);
    }

    #[test]
    fn max_in_flight_limits_read_ahead() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 8,
        };
        let (r, _) = reader(100_000, 50, options);
        let mut r = r.with_max_in_flight(1);
        let mut buf = [0u8; 10];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.stats().requests, 1);
    }

    #[test]
    fn sequential_read_matches_source() {
        let options = ReadAhead {
            block_size: 4096,
            blocks_ahead: 8,
        };
        let (mut r, data) = reader(100_003, 0, options);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert_eq!(r.stats().bytes_delivered, data.len() as u64);
    }

    #[test]
    fn seeks_land_on_the_right_bytes() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 4,
        };
        let (mut r, data) = reader(50_000, 0, options);
        let mut buf = [0u8; 1500];
        for &(from, expect) in &[
            (SeekFrom::End(-1500), 48_500u64),
            (SeekFrom::Start(0), 0),
            (SeekFrom::Start(31_337), 31_337),
            (SeekFrom::Current(-2000), 30_837),
        ] {
            assert_eq!(r.seek(from).unwrap(), expect);
            r.read_exact(&mut buf).unwrap();
            let at = expect as usize;
            assert_eq!(&buf[..], &data[at..at + 1500]);
        }
        r.seek(SeekFrom::Start(50_000)).unwrap();
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert!(r.seek(SeekFrom::Current(-60_000)).is_err());
    }

    /// Regression: aborting SMB reads leaked credits and hung the connection.
    #[test]
    fn reads_are_never_cancelled() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 8,
        };
        let (mut r, data) = reader(100_000, 20, options);
        let completed = r.source.completed.clone();
        let mut buf = [0u8; 10];
        r.read_exact(&mut buf).unwrap();
        // Jump far away: the first window's reads must still complete.
        r.seek(SeekFrom::Start(90_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[90_000..90_010]);
        let requests = r.stats().requests;
        assert!(requests <= 16 + 1, "in-flight cap exceeded: {requests}");
        drop(r);
        assert_eq!(completed.load(Ordering::SeqCst) as u64, requests);
    }

    #[test]
    fn jumping_back_into_what_was_played_reads_nothing_again() {
        // 64 KiB blocks: 200 of them played (12.5 MiB), well within what's kept.
        let options = ReadAhead {
            block_size: 64 * 1024,
            blocks_ahead: 4,
        };
        let (mut r, data) = reader(220 * 64 * 1024, 0, options);
        let mut buf = vec![0u8; 200 * 64 * 1024];
        r.read_exact(&mut buf).unwrap();
        let requests = r.stats().requests;
        // Back to the start, and to the middle: no new reads.
        for at in [0usize, 100 * 64 * 1024 + 5] {
            r.seek(SeekFrom::Start(at as u64)).unwrap();
            let mut part = [0u8; 1000];
            r.read_exact(&mut part).unwrap();
            assert_eq!(&part[..], &data[at..at + 1000]);
        }
        assert_eq!(r.stats().requests, requests);
    }

    /// Fails every block's first request.
    struct Flaky {
        data: Vec<u8>,
        failed: std::sync::Mutex<std::collections::HashSet<u64>>,
    }

    impl BlockSource for Flaky {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            Box::pin(async move {
                if self.failed.lock().unwrap().insert(offset) {
                    return Err(io::Error::other("network hiccup"));
                }
                Ok(self.data[offset as usize..offset as usize + len].to_vec())
            })
        }
    }

    #[test]
    fn failed_reads_are_retried() {
        let data: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_time()
                .build()
                .unwrap(),
        );
        let source = Flaky {
            data: data.clone(),
            failed: Default::default(),
        };
        let options = ReadAhead {
            block_size: 4096,
            blocks_ahead: 2,
        };
        let mut r = ReadAheadReader::new(runtime, source, data.len() as u64, options);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn window_grows_and_restarts_after_jumps() {
        // First read, then straight on: 4, 8, 16, 32 (the maximum).
        assert_eq!(next_window(32, None, 0, false, 32), 4);
        assert_eq!(next_window(4, Some(0), 1, true, 32), 8);
        assert_eq!(next_window(8, Some(1), 2, true, 32), 16);
        assert_eq!(next_window(32, Some(2), 3, true, 32), 32);
        // More reads from the same block keep it; a jump starts over.
        assert_eq!(next_window(16, Some(5), 5, true, 32), 16);
        assert_eq!(next_window(32, Some(5), 900, false, 32), 4);
        assert_eq!(next_window(32, Some(5), 2, false, 32), 4);
        // Back into blocks still held (interleaved audio and video) keeps it.
        assert_eq!(next_window(32, Some(5), 4, true, 32), 32);
        // Small read-aheads never exceed their own size.
        assert_eq!(next_window(2, None, 0, false, 2), 2);
    }

    /// A network link: one read at a time at a fixed speed, so later reads
    /// wait for earlier ones (unlike `Slow`, whose requests overlap freely).
    struct Link {
        data: Vec<u8>,
        per_byte: Duration,
        /// Added to every read after its turn on the link (not queued).
        rtt: Duration,
        busy_until: std::sync::Mutex<Instant>,
        /// Offsets fetched, in order.
        fetched: std::sync::Mutex<Vec<u64>>,
    }

    impl BlockSource for Link {
        fn fetch(
            self: Arc<Self>,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'static, io::Result<Vec<u8>>> {
            let done = {
                let mut busy = self.busy_until.lock().unwrap();
                *busy = (*busy).max(Instant::now()) + self.per_byte * len as u32;
                *busy + self.rtt
            };
            self.fetched.lock().unwrap().push(offset);
            Box::pin(async move {
                tokio::time::sleep_until(done.into()).await;
                let start = offset as usize;
                Ok(self.data[start..start + len].to_vec())
            })
        }
    }

    fn link_reader(len: usize, ms_per_block: u64, options: ReadAhead) -> ReadAheadReader<Link> {
        link_reader_rtt(len, ms_per_block, 0, options)
    }

    fn link_reader_rtt(
        len: usize,
        ms_per_block: u64,
        rtt_ms: u64,
        options: ReadAhead,
    ) -> ReadAheadReader<Link> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_time()
                .build()
                .unwrap(),
        );
        let source = Link {
            data: (0..len).map(|i| (i % 251) as u8).collect(),
            per_byte: Duration::from_millis(ms_per_block) / options.block_size as u32,
            rtt: Duration::from_millis(rtt_ms),
            busy_until: std::sync::Mutex::new(Instant::now()),
            fetched: Default::default(),
        };
        ReadAheadReader::new(runtime, source, len as u64, options)
    }

    /// A demuxer alternating between streams stored a few blocks apart must
    /// keep the full window (reading ahead), not start it over each time.
    #[test]
    fn interleaved_reads_keep_the_window() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 32,
        };
        let mut r = link_reader_rtt(204_000, 1, 20, options);
        let mut buf = [0u8; 10];
        let started = Instant::now();
        for block in 0..200u64 {
            for at in [block + 3, block] {
                r.seek(SeekFrom::Start(at * 1000)).unwrap();
                r.read_exact(&mut buf).unwrap();
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(r.window, 32);
        // 3 reads per 20 ms round trip: ~1.3 s.
        assert!(elapsed < Duration::from_millis(2000), "took {elapsed:?}");
    }

    /// Probing an MP4 with its index at the end: header, the end, then back.
    #[test]
    fn a_jump_soon_after_opening_does_not_wait_for_a_full_window() {
        let options = ReadAhead {
            block_size: 1000,
            blocks_ahead: 32,
        };
        // 5 ms per block: a full window queued ahead would take 160 ms.
        let mut r = link_reader(200_000, 5, options);
        let mut buf = [0u8; 100];
        let started = Instant::now();
        r.read_exact(&mut buf).unwrap();
        r.seek(SeekFrom::Start(150_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(80), "took {elapsed:?}");
        // Back to the start: block 0 is still here, not fetched again.
        r.seek(SeekFrom::Start(10)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &r.source.data[10..110]);
        let fetched = r.source.fetched.lock().unwrap().clone();
        assert_eq!(
            fetched.iter().filter(|&&o| o == 0).count(),
            1,
            "{fetched:?}"
        );
    }

    #[test]
    fn pipelining_hides_latency() {
        // 64 blocks at 10 ms each: ~640 ms if serial, ~40 ms with 16 in flight.
        let options = ReadAhead {
            block_size: 8192,
            blocks_ahead: 16,
        };
        let (mut r, _) = reader(64 * 8192, 10, options);
        let started = Instant::now();
        std::io::copy(&mut r, &mut std::io::sink()).unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(250), "took {elapsed:?}");
        assert_eq!(r.stats().requests, 64);
    }
}
