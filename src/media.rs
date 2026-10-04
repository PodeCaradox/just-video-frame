//! Safe wrapper over `native/media.c`: FFmpeg demux/decode over any `Read + Seek`.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::{Read, Seek, SeekFrom},
    panic::{AssertUnwindSafe, catch_unwind},
};

/// Hardware decoding backend for this platform: the V4L2 (Qualcomm iris)
/// decoder on ARM64 / Steam Frame, Vulkan video elsewhere.
pub fn default_hw_backend() -> Option<&'static str> {
    if cfg!(target_arch = "aarch64") {
        Some("v4l2m2m")
    } else {
        Some("vulkan")
    }
}

pub trait Source: Read + Seek + Send {}
impl<T: Read + Seek + Send> Source for T {}

/// What the demuxer has read so far: bytes, and how often it read somewhere
/// other than straight on (each one a new network round trip).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IoCount {
    pub bytes: u64,
    pub jumps: u64,
    /// Time spent waiting for reads (microseconds).
    pub wait_us: u64,
}

impl IoCount {
    /// Reads since `earlier`.
    pub fn since(self, earlier: IoCount) -> IoCount {
        IoCount {
            bytes: self.bytes - earlier.bytes,
            jumps: self.jumps - earlier.jumps,
            wait_us: self.wait_us - earlier.wait_us,
        }
    }
}

impl std::fmt::Display for IoCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1} MiB in {} jumps, waited {} ms",
            self.bytes as f64 / (1 << 20) as f64,
            self.jumps,
            self.wait_us / 1000
        )
    }
}

/// Counts what passes through to FFmpeg (for timing logs and benchmarks).
struct Counted<S> {
    inner: S,
    count: std::sync::Arc<std::sync::Mutex<IoCount>>,
    /// Where the next read continues straight on; a read elsewhere is a jump.
    position: u64,
    expected: Option<u64>,
}

impl<S: Read> Read for Counted<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let began = std::time::Instant::now();
        let n = self.inner.read(buf)?;
        let mut count = self.count.lock().expect("io count");
        count.wait_us += began.elapsed().as_micros() as u64;
        if self.expected != Some(self.position) {
            count.jumps += 1;
        }
        count.bytes += n as u64;
        self.position += n as u64;
        self.expected = Some(self.position);
        Ok(n)
    }
}

impl<S: Seek> Seek for Counted<S> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.position = self.inner.seek(from)?;
        Ok(self.position)
    }
}

const AVSEEK_SIZE: c_int = 0x10000;
const AVERROR_EIO: c_int = -5;

#[repr(C)]
struct RawInfo {
    container: [c_char; 64],
    duration_seconds: f64,
    bit_rate: i64,
    video_codec: [c_char; 32],
    video_profile: [c_char; 48],
    pixel_format: [c_char; 32],
    width: i32,
    height: i32,
    bit_depth: i32,
    fps: f64,
    stereo_mode: [c_char; 32],
    stereo_inverted: i32,
    projection: [c_char; 48],
    bound_left: u32,
    bound_top: u32,
    bound_right: u32,
    bound_bottom: u32,
    audio_codec: [c_char; 32],
    audio_channels: i32,
    audio_sample_rate: i32,
}

#[repr(C)]
struct RawSubtitleTrack {
    codec: [c_char; 32],
    language: [c_char; 16],
    title: [c_char; 64],
    is_default: i32,
    forced: i32,
    supported: i32,
}

#[repr(C)]
struct RawAudioTrack {
    codec: [c_char; 32],
    language: [c_char; 16],
    title: [c_char; 64],
    channels: i32,
    is_default: i32,
}

/// An audio stream in the file.
#[derive(Clone, Debug, Serialize)]
pub struct AudioTrackInfo {
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: u32,
    pub default: bool,
}

#[repr(C)]
struct RawCue {
    start: f64,
    end: f64,
    clear: i32,
    text: [c_char; 1024],
    rgba: *mut u8,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    frame_width: i32,
    frame_height: i32,
}

/// A subtitle stream in the file.
#[derive(Clone, Debug, Serialize)]
pub struct SubtitleTrackInfo {
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// A text format we can show (bitmap subtitles, e.g. PGS, are not).
    pub supported: bool,
}

#[repr(C)]
struct RawStats {
    frames: i32,
    hardware_frames: i32,
    software_frames: i32,
    elapsed_seconds: f64,
    decoder: [c_char; 48],
    hw_backend: [c_char; 16],
    pixel_format: [c_char; 32],
    note: [c_char; 128],
    error: [c_char; 256],
}

type ReadFn = unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int;
type SeekFn = unsafe extern "C" fn(*mut c_void, i64, c_int) -> i64;

#[repr(C)]
struct RawMedia {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn jv_media_open(
        name: *const c_char,
        read: ReadFn,
        seek: SeekFn,
        opaque: *mut c_void,
        info: *mut RawInfo,
        error: *mut c_char,
        error_size: c_int,
    ) -> *mut RawMedia;
    fn jv_media_decode(
        media: *mut RawMedia,
        hw_backend: *const c_char,
        allow_software: c_int,
        decoder_options: *const c_char,
        frame_limit: c_int,
        stats: *mut RawStats,
    ) -> c_int;
    fn jv_media_close(media: *mut RawMedia);
}

