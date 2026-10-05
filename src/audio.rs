//! Audio output through PulseAudio's simple API (served by PipeWire on
//! SteamOS). libpulse-simple is loaded at runtime, so building needs no audio
//! headers and a missing sound server only disables sound.

use anyhow::{Context, bail};
use std::ffi::{c_char, c_int, c_void};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_SAMPLE_FLOAT32LE: c_int = 5;

type NewFn = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type WriteFn = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
type LatencyFn = unsafe extern "C" fn(*mut c_void, *mut c_int) -> u64;
type FlushFn = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut c_void);

pub struct Output {
    _library: libloading::Library,
    stream: *mut c_void,
    write: WriteFn,
    latency: LatencyFn,
    flush: FlushFn,
    free: FreeFn,
}

// SAFETY: the stream is used from one thread at a time (the audio thread).
unsafe impl Send for Output {}

impl Output {
    pub fn open(name: &str) -> anyhow::Result<Self> {
        unsafe {
            let library = libloading::Library::new("libpulse-simple.so.0")
                .context("PulseAudio client library not found")?;
            let new: NewFn = *library.get(b"pa_simple_new\0")?;
            let write: WriteFn = *library.get(b"pa_simple_write\0")?;
            let latency: LatencyFn = *library.get(b"pa_simple_get_latency\0")?;
            let flush: FlushFn = *library.get(b"pa_simple_flush\0")?;
            let free: FreeFn = *library.get(b"pa_simple_free\0")?;
            let spec = SampleSpec {
                format: PA_SAMPLE_FLOAT32LE,
                rate: RATE,
                channels: CHANNELS as u8,
            };
            // ~60 ms target buffer: low enough for tight A/V sync, high enough
            // to survive a busy frame.
            let bytes_per_second = RATE * CHANNELS * 4;
            let attr = BufferAttr {
                maxlength: u32::MAX,
                tlength: bytes_per_second * 60 / 1000,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: u32::MAX,
            };
            let app = std::ffi::CString::new("Just Video")?;
            let stream_name = std::ffi::CString::new(name.replace('\0', ""))?;
            let mut error = 0;
            let stream = new(
                std::ptr::null(),
                app.as_ptr(),
                PA_STREAM_PLAYBACK,
                std::ptr::null(),
                stream_name.as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut error,
            );
            if stream.is_null() {
                bail!("Can't open audio output (PulseAudio error {error})");
            }
            Ok(Self {
                _library: library,
                stream,
                write,
                latency,
                flush,
                free,
            })
        }
    }

    /// Blocks until the samples are queued (this paces the audio thread).
    pub fn write(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        let mut error = 0;
        let ret = unsafe {
            (self.write)(
                self.stream,
                samples.as_ptr() as *const c_void,
                size_of_val(samples),
                &mut error,
            )
        };
        if ret < 0 {
            bail!("Audio write failed (PulseAudio error {error})");
        }
        Ok(())
    }

    /// Seconds between queued audio and what is heard now.
    pub fn latency(&mut self) -> f64 {
        let mut error = 0;
        let usec = unsafe { (self.latency)(self.stream, &mut error) };
        if usec == u64::MAX {
            0.0
        } else {
            usec as f64 / 1e6
        }
    }

    /// Drops queued audio (after a seek).
    pub fn flush(&mut self) {
        let mut error = 0;
        unsafe { (self.flush)(self.stream, &mut error) };
    }
}

/// Software volume. The simple API has no stream volume, so samples are
/// scaled here (the player plays as decoded; the headset's volume sets
/// loudness).
pub struct Gain;

impl Gain {
    /// `samples` (interleaved, [`CHANNELS`]) at `level` (0..=1, 1 = as
    /// decoded). The curve is perceptual: equal level steps sound evenly
    /// spaced.
    pub fn apply(&self, samples: &[f32], level: f32) -> Vec<f32> {
        let gain = level * level;
        samples.iter().map(|s| s * gain).collect()
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe { (self.free)(self.stream) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_level_and_below_is_a_plain_multiply() {
        let g = Gain;
        let samples = [0.5, -0.25, 1.2, -1.0];
        assert_eq!(g.apply(&samples, 1.0), samples.to_vec());
        assert_eq!(g.apply(&samples, 0.5), vec![0.125, -0.0625, 0.3, -0.25]);
    }
}
