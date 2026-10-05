//! Safe wrapper over `native/media.c`: FFmpeg demux/decode over any `Read + Seek`.

use crate::vr::{Layout, Projection, Stereo};
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
    fn jv_decoder_first_picture(decoder: *mut RawDecoder);
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

    /// One plane as a slice from its first row to the end of its last, and
    /// the row stride in bytes.
    fn plane(&self, plane: usize) -> (&[u8], usize) {
        let (w, h, c) = self.plane_size(plane);
        let row_bytes = (w * c) as usize * self.bytes_per_sample();
        let stride = self.raw.linesize[plane].max(0) as usize;
        assert!(stride >= row_bytes, "negative or short stride");
        // SAFETY: as in `rows`; the rows are contiguous at `stride`.
        let data = unsafe {
            std::slice::from_raw_parts(self.raw.data[plane], stride * (h as usize - 1) + row_bytes)
        };
        (data, stride)
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
    /// Counts in `OPEN_DECODERS` (thumbnail decoders don't).
    counted: bool,
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
        self.open_decoder(hw_backend, allow_software, decoder_options, true)
    }

    /// `counted`: whether the decoder counts as playback (`open_decoders`).
    fn open_decoder(
        self,
        hw_backend: Option<&str>,
        allow_software: bool,
        decoder_options: &str,
        counted: bool,
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
        if counted {
            OPEN_DECODERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let hardware = optional(&s.hw_backend).is_some();
        if hardware {
            HARDWARE_DECODERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Counted by the first jump (see `VideoDecoder::can_free_behind`).
            other_decoder_sessions();
        }
        Ok(VideoDecoder {
            hardware,
            counted,
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
        if self.counted {
            OPEN_DECODERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
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

/// A small picture for file lists: sRGB-encoded RGBA, alpha 255.
#[derive(Clone, Debug)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A region of a frame, in fractions of its width and height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Crop {
    pub const FULL: Crop = Crop {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    /// The largest centred part of `self` (of a `width` x `height` frame)
    /// with the given width / height ratio.
    fn fit(self, width: u32, height: u32, aspect: f64) -> Crop {
        let (pw, ph) = (self.w * width as f64, self.h * height as f64);
        if pw / ph > aspect {
            let w = ph * aspect / width as f64;
            Crop {
                x: self.x + (self.w - w) / 2.0,
                w,
                ..self
            }
        } else {
            let h = pw / aspect / height as f64;
            Crop {
                y: self.y + (self.h - h) / 2.0,
                h,
                ..self
            }
        }
    }

    /// What to show of a `width` x `height` frame in an `out_w` x `out_h`
    /// thumbnail: the left eye of stereo video, and for VR180/360 and fisheye
    /// a 16:9 centre of it (the rest is a distorted panorama). Never stretches.
    pub fn for_layout(layout: &Layout, width: u32, height: u32, out_w: u32, out_h: u32) -> Crop {
        // Which half holds the left eye.
        let second = if layout.swap_eyes { 0.5 } else { 0.0 };
        let eye = match layout.stereo {
            Stereo::Mono => Crop::FULL,
            Stereo::SideBySide => Crop {
                x: second,
                w: 0.5,
                ..Crop::FULL
            },
            Stereo::TopBottom => Crop {
                y: second,
                h: 0.5,
                ..Crop::FULL
            },
        };
        let eye = if layout.projection == Projection::Flat {
            eye
        } else {
            eye.fit(width, height, 16.0 / 9.0)
        };
        eye.fit(width, height, out_w as f64 / out_h as f64)
    }
}

/// One plane's samples and its row stride in bytes.
#[derive(Clone, Copy)]
pub struct PlaneData<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

/// A decoded picture's planes, as [`Frame`] describes them.
pub struct YuvImage<'a> {
    pub width: u32,
    pub height: u32,
    pub layout: PlaneLayout,
    /// 8 or 10.
    pub bits: u32,
    pub matrix: Matrix,
    pub full_range: bool,
    pub y: PlaneData<'a>,
    /// U, or interleaved UV.
    pub u: PlaneData<'a>,
    /// V (unused when semi-planar).
    pub v: PlaneData<'a>,
}

/// Points sampled per output pixel along each axis, at most (8K sources
/// have 30 or more source pixels per thumbnail pixel).
const THUMB_GRID: usize = 4;

impl YuvImage<'_> {
    /// The sample at `index` (in samples) of a plane, as its code value.
    fn code(&self, plane: PlaneData, index: usize) -> u32 {
        if self.bits <= 8 {
            return plane.data[index] as u32;
        }
        let raw = u16::from_le_bytes([plane.data[2 * index], plane.data[2 * index + 1]]) as u32;
        // Planar is LSB-aligned; semi-planar (P010) MSB-aligned.
        if self.layout == PlaneLayout::Planar {
            raw
        } else {
            raw >> (16 - self.bits)
        }
    }

    /// R'G'B' (0 to 255) of the pixel at (`x`, `y`).
    fn rgb(&self, x: usize, y: usize, m: &Conversion) -> [f32; 3] {
        let bytes = if self.bits > 8 { 2 } else { 1 };
        let luma = self.code(self.y, y * self.y.stride / bytes + x);
        let (cx, cy) = (x / 2, y / 2);
        let (cb, cr) = if self.layout == PlaneLayout::Planar {
            let at = cy * self.u.stride / bytes + cx;
            let at_v = cy * self.v.stride / bytes + cx;
            (self.code(self.u, at), self.code(self.v, at_v))
        } else {
            let at = cy * self.u.stride / bytes + 2 * cx;
            (self.code(self.u, at), self.code(self.u, at + 1))
        };
        let y = (luma as f32 - m.y_offset) * m.y_scale;
        let cb = (cb as f32 - m.c_offset) * m.c_scale;
        let cr = (cr as f32 - m.c_offset) * m.c_scale;
        [
            y + m.r_cr * cr,
            y - m.g_cb * cb - m.g_cr * cr,
            y + m.b_cb * cb,
        ]
    }
}

/// Code values to R'G'B' in 0 to 255 (see `color_params` in the renderer).
struct Conversion {
    y_offset: f32,
    y_scale: f32,
    c_offset: f32,
    c_scale: f32,
    r_cr: f32,
    g_cb: f32,
    g_cr: f32,
    b_cb: f32,
}

impl Conversion {
    fn new(bits: u32, matrix: Matrix, full_range: bool) -> Self {
        let (kr, kb) = match matrix {
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let k = (1u32 << (bits - 8)) as f32;
        let max = ((1u32 << bits) - 1) as f32;
        let (y_offset, y_scale, c_scale) = if full_range {
            (0.0, 255.0 / max, 255.0 / max)
        } else {
            (16.0 * k, 255.0 / (219.0 * k), 255.0 / (224.0 * k))
        };
        Conversion {
            y_offset,
            y_scale,
            c_offset: 128.0 * k,
            c_scale,
            r_cr: 2.0 * (1.0 - kr),
            g_cb: 2.0 * kb * (1.0 - kb) / kg,
            g_cr: 2.0 * kr * (1.0 - kr) / kg,
            b_cb: 2.0 * (1.0 - kb),
        }
    }
}

/// Box-downscales `crop` of the picture to `out_w` x `out_h`. A fixed grid of
/// points in each output pixel's source cell stands in for every pixel in it.
/// Video is gamma-encoded already, so R'G'B' goes out as sRGB. HDR (PQ/HLG)
/// is not tone mapped and will look flat.
pub fn yuv_to_thumb(img: &YuvImage, crop: Crop, out_w: u32, out_h: u32) -> Thumb {
    let m = Conversion::new(img.bits, img.matrix, img.full_range);
    let (fw, fh) = (img.width as f64, img.height as f64);
    let (x0, y0) = (crop.x * fw, crop.y * fh);
    let (cell_w, cell_h) = (crop.w * fw / out_w as f64, crop.h * fh / out_h as f64);
    let nx = (cell_w.ceil() as usize).clamp(1, THUMB_GRID);
    let ny = (cell_h.ceil() as usize).clamp(1, THUMB_GRID);
    let (max_x, max_y) = (img.width as usize - 1, img.height as usize - 1);
    let xs: Vec<usize> = (0..out_w as usize * nx)
        .map(|i| ((x0 + (i as f64 + 0.5) * cell_w / nx as f64) as usize).min(max_x))
        .collect();
    let mut rgba = Vec::with_capacity(out_w as usize * out_h as usize * 4);
    for oy in 0..out_h as usize {
        for ox in 0..out_w as usize {
            let mut sum = [0.0f32; 3];
            for j in 0..ny {
                let y =
                    ((y0 + ((oy * ny + j) as f64 + 0.5) * cell_h / ny as f64) as usize).min(max_y);
                for &x in &xs[ox * nx..(ox + 1) * nx] {
                    let c = img.rgb(x, y, &m);
                    sum.iter_mut().zip(c).for_each(|(s, c)| *s += c);
                }
            }
            let n = (nx * ny) as f32;
            rgba.extend(sum.map(|s| (s / n + 0.5).clamp(0.0, 255.0) as u8));
            rgba.push(255);
        }
    }
    Thumb {
        width: out_w,
        height: out_h,
        rgba,
    }
}

/// [`yuv_to_thumb`] for a decoded frame.
pub fn frame_to_rgba(frame: &Frame, crop: Crop, out_w: u32, out_h: u32) -> Thumb {
    let plane = |i| {
        if i < frame.plane_count() {
            let (data, stride) = frame.plane(i);
            PlaneData { data, stride }
        } else {
            PlaneData {
                data: &[],
                stride: 0,
            }
        }
    };
    let img = YuvImage {
        width: frame.width(),
        height: frame.height(),
        layout: frame.layout(),
        bits: frame.bits(),
        matrix: frame.matrix(),
        full_range: frame.full_range(),
        y: plane(0),
        u: plane(1),
        v: plane(2),
    };
    yuv_to_thumb(&img, crop, out_w, out_h)
}

/// [`thumbnail_unless`] without a way out.
pub fn thumbnail(
    name: &str,
    source: impl Source + 'static,
    at_fraction: f64,
    layout: &Layout,
    out_w: u32,
    out_h: u32,
) -> anyhow::Result<Thumb> {
    thumbnail_unless(name, source, at_fraction, layout, out_w, out_h, &|| false)?
        .ok_or_else(|| anyhow::anyhow!("Thumbnail stopped"))
}

/// A thumbnail from the keyframe at or before `at_fraction` of the video (no
/// seek when the duration is unknown). Software decoding on two threads, and
/// the decoder doesn't count as playback (`open_decoders`). `stop` is asked
/// after opening, after the seek and after decoding; `Ok(None)` when it said so.
pub fn thumbnail_unless(
    name: &str,
    source: impl Source + 'static,
    at_fraction: f64,
    layout: &Layout,
    out_w: u32,
    out_h: u32,
    stop: &dyn Fn() -> bool,
) -> anyhow::Result<Option<Thumb>> {
    let media = Media::open(name, source)?;
    if stop() {
        return Ok(None);
    }
    let duration = media.info().duration_seconds;
    let mut decoder = media.open_decoder(None, true, "threads=2", false)?;
    if duration > 0.0 {
        // A file that can't seek gives its first picture.
        decoder.seek(duration * at_fraction.clamp(0.0, 1.0)).ok();
        if stop() {
            return Ok(None);
        }
    }
    unsafe { jv_decoder_first_picture(decoder.raw) };
    let Some(frame) = decoder.next_frame()? else {
        bail!("No picture to show");
    };
    if stop() {
        return Ok(None);
    }
    let crop = Crop::for_layout(layout, frame.width(), frame.height(), out_w, out_h);
    Ok(Some(frame_to_rgba(&frame, crop, out_w, out_h)))
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

#[cfg(test)]
mod thumb_tests {
    use super::*;
    use crate::vr::Evidence;

    fn plane(data: &[u8], stride: usize) -> PlaneData<'_> {
        PlaneData { data, stride }
    }

    fn layout(projection: Projection, stereo: Stereo, swap_eyes: bool) -> Layout {
        Layout {
            projection,
            stereo,
            swap_eyes,
            projection_from: Evidence::Default,
            stereo_from: Evidence::Default,
        }
    }

    /// A `w` x `h` picture of one colour, stored as `layout` with `bits`.
    fn solid(
        (w, h): (u32, u32),
        layout: PlaneLayout,
        bits: u32,
        matrix: Matrix,
        full_range: bool,
        (y, u, v): (u32, u32, u32),
    ) -> Thumb {
        let put = |out: &mut Vec<u8>, code: u32| {
            if bits <= 8 {
                out.push(code as u8);
            } else {
                let shifted = if layout == PlaneLayout::Planar {
                    code
                } else {
                    code << (16 - bits)
                };
                out.extend((shifted as u16).to_le_bytes());
            }
        };
        let (cw, ch) = ((w / 2) as usize, (h / 2) as usize);
        let (mut py, mut pu, mut pv) = (vec![], vec![], vec![]);
        (0..w * h).for_each(|_| put(&mut py, y));
        for _ in 0..cw * ch {
            put(&mut pu, u);
            if layout == PlaneLayout::Planar {
                put(&mut pv, v);
            } else {
                put(&mut pu, v);
            }
        }
        let bytes = if bits > 8 { 2 } else { 1 };
        let img = YuvImage {
            width: w,
            height: h,
            layout,
            bits,
            matrix,
            full_range,
            y: plane(&py, w as usize * bytes),
            u: plane(
                &pu,
                cw * bytes * if layout == PlaneLayout::Planar { 1 } else { 2 },
            ),
            v: plane(&pv, cw * bytes),
        };
        yuv_to_thumb(&img, Crop::FULL, 1, 1)
    }

    fn near(thumb: &Thumb, want: [u8; 3]) {
        assert_eq!(thumb.rgba[3], 255);
        for (got, want) in thumb.rgba[..3].iter().zip(want) {
            assert!(
                got.abs_diff(want) <= 2,
                "{:?} vs {want:?}",
                &thumb.rgba[..3]
            );
        }
    }

    const ALL: [(PlaneLayout, u32); 4] = [
        (PlaneLayout::Planar, 8),
        (PlaneLayout::SemiPlanar, 8),
        (PlaneLayout::Planar, 10),
        (PlaneLayout::SemiPlanarMsb, 10),
    ];

    #[test]
    fn limited_range_red_in_every_layout() {
        for (layout, bits) in ALL {
            let k = 1 << (bits - 8);
            // Studio-swing codes of pure red.
            let red = |c: (u32, u32, u32)| (c.0 * k, c.1 * k, c.2 * k);
            let t = solid(
                (4, 4),
                layout,
                bits,
                Matrix::Bt709,
                false,
                red((63, 102, 240)),
            );
            near(&t, [255, 0, 0]);
            let t = solid(
                (4, 4),
                layout,
                bits,
                Matrix::Bt601,
                false,
                red((81, 90, 240)),
            );
            near(&t, [255, 0, 0]);
            let t = solid(
                (4, 4),
                layout,
                bits,
                Matrix::Bt2020,
                false,
                red((74, 97, 240)),
            );
            near(&t, [255, 0, 0]);
        }
    }

    #[test]
    fn limited_range_black_and_white() {
        for (layout, bits) in ALL {
            let k = 1 << (bits - 8);
            let grey = |y: u32| (y * k, 128 * k, 128 * k);
            near(
                &solid((2, 2), layout, bits, Matrix::Bt709, false, grey(16)),
                [0, 0, 0],
            );
            near(
                &solid((2, 2), layout, bits, Matrix::Bt709, false, grey(235)),
                [255; 3],
            );
            // Below black and above white clamp.
            near(
                &solid((2, 2), layout, bits, Matrix::Bt709, false, grey(0)),
                [0, 0, 0],
            );
            near(
                &solid((2, 2), layout, bits, Matrix::Bt709, false, grey(255)),
                [255; 3],
            );
        }
    }

    #[test]
    fn full_range_red_and_grey() {
        for (layout, bits) in ALL {
            let k = 1 << (bits - 8);
            let max = (1 << bits) - 1;
            // Full-swing pure red: Y = 255 * Kr, Cb = 128 - 255 * Kr / (2 * (1 - Kb)), Cr = 255.
            let t = solid(
                (4, 4),
                layout,
                bits,
                Matrix::Bt709,
                true,
                (54 * k, 99 * k, max),
            );
            near(&t, [255, 0, 0]);
            let t = solid(
                (4, 4),
                layout,
                bits,
                Matrix::Bt601,
                true,
                (76 * k, 85 * k, max),
            );
            near(&t, [255, 0, 0]);
            // Mid grey: 128 of 255.
            let t = solid(
                (2, 2),
                layout,
                bits,
                Matrix::Bt709,
                true,
                (128 * k, 128 * k, 128 * k),
            );
            near(&t, [128; 3]);
            near(
                &solid(
                    (2, 2),
                    layout,
                    bits,
                    Matrix::Bt709,
                    true,
                    (max, 128 * k, 128 * k),
                ),
                [255; 3],
            );
        }
    }

    #[test]
    fn downscale_averages_cells_and_honours_stride() {
        // 8x4 luma, 4 bytes of padding per row: black left half, white right.
        let mut y = vec![99u8; 12 * 4];
        for row in 0..4 {
            for x in 0..8 {
                y[row * 12 + x] = if x < 4 { 16 } else { 235 };
            }
        }
        let chroma = vec![128u8; 4 * 2];
        let img = YuvImage {
            width: 8,
            height: 4,
            layout: PlaneLayout::Planar,
            bits: 8,
            matrix: Matrix::Bt709,
            full_range: false,
            y: plane(&y, 12),
            u: plane(&chroma, 4),
            v: plane(&chroma, 4),
        };
        let t = yuv_to_thumb(&img, Crop::FULL, 2, 1);
        assert_eq!((t.width, t.height, t.rgba.len()), (2, 1, 8));
        near(
            &Thumb {
                rgba: t.rgba[..4].to_vec(),
                ..t.clone()
            },
            [0, 0, 0],
        );
        near(
            &Thumb {
                rgba: t.rgba[4..].to_vec(),
                ..t.clone()
            },
            [255; 3],
        );
        // One output pixel over both halves: the average, 50 % grey-ish.
        let mid = yuv_to_thumb(&img, Crop::FULL, 1, 1);
        near(&mid, [128, 128, 128]);
        // Cropping to the right half only.
        let right = Crop {
            x: 0.5,
            w: 0.5,
            ..Crop::FULL
        };
        near(&yuv_to_thumb(&img, right, 3, 3), [255; 3]);
    }

    fn close(a: Crop, b: Crop) {
        let ok = |x: f64, y: f64| (x - y).abs() < 1e-9;
        assert!(
            ok(a.x, b.x) && ok(a.y, b.y) && ok(a.w, b.w) && ok(a.h, b.h),
            "{a:?} vs {b:?}"
        );
    }

    #[test]
    fn flat_mono_is_centre_cropped_to_the_output_shape() {
        let flat = layout(Projection::Flat, Stereo::Mono, false);
        // 16:9 source into a (nearly) 16:9 slot: almost everything.
        let c = Crop::for_layout(&flat, 1920, 1080, 160, 90);
        close(c, Crop::FULL);
        // 21:9 source: crop the sides.
        let c = Crop::for_layout(&flat, 2520, 1080, 160, 90);
        close(
            c,
            Crop {
                x: (1.0 - 1920.0 / 2520.0) / 2.0,
                w: 1920.0 / 2520.0,
                ..Crop::FULL
            },
        );
        // 4:3 source: crop top and bottom.
        let c = Crop::for_layout(&flat, 1440, 1080, 160, 90);
        let h = 1440.0 * 9.0 / 16.0 / 1080.0;
        close(
            c,
            Crop {
                y: (1.0 - h) / 2.0,
                h,
                ..Crop::FULL
            },
        );
    }

    #[test]
    fn stereo_takes_the_left_eye() {
        // Flat 3D, 16:9 eyes in a 32:9 side-by-side frame.
        let sbs = layout(Projection::Flat, Stereo::SideBySide, false);
        close(
            Crop::for_layout(&sbs, 3840, 1080, 160, 90),
            Crop {
                w: 0.5,
                ..Crop::FULL
            },
        );
        let swapped = layout(Projection::Flat, Stereo::SideBySide, true);
        close(
            Crop::for_layout(&swapped, 3840, 1080, 160, 90),
            Crop {
                x: 0.5,
                w: 0.5,
                ..Crop::FULL
            },
        );
        let tb = layout(Projection::Flat, Stereo::TopBottom, false);
        close(
            Crop::for_layout(&tb, 1920, 2160, 160, 90),
            Crop {
                h: 0.5,
                ..Crop::FULL
            },
        );
        let swapped = layout(Projection::Flat, Stereo::TopBottom, true);
        close(
            Crop::for_layout(&swapped, 1920, 2160, 160, 90),
            Crop {
                y: 0.5,
                h: 0.5,
                ..Crop::FULL
            },
        );
    }

    #[test]
    fn vr_takes_a_16_9_centre_of_the_eye() {
        // VR180 side-by-side, square eyes of 4096 px.
        let vr = layout(Projection::Equirect180, Stereo::SideBySide, false);
        let c = Crop::for_layout(&vr, 8192, 4096, 160, 90);
        let h = 9.0 / 16.0;
        close(
            c,
            Crop {
                x: 0.0,
                y: (1.0 - h) / 2.0,
                w: 0.5,
                h,
            },
        );
        // Its pixel shape is 16:9.
        assert!((c.w * 8192.0 / (c.h * 4096.0) - 16.0 / 9.0).abs() < 1e-9);
        // VR360 top/bottom, 2:1 eyes with the left on the bottom (swapped):
        // the sides are cropped.
        let vr = layout(Projection::Equirect360, Stereo::TopBottom, true);
        let c = Crop::for_layout(&vr, 4096, 4096, 160, 90);
        let w = 8.0 / 9.0;
        close(
            c,
            Crop {
                x: (1.0 - w) / 2.0,
                y: 0.5,
                w,
                h: 0.5,
            },
        );
        // Mono fisheye 2:1: the centre, again 16:9.
        let fish = layout(Projection::Fisheye180, Stereo::Mono, false);
        let c = Crop::for_layout(&fish, 4000, 2000, 160, 90);
        assert!((c.w * 4000.0 / (c.h * 2000.0) - 16.0 / 9.0).abs() < 1e-9);
        assert!((c.x + c.w / 2.0 - 0.5).abs() < 1e-9 && (c.y + c.h / 2.0 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn output_has_the_requested_size() {
        let y = vec![100u8; 64 * 36];
        let c = vec![128u8; 32 * 18];
        let img = YuvImage {
            width: 64,
            height: 36,
            layout: PlaneLayout::Planar,
            bits: 8,
            matrix: Matrix::Bt709,
            full_range: true,
            y: plane(&y, 64),
            u: plane(&c, 32),
            v: plane(&c, 32),
        };
        for (w, h) in [(220, 124), (16, 9), (1, 1), (100, 10)] {
            let t = yuv_to_thumb(&img, Crop::FULL, w, h);
            assert_eq!((t.width, t.height), (w, h));
            assert_eq!(t.rgba.len(), (w * h * 4) as usize);
        }
    }
}