type BoxedSource = Box<dyn Source>;

unsafe extern "C" fn read_cb(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    // SAFETY: opaque is the stable Box<BoxedSource> owned by `Media`; FFmpeg
    // passes a writable buffer of `size` bytes.
    let source = unsafe { &mut *(opaque as *mut BoxedSource) };
    let out = unsafe { std::slice::from_raw_parts_mut(buf, size.max(0) as usize) };
    match catch_unwind(AssertUnwindSafe(|| source.read(out))) {
        Ok(Ok(n)) => n as c_int,
        _ => AVERROR_EIO,
    }
}

unsafe extern "C" fn seek_cb(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let source = unsafe { &mut *(opaque as *mut BoxedSource) };
    let result = catch_unwind(AssertUnwindSafe(|| {
        if whence == AVSEEK_SIZE {
            let here = source.stream_position()?;
            let end = source.seek(SeekFrom::End(0))?;
            source.seek(SeekFrom::Start(here))?;
            return Ok(end);
        }
        let from = match whence {
            0 => SeekFrom::Start(offset.try_into().map_err(std::io::Error::other)?),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
        };
        source.seek(from)
    }));
    match result {
        Ok(Ok(position)) => position as i64,
        _ => AVERROR_EIO as i64,
    }
}

fn text(raw: &[c_char]) -> String {
    // SAFETY: the C side always NUL-terminates via snprintf into zeroed arrays.
    unsafe { CStr::from_ptr(raw.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn optional(raw: &[c_char]) -> Option<String> {
    Some(text(raw)).filter(|s| !s.is_empty())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoInfo {
    pub codec: String,
    pub profile: Option<String>,
    pub pixel_format: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Luma bit depth (8, 10, 12); 0 when unknown.
    pub bit_depth: u32,
    pub fps: f64,
    /// FFmpeg stereo3d type name, e.g. "side by side", "top and bottom".
    pub stereo_mode: Option<String>,
    pub stereo_inverted: bool,
    /// FFmpeg spherical projection name, e.g. "equirectangular", "fisheye".
    pub projection: Option<String>,
    /// Horizontal coverage implied by equirectangular bounds, in degrees.
    pub horizontal_degrees: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioInfo {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaInfo {
    pub container: String,
    pub duration_seconds: f64,
    pub bit_rate: i64,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    pub subtitles: Vec<SubtitleTrackInfo>,
    pub audio_tracks: Vec<AudioTrackInfo>,
    /// The audio track played at first (index into `audio_tracks`).
    pub audio_track: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecodeStats {
    pub frames: u32,
    pub hardware_frames: u32,
    pub software_frames: u32,
    pub elapsed_seconds: f64,
    pub frames_per_second: f64,
    pub decoder: String,
    pub hw_backend: Option<String>,
    pub output_pixel_format: Option<String>,
    /// Why hardware decoding was skipped, if it was.
    pub note: Option<String>,
    pub error: Option<String>,
}

pub struct Media {
    raw: *mut RawMedia,
    // Boxed twice so the pointer handed to C stays valid while `Media` moves.
    _source: Box<BoxedSource>,
    info: MediaInfo,
    io: std::sync::Arc<std::sync::Mutex<IoCount>>,
}

// SAFETY: the FFmpeg contexts are only touched through &mut self.
unsafe impl Send for Media {}

impl Media {
    pub fn open(name: &str, source: impl Source + 'static) -> anyhow::Result<Self> {
        let io = std::sync::Arc::new(std::sync::Mutex::new(IoCount::default()));
        let source = Counted {
            inner: source,
            count: io.clone(),
            position: 0,
            expected: None,
        };
        let mut source: Box<BoxedSource> = Box::new(Box::new(source));
        let name = CString::new(name.replace('\0', ""))?;
        let mut raw_info = std::mem::MaybeUninit::<RawInfo>::zeroed();
        let mut error = [0 as c_char; 256];
        // SAFETY: all pointers are valid for the call; `source` outlives `raw`.
        let raw = unsafe {
            jv_media_open(
                name.as_ptr(),
                read_cb,
                seek_cb,
                (&mut *source) as *mut BoxedSource as *mut c_void,
                raw_info.as_mut_ptr(),
                error.as_mut_ptr(),
                error.len() as c_int,
            )
        };
        if raw.is_null() {
            bail!("{}", text(&error));
        }
        let r = unsafe { raw_info.assume_init() };
        let video = optional(&r.video_codec).map(|codec| VideoInfo {
            codec,
            profile: optional(&r.video_profile),
            pixel_format: optional(&r.pixel_format),
            width: r.width.max(0) as u32,
            height: r.height.max(0) as u32,
            bit_depth: r.bit_depth.max(0) as u32,
            fps: r.fps,
            stereo_mode: optional(&r.stereo_mode),
            stereo_inverted: r.stereo_inverted != 0,
            projection: optional(&r.projection),
            horizontal_degrees: optional(&r.projection).map(|_| {
                let covered = 1.0 - (r.bound_left as f64 + r.bound_right as f64) / 4294967296.0;
                (covered * 360.0).clamp(0.0, 360.0)
            }),
        });
        let audio = optional(&r.audio_codec).map(|codec| AudioInfo {
            codec,
            channels: r.audio_channels.max(0) as u32,
            sample_rate: r.audio_sample_rate.max(0) as u32,
        });
        let count = unsafe { jv_media_subtitle_count(raw) };
        let subtitles = (0..count)
            .filter_map(|i| {
                let mut t = std::mem::MaybeUninit::<RawSubtitleTrack>::zeroed();
                // SAFETY: `raw` is open and `i` is in range.
                (unsafe { jv_media_subtitle_track(raw, i, t.as_mut_ptr()) } == 0).then(|| {
                    let t = unsafe { t.assume_init() };
                    SubtitleTrackInfo {
                        codec: text(&t.codec),
                        language: optional(&t.language),
                        title: optional(&t.title),
                        default: t.is_default != 0,
                        forced: t.forced != 0,
                        supported: t.supported != 0,
                    }
                })
            })
            .collect();
        let audio_count = unsafe { jv_media_audio_count(raw) };
        let audio_tracks = (0..audio_count)
            .filter_map(|i| {
                let mut t = std::mem::MaybeUninit::<RawAudioTrack>::zeroed();
                // SAFETY: `raw` is open and `i` is in range.
                (unsafe { jv_media_audio_track(raw, i, t.as_mut_ptr()) } == 0).then(|| {
                    let t = unsafe { t.assume_init() };
                    AudioTrackInfo {
                        codec: text(&t.codec),
                        language: optional(&t.language),
                        title: optional(&t.title),
                        channels: t.channels.max(0) as u32,
                        default: t.is_default != 0,
                    }
                })
            })
            .collect();
        let audio_track = usize::try_from(unsafe { jv_media_current_audio(raw) }).ok();
        Ok(Self {
            raw,
            _source: source,
            io,
            info: MediaInfo {
                container: text(&r.container),
                duration_seconds: r.duration_seconds,
                bit_rate: r.bit_rate,
                video,
                audio,
                subtitles,
                audio_tracks,
                audio_track,
            },
        })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// Everything the demuxer has read so far.
    pub fn io(&self) -> IoCount {
        *self.io.lock().expect("io count")
    }

    /// Decodes up to `frames` video frames from the start. `hw_backend` is an
    /// FFmpeg device type ("vulkan", "vaapi"), "v4l2m2m", or `None` for software.
    pub fn decode(
        &mut self,
        hw_backend: Option<&str>,
        allow_software: bool,
        decoder_options: &str,
        frames: u32,
    ) -> anyhow::Result<DecodeStats> {
        let backend = hw_backend.map(CString::new).transpose()?;
        let options = CString::new(decoder_options)?;
        let mut raw = std::mem::MaybeUninit::<RawStats>::zeroed();
        // SAFETY: `self.raw` is live; the stats pointer is valid for the call.
        unsafe {
            jv_media_decode(
                self.raw,
                backend.as_ref().map_or(std::ptr::null(), |b| b.as_ptr()),
                allow_software as c_int,
                options.as_ptr(),
                frames.min(i32::MAX as u32) as c_int,
                raw.as_mut_ptr(),
            );
        }
        let s = unsafe { raw.assume_init() };
        Ok(DecodeStats {
            frames: s.frames.max(0) as u32,
            hardware_frames: s.hardware_frames.max(0) as u32,
            software_frames: s.software_frames.max(0) as u32,
            elapsed_seconds: s.elapsed_seconds,
            frames_per_second: if s.elapsed_seconds > 0.0 {
                s.frames as f64 / s.elapsed_seconds
            } else {
                0.0
            },
            decoder: text(&s.decoder),
            hw_backend: optional(&s.hw_backend),
            output_pixel_format: optional(&s.pixel_format),
            note: optional(&s.note),
            error: optional(&s.error),
        })
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        // SAFETY: closes FFmpeg before `_source` (declared later) is dropped.
        unsafe { jv_media_close(self.raw) };
    }
}

#[repr(C)]
struct RawDecoder {
    _private: [u8; 0],
}

#[repr(C)]
struct RawFrame {
    handle: *mut c_void,
    layout: i32,
    width: i32,
    height: i32,
    bits: i32,
    plane_count: i32,
    data: [*const u8; 3],
    linesize: [i32; 3],
    pts: f64,
    matrix: i32,
    full_range: i32,
    transfer: i32,
    hardware: i32,
}

unsafe extern "C" {
    fn jv_decoder_open(
        media: *mut RawMedia,
        hw_backend: *const c_char,
        allow_software: c_int,
        decoder_options: *const c_char,
        stats: *mut RawStats,
    ) -> *mut RawDecoder;
    fn jv_decoder_next(decoder: *mut RawDecoder, frame: *mut RawFrame) -> c_int;
    fn jv_decoder_seek(decoder: *mut RawDecoder, seconds: f64) -> c_int;
    fn jv_decoder_reopen_video(
        decoder: *mut RawDecoder,
        try_hardware: c_int,
        free_behind: c_int,
    ) -> c_int;
    fn jv_decoder_return_to_hardware(decoder: *mut RawDecoder) -> c_int;
    fn jv_decoder_skip_nonref_until(decoder: *mut RawDecoder, seconds: f64);
    fn jv_decoder_skip_to_keyframe_after(decoder: *mut RawDecoder, seconds: f64);
    fn jv_decoder_keyframe(decoder: *mut RawDecoder, seconds: f64, after: c_int) -> f64;
    fn jv_frame_release(handle: *mut c_void);
    fn jv_decoder_enable_audio(decoder: *mut RawDecoder, rate: c_int, channels: c_int) -> c_int;
    fn jv_decoder_audio_available(decoder: *const RawDecoder) -> c_int;
    fn jv_decoder_audio_read(
        decoder: *mut RawDecoder,
        out: *mut f32,
        frames: c_int,
        pts: *mut f64,
    ) -> c_int;
    fn jv_decoder_close(decoder: *mut RawDecoder);
    fn jv_media_subtitle_count(media: *const RawMedia) -> c_int;
    fn jv_media_audio_count(media: *const RawMedia) -> c_int;
    fn jv_media_audio_track(media: *const RawMedia, track: c_int, out: *mut RawAudioTrack)
    -> c_int;
    fn jv_media_current_audio(media: *const RawMedia) -> c_int;
    fn jv_decoder_select_audio(decoder: *mut RawDecoder, track: c_int) -> c_int;
    fn jv_media_subtitle_track(
        media: *const RawMedia,
        track: c_int,
        out: *mut RawSubtitleTrack,
    ) -> c_int;
    fn jv_decoder_select_subtitle(decoder: *mut RawDecoder, track: c_int) -> c_int;
    fn jv_free(pointer: *mut c_void);
    fn jv_decoder_subtitle_read(decoder: *mut RawDecoder, out: *mut RawCue) -> c_int;
}

const AVERROR_EOF: c_int = -0x20464F45; // FFERRTAG('E','O','F',' ')
const AVERROR_PATCHWELCOME: c_int = -0x45574150; // FFERRTAG('P','A','W','E')

/// How the planes of a decoded frame are arranged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneLayout {
    /// Y, U, V planes; 10-bit samples are LSB-aligned in 16 bits.
    Planar,
    /// Y plane + interleaved UV (NV12).
    SemiPlanar,
    /// Y + interleaved UV with 10 bits MSB-aligned in 16 bits (P010).
    SemiPlanarMsb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt709,
    Bt601,
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    Sdr,
    Pq,
    Hlg,
}

/// One decoded picture in CPU memory. Freed when dropped.
pub struct Frame {
    raw: RawFrame,
    /// Unique among the process's frames (see [`Frame::serial`]).
    serial: u64,
    /// The decoder's count of frames alive (see [`VideoDecoder::frames_alive`]).
    alive: Arc<AtomicUsize>,
}

// SAFETY: the frame's buffers are reference counted by FFmpeg and immutable
// once decoded; freeing from another thread is allowed.
unsafe impl Send for Frame {}

/// The next [`Frame::serial`].
static FRAME_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Frame {
    /// Tells this frame from any other, e.g. one already copied for upload.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    pub fn layout(&self) -> PlaneLayout {
        match self.raw.layout {
            0 => PlaneLayout::Planar,
            1 => PlaneLayout::SemiPlanar,
            _ => PlaneLayout::SemiPlanarMsb,
        }
    }

    pub fn width(&self) -> u32 {
        self.raw.width as u32
    }

    pub fn height(&self) -> u32 {
        self.raw.height as u32
    }

    /// 8 or 10.
    pub fn bits(&self) -> u32 {
        self.raw.bits as u32
    }

    /// Seconds from the start of the stream, if known.
    pub fn pts(&self) -> Option<f64> {
        (self.raw.pts >= 0.0).then_some(self.raw.pts)
    }

    pub fn matrix(&self) -> Matrix {
        match self.raw.matrix {
            1 => Matrix::Bt601,
            2 => Matrix::Bt2020,
            _ => Matrix::Bt709,
        }
    }

    pub fn full_range(&self) -> bool {
        self.raw.full_range != 0
    }

    pub fn transfer(&self) -> Transfer {
        match self.raw.transfer {
            1 => Transfer::Pq,
            2 => Transfer::Hlg,
            _ => Transfer::Sdr,
        }
    }

    pub fn hardware(&self) -> bool {
        self.raw.hardware != 0
    }

    /// Bytes per sample: 1 for 8-bit, 2 for 10-bit.
    pub fn bytes_per_sample(&self) -> usize {
        if self.raw.bits > 8 { 2 } else { 1 }
    }

    /// Plane dimensions in texels: (width, height, components per texel).
    pub fn plane_size(&self, plane: usize) -> (u32, u32, u32) {
        let (w, h) = (self.width(), self.height());
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        match (self.layout(), plane) {
            (_, 0) => (w, h, 1),
            (PlaneLayout::Planar, _) => (cw, ch, 1),
            _ => (cw, ch, 2),
        }
    }

    pub fn plane_count(&self) -> usize {
        self.raw.plane_count as usize
    }

    /// Row slices of one plane (without padding), top to bottom.
    pub fn rows(&self, plane: usize) -> impl Iterator<Item = &[u8]> {
        let (w, h, c) = self.plane_size(plane);
        let row_bytes = (w * c) as usize * self.bytes_per_sample();
        let stride = self.raw.linesize[plane] as isize;
        let base = self.raw.data[plane];
        (0..h as isize).map(move |y| {
            // SAFETY: FFmpeg guarantees `height` rows of `linesize` bytes, each
            // holding at least `row_bytes` of samples, alive until release.
            unsafe { std::slice::from_raw_parts(base.offset(y * stride), row_bytes) }
        })
    }
}

/// Pictures larger than this are copied by several threads: one core copies
/// 8K (44 MB) in ~2.5 ms on the Frame, with spikes to 9 ms.
const PARALLEL_COPY_BYTES: usize = 16 << 20;
/// Threads copying a large picture, the caller's included.
const COPY_THREADS: usize = 4;

impl Frame {
    /// Bytes `copy_to` writes: every plane's rows, packed.
    pub fn packed_len(&self) -> usize {
        (0..self.plane_count())
            .map(|p| self.rows(p).map(<[u8]>::len).sum::<usize>())
            .sum()
    }

    /// Copies every plane's rows, packed one after another, to the start of
    /// `dst` (at least `packed_len` bytes); returns the bytes written.
    pub fn copy_to(&self, dst: &mut [u8]) -> usize {
        let rows: Vec<&[u8]> = (0..self.plane_count()).flat_map(|p| self.rows(p)).collect();
        copy_rows(&rows, dst)
    }
}

/// Copies `rows` packed into `dst`, large copies split over `COPY_THREADS`.
fn copy_rows(rows: &[&[u8]], dst: &mut [u8]) -> usize {
    let total: usize = rows.iter().map(|r| r.len()).sum();
    assert!(dst.len() >= total, "copy destination too small");
    let copy = |rows: &[&[u8]], dst: &mut [u8]| {
        let mut offset = 0;
        for row in rows {
            dst[offset..offset + row.len()].copy_from_slice(row);
            offset += row.len();
        }
    };
    if total < PARALLEL_COPY_BYTES {
        copy(rows, dst);
        return total;
    }
    std::thread::scope(|scope| {
        let per = rows.len().div_ceil(COPY_THREADS);
        let mut rest = &mut dst[..total];
        let mut parts = rows.chunks(per).peekable();
        while let Some(part) = parts.next() {
            let len: usize = part.iter().map(|r| r.len()).sum();
            let (mine, after) = std::mem::take(&mut rest).split_at_mut(len);
            rest = after;
            if parts.peek().is_some() {
                scope.spawn(move || copy(part, mine));
            } else {
                copy(part, mine);
            }
        }
    });
    total
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { jv_frame_release(self.raw.handle) };
        self.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A video decoder owning its media; pull frames with [`VideoDecoder::next_frame`].
pub struct VideoDecoder {
    raw: *mut RawDecoder,
    media: Media,
    stats: DecodeStats,
    /// When the viewer asked for this video (for the time-to-first-frame log).
    pub requested_at: Option<std::time::Instant>,
    /// Holds the hardware decoder (see `HARDWARE_DECODERS`).
    hardware: bool,
    /// Frames handed out and not yet dropped. A V4L2 decoder session stays
    /// open until all its frames are gone.
    alive: Arc<AtomicUsize>,
}

// SAFETY: used from one thread at a time (the decode thread).
unsafe impl Send for VideoDecoder {}

impl Media {
    /// Opens the video decoder; same hardware-first and fallback rules as [`Media::decode`].
    pub fn into_decoder(
        self,
        hw_backend: Option<&str>,
        allow_software: bool,
        decoder_options: &str,
    ) -> anyhow::Result<VideoDecoder> {
        let backend = hw_backend.map(CString::new).transpose()?;
        let options = CString::new(decoder_options)?;
        let mut raw = std::mem::MaybeUninit::<RawStats>::zeroed();
        let decoder = unsafe {
            jv_decoder_open(
                self.raw,
                backend.as_ref().map_or(std::ptr::null(), |b| b.as_ptr()),
                allow_software as c_int,
                options.as_ptr(),
                raw.as_mut_ptr(),
            )
        };
        let s = unsafe { raw.assume_init() };
        if decoder.is_null() {
            bail!("{}", text(&s.error));
        }
        OPEN_DECODERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let hardware = optional(&s.hw_backend).is_some();
        if hardware {
            HARDWARE_DECODERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Counted by the first jump (see `VideoDecoder::can_free_behind`).
            other_decoder_sessions();
        }
        Ok(VideoDecoder {
            hardware,
            alive: Arc::default(),
            raw: decoder,
            media: self,
            stats: DecodeStats {
                frames: 0,
                hardware_frames: 0,
                software_frames: 0,
                elapsed_seconds: 0.0,
                frames_per_second: 0.0,
                decoder: text(&s.decoder),
                hw_backend: optional(&s.hw_backend),
                output_pixel_format: None,
                note: optional(&s.note),
                error: None,
            },
            requested_at: None,
        })
    }
}

impl VideoDecoder {
    pub fn info(&self) -> &MediaInfo {
        self.media.info()
    }

    /// Everything the demuxer has read so far.
    pub fn io(&self) -> IoCount {
        self.media.io()
    }

    /// The keyframe in the file's index at or before (or with `after`, at or
    /// after) `seconds`. None when the index doesn't say (e.g. Matroska
    /// before its first seek).
    pub fn keyframe(&mut self, seconds: f64, after: bool) -> Option<f64> {
        let t = unsafe { jv_decoder_keyframe(self.raw, seconds.max(0.0), after as c_int) };
        (t >= 0.0).then_some(t)
    }

    /// Until a packet reaches `seconds`, skip decoding frames no other frame
    /// refers to: after a seek they are only decoded to be thrown away.
    /// Hardware (V4L2) decoders ignore this.
    pub fn skip_nonref_until(&mut self, seconds: f64) {
        unsafe { jv_decoder_skip_nonref_until(self.raw, seconds) }
    }

    /// Decodes only keyframes until one at or after `seconds`: for catching
    /// up when decoding has fallen far behind. Hardware (V4L2) decoders
    /// ignore this.
    pub fn skip_to_keyframe_after(&mut self, seconds: f64) {
        unsafe { jv_decoder_skip_to_keyframe_after(self.raw, seconds) }
    }

    /// Decoder name, backend and fallback note (counters are not updated).
    pub fn stats(&self) -> &DecodeStats {
        &self.stats
    }

    /// The next frame, or `None` at the end of the stream.
    pub fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
        let mut raw = std::mem::MaybeUninit::<RawFrame>::zeroed();
        match unsafe { jv_decoder_next(self.raw, raw.as_mut_ptr()) } {
            0 => {
                self.alive.fetch_add(1, Ordering::SeqCst);
                Ok(Some(Frame {
                    raw: unsafe { raw.assume_init() },
                    serial: FRAME_SERIAL.fetch_add(1, Ordering::Relaxed),
                    alive: self.alive.clone(),
                }))
            }
            AVERROR_EOF => Ok(None),
            AVERROR_PATCHWELCOME => bail!("Decoder produced an unsupported pixel format"),
            code => bail!("Decoding failed (FFmpeg error {code})"),
        }
    }

    /// Also decodes the audio track as interleaved f32 at `rate` Hz, `channels`
    /// channels. Returns false when the file has no playable audio.
    pub fn enable_audio(&mut self, rate: u32, channels: u32) -> bool {
        unsafe { jv_decoder_enable_audio(self.raw, rate as c_int, channels as c_int) == 0 }
    }

    /// Takes the audio decoded so far (read alongside video frames), with the
    /// time of its first sample in seconds from the start of the video.
    pub fn take_audio(&mut self, channels: u32) -> Option<(Vec<f32>, Option<f64>)> {
        let frames = unsafe { jv_decoder_audio_available(self.raw) };
        if frames <= 0 {
            return None;
        }
        let mut samples = vec![0f32; frames as usize * channels as usize];
        let mut pts = -1.0;
        let n = unsafe { jv_decoder_audio_read(self.raw, samples.as_mut_ptr(), frames, &mut pts) };
        samples.truncate(n.max(0) as usize * channels as usize);
        Some((samples, (pts >= 0.0).then_some(pts)))
    }

    /// Decodes subtitle track `track` (index into `info().subtitles`) alongside
    /// the video, or none. False if it can't be decoded.
    pub fn select_subtitle(&mut self, track: Option<usize>) -> bool {
        let track = track.map_or(-1, |t| t as c_int);
        unsafe { jv_decoder_select_subtitle(self.raw, track) == 0 }
    }

    /// Plays audio track `track` (index into `info().audio_tracks`) instead;
    /// seek afterwards. False if it can't be decoded (the old one stays).
    pub fn select_audio(&mut self, track: usize) -> bool {
        unsafe { jv_decoder_select_audio(self.raw, track as c_int) == 0 }
    }

    /// Subtitle cues decoded since the last call.
    pub fn take_subtitles(&mut self) -> Vec<crate::subtitles::Cue> {
        let mut cues = Vec::new();
        let mut raw = std::mem::MaybeUninit::<RawCue>::zeroed();
        // SAFETY: `raw` is writable; the decoder is open.
        while unsafe { jv_decoder_subtitle_read(self.raw, raw.as_mut_ptr()) } == 1 {
            let cue = unsafe { raw.assume_init_ref() };
            // Take the picture (we own it now), then free the C copy.
            let image = (!cue.rgba.is_null() && cue.width > 0 && cue.height > 0).then(|| {
                let len = cue.width as usize * cue.height as usize * 4;
                // SAFETY: C allocated `width * height * 4` bytes at `rgba`.
                let rgba = unsafe { std::slice::from_raw_parts(cue.rgba, len) }.to_vec();
                std::sync::Arc::new(crate::subtitles::Bitmap {
                    rgba,
                    width: cue.width as u32,
                    height: cue.height as u32,
                    x: cue.x,
                    y: cue.y,
                    frame_width: cue.frame_width.max(1) as u32,
                    frame_height: cue.frame_height.max(1) as u32,
                })
            });
            if !cue.rgba.is_null() {
                unsafe { jv_free(cue.rgba as *mut c_void) };
            }
            let text = crate::subtitles::clean_markup(&text(&cue.text));
            if cue.clear != 0 || !text.is_empty() || image.is_some() {
                // A cue with neither text nor picture erases (see `Cue`).
                cues.push(crate::subtitles::Cue {
                    start: cue.start,
                    end: cue.end,
                    text,
                    image,
                });
            }
        }
        cues
    }

    /// Jumps to the keyframe at or before `seconds`; frames before the target
    /// still arrive and should be skipped by the caller.
    pub fn seek(&mut self, seconds: f64) -> anyhow::Result<()> {
        match unsafe { jv_decoder_seek(self.raw, seconds.max(0.0)) } {
            0 => Ok(()),
            code => bail!("Seek failed (FFmpeg error {code})"),
        }
    }

    /// Whether a jump needs a new decoder: the hardware one can't be flushed.
    pub fn flush_unreliable(&self) -> bool {
        self.hardware && reopen_to_seek(self.info().video.as_ref())
    }

    /// Frames from this decoder still alive anywhere.
    pub fn frames_alive(&self) -> usize {
        self.alive.load(Ordering::SeqCst)
    }

    /// Replaces the hardware video decoder with a new one, or with software
    /// decoding if the device won't open (or at once without
    /// `try_hardware`). The old session only closes once [`Self::frames_alive`]
    /// is 0. With `free_behind`, it's freed on a thread while the new one
    /// opens (see [`Self::can_free_behind`]). Seek afterwards.
    pub fn reopen_video(&mut self, try_hardware: bool, free_behind: bool) -> anyhow::Result<()> {
        let code = unsafe {
            jv_decoder_reopen_video(self.raw, try_hardware as c_int, free_behind as c_int)
        };
        if code < 0 {
            bail!("Reopening the decoder failed (FFmpeg error {code})");
        }
        if code == 0 && self.hardware {
            self.hardware = false;
            HARDWARE_DECODERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.stats.hw_backend = None;
            self.stats.note = Some("Hardware decoder failed; decoding on the CPU".into());
        }
        Ok(())
    }

    /// Whether a new hardware session can start while this one is still
    /// being freed (~50 ms saved at each jump that replaces the decoder). The
    /// iris driver counts every open session as the one starting, a new one
    /// at 30 fps: with Steam's sessions and both of ours, it must still fit.
    pub fn can_free_behind(&self) -> bool {
        let Some(video) = self.info().video.as_ref().filter(|_| self.hardware) else {
            return false;
        };
        let Some(others) = other_decoder_sessions() else {
            return false;
        };
        let mbpf = video.width.div_ceil(16) as u64 * video.height.div_ceil(16) as u64;
        let sessions = others as u64 + 2;
        sessions * mbpf <= IRIS_MAX_MBPF && sessions * mbpf * 30 <= IRIS_MAX_MBPS
    }

    /// Decoding on the CPU only because the hardware decoder failed (to open,
    /// or at a jump), so [`Self::return_to_hardware`] may work later.
    pub fn hardware_lost(&self) -> bool {
        !self.hardware
            && self
                .stats
                .note
                .as_deref()
                .is_some_and(|n| n.starts_with("Hardware decoder failed"))
    }

    /// Moves decoding back to the hardware decoder after [`Self::hardware_lost`],
    /// if its device opens again; on failure the CPU decoder carries on
    /// untouched. Seek afterwards.
    pub fn return_to_hardware(&mut self) -> anyhow::Result<()> {
        // It has buffers for one 8K stream: another video may still hold it.
        if HARDWARE_DECODERS.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            bail!("another video holds the hardware decoder");
        }
        let code = unsafe { jv_decoder_return_to_hardware(self.raw) };
        if code < 0 {
            bail!("FFmpeg error {code}");
        }
        self.hardware = true;
        HARDWARE_DECODERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.stats.hw_backend = Some("v4l2m2m".into());
        self.stats.note = None;
        Ok(())
    }
}

/// The iris core's limits: macroblocks (16x16) per frame and per second.
const IRIS_MAX_MBPF: u64 = 278_528;
const IRIS_MAX_MBPS: u64 = 7_833_600;

/// Sessions other processes hold on the headset's hardware decoder (Steam's
/// web helper keeps one or two), or `None` without one or before the first
/// count. Counting reads every process's open files (~4000, several ms), so
/// it's done on a thread, at most every `SESSIONS_STALE`; this returns the
/// last count at once.
fn other_decoder_sessions() -> Option<usize> {
    static COUNT: std::sync::Mutex<(Option<usize>, Option<std::time::Instant>, bool)> =
        std::sync::Mutex::new((None, None, false));
    const SESSIONS_STALE: std::time::Duration = std::time::Duration::from_secs(5);
    let mut count = COUNT.lock().expect("session count");
    let (last, counted_at, counting) = *count;
    if !counting && counted_at.is_none_or(|at| at.elapsed() >= SESSIONS_STALE) {
        count.2 = true;
        std::thread::spawn(|| {
            let sessions = count_other_decoder_sessions();
            *COUNT.lock().expect("session count") =
                (sessions, Some(std::time::Instant::now()), false);
        });
    }
    last
}

fn count_other_decoder_sessions() -> Option<usize> {
    let device = std::fs::canonicalize("/dev/video-dec0").ok()?;
    let me = std::process::id().to_string();
    let mut count = 0;
    for process in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = process.file_name();
        let name = name.to_string_lossy();
        if name == me || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        count += fds
            .flatten()
            .filter(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == device))
            .count();
    }
    Some(count)
}

/// Whether the headset's hardware decoder looks usable: its firmware isn't
/// down (`CORE_DEINIT` after a crash, until `iris-driver-rebind` restarts the
/// driver, ~15 s; the file is gone meanwhile). True where there's no such file.
pub fn hardware_decoder_ready() -> bool {
    const IRIS: &str = "/sys/bus/platform/drivers/qcom-iris";
    if !std::path::Path::new(IRIS).exists() {
        return true;
    }
    std::fs::read_to_string(format!("{IRIS}/aa00000.video-codec/core_state"))
        .is_ok_and(|state| state.trim() != "CORE_DEINIT")
}

/// The headset's hardware H.264 decoder (iris) stops for good after a jump
/// flushes it: it returns one empty picture and never takes the next packet.
/// A new decoder costs 14-26 ms at 1080p, like opening a video at a time.
/// HEVC flushes fine, while the driver has memory to spare (VP9 untested).
fn reopen_to_seek(video: Option<&VideoInfo>) -> bool {
    video.is_some_and(|v| v.codec == "h264")
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        let started = std::time::Instant::now();
        unsafe { jv_decoder_close(self.raw) };
        if self.hardware {
            eprintln!(
                "Timing: hardware decoder closed in {:.0} ms",
                started.elapsed().as_secs_f64() * 1e3
            );
            HARDWARE_DECODERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        OPEN_DECODERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Video decoders not yet closed (something is playing or about to).
static OPEN_DECODERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Of those, on the hardware decoder. It has buffers for only one 8K stream:
/// opening a new one before the last closed falls back to the CPU.
static HARDWARE_DECODERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn open_decoders() -> usize {
    OPEN_DECODERS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Waits (up to `limit`) until every earlier hardware decoder has closed.
/// A closing software decoder holds nothing a new video needs.
pub fn wait_for_decoders_closed(limit: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    while HARDWARE_DECODERS.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        if started.elapsed() > limit {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

#[cfg(test)]
mod seek_tests {
    use super::*;

    #[test]
    fn rows_copy_packed_alone_or_in_parallel() {
        for (rows, width) in [(3usize, 5usize), (1200, 16 * 1024)] {
            let data: Vec<Vec<u8>> = (0..rows)
                .map(|r| (0..width).map(|i| ((r * 7 + i) % 251) as u8).collect())
                .collect();
            let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
            let mut dst = vec![0u8; rows * width + 9];
            assert_eq!(copy_rows(&refs, &mut dst), rows * width);
            assert_eq!(&dst[..rows * width], data.concat().as_slice());
            assert!(dst[rows * width..].iter().all(|&b| b == 0));
        }
    }

    fn info(codec: &str) -> Option<VideoInfo> {
        Some(VideoInfo {
            codec: codec.into(),
            profile: None,
            pixel_format: Some("yuv420p".into()),
            width: 1920,
            height: 1080,
            bit_depth: 8,
            fps: 30.0,
            stereo_mode: None,
            stereo_inverted: false,
            projection: None,
            horizontal_degrees: None,
        })
    }

    #[test]
    fn only_h264_gets_a_new_hardware_decoder_at_a_jump() {
        assert!(reopen_to_seek(info("h264").as_ref()));
        assert!(!reopen_to_seek(info("hevc").as_ref()));
        assert!(!reopen_to_seek(info("vp9").as_ref()));
        assert!(!reopen_to_seek(None));
    }
}
