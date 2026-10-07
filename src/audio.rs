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

/// Keeps the sound within full scale, beyond which the sound server cuts off
/// the tops of the waves (a harsh crackle). Surround tracks need this: mixed
/// down to stereo, each side gets its front channel plus 0.7 of the centre
/// and of each surround channel, so loud scenes in 5.1 and 7.1 films add up
/// to more than full scale. Sound within full scale passes unchanged; above
/// it, the level drops at once and comes back over a fraction of a second,
/// by the same factor for all channels so voices stay where they are.
pub struct Limiter {
    gain: f32,
    /// Per frame, the distance of `gain` from 1 shrinks by this factor.
    release: f32,
}

impl Limiter {
    /// Seconds for the level to come most of the way (1/e) back after a peak.
    const RELEASE_SECONDS: f32 = 0.3;

    pub fn new(rate: u32) -> Self {
        Self {
            gain: 1.0,
            release: (-1.0 / (Self::RELEASE_SECONDS * rate as f32)).exp(),
        }
    }

    /// Full level again: after a jump the next sound is unrelated.
    pub fn reset(&mut self) {
        self.gain = 1.0;
    }

    /// Limits `samples` (interleaved, `channels` per frame) in place.
    pub fn apply(&mut self, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_mut(channels.max(1)) {
            // A broken frame would play as noise (and turn the gain into NaN).
            if frame.iter().any(|s| !s.is_finite()) {
                frame.fill(0.0);
                continue;
            }
            let peak = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
            self.gain = 1.0 - (1.0 - self.gain) * self.release;
            if peak * self.gain > 1.0 {
                self.gain = 1.0 / peak;
            }
            if self.gain < 1.0 {
                frame.iter_mut().for_each(|s| *s *= self.gain);
            }
        }
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

    /// `seconds` of a 440 Hz tone at `amplitude` in the left channel and half
    /// that in the right.
    fn tone(amplitude: f32, seconds: f32) -> Vec<f32> {
        let frames = (RATE as f32 * seconds) as usize;
        let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
        (0..frames)
            .flat_map(|i| {
                let s = amplitude * (i as f32 * step).sin();
                [s, 0.5 * s]
            })
            .collect()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn limiter_leaves_sound_within_full_scale_alone() {
        let mut limiter = Limiter::new(RATE);
        let mut samples = tone(1.0, 0.5);
        samples.extend([1.0, -1.0, 0.0, 0.25]);
        let expected = samples.clone();
        limiter.apply(&mut samples, 2);
        assert_eq!(samples, expected);
    }

    #[test]
    fn limiter_keeps_loud_surround_mixes_within_full_scale() {
        // A 7.1 track mixed down in a loud scene: up to 1.6 times full scale.
        let mut limiter = Limiter::new(RATE);
        let input = tone(1.6, 1.0);
        let mut samples = input.clone();
        limiter.apply(&mut samples, 2);
        let out = peak(&samples);
        assert!(out <= 1.0 + 1e-6, "peak {out}");
        assert!(out > 0.99, "only as far down as needed: {out}");
        // Both channels by the same factor: the right stays at half the left.
        for (o, i) in samples.chunks(2).zip(input.chunks(2)) {
            assert!((o[1] - 0.5 * o[0]).abs() < 1e-6, "{o:?} from {i:?}");
            assert!(o[0].abs() <= i[0].abs());
        }
    }

    #[test]
    fn limiter_comes_back_after_a_peak() {
        let mut limiter = Limiter::new(RATE);
        let mut bang = [2.0, -2.0];
        limiter.apply(&mut bang, 2);
        assert_eq!(bang, [1.0, -1.0]);
        // Just after the peak, quieter sound is still turned down...
        let mut soon = tone(0.5, 0.05);
        limiter.apply(&mut soon, 2);
        assert!(peak(&soon) < 0.9 * 0.5, "{}", peak(&soon));
        // ...and two seconds later it is back at full level.
        let mut later = tone(0.5, 2.0);
        limiter.apply(&mut later, 2);
        let tail = &later[later.len() - RATE as usize / 10..];
        assert!(peak(tail) > 0.995 * 0.5, "{}", peak(tail));
        // After a jump it starts at full level at once.
        limiter.apply(&mut [2.0, 2.0], 2);
        limiter.reset();
        let mut fresh = tone(0.5, 0.05);
        let expected = fresh.clone();
        limiter.apply(&mut fresh, 2);
        assert_eq!(fresh, expected);
    }

    #[test]
    fn limiter_silences_broken_frames() {
        let mut limiter = Limiter::new(RATE);
        let mut samples = [f32::INFINITY, 0.5, f32::NAN, 0.25, 0.5, -0.5];
        limiter.apply(&mut samples, 2);
        assert_eq!(samples, [0.0, 0.0, 0.0, 0.0, 0.5, -0.5]);
    }
}
