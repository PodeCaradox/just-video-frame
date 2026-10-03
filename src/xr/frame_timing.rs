//! Where the frame loop's time goes while a video plays, logged every few
//! seconds as `Timing: frames …` (see the headset log).

use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub enum Phase {
    /// `xrWaitFrame`: waiting for the runtime's next frame.
    Wait,
    /// Input, view poses and picking the picture to show.
    Advance,
    /// Copying the new picture into the staging buffer.
    Copy,
    /// Recording the upload and draws, acquiring swapchain images.
    Record,
    /// Submitting to the GPU.
    Gpu,
    /// Panels, layers and `xrEndFrame`.
    End,
    /// Measured on the GPU (for the previous frame): the picture's upload…
    GpuUpload,
    /// …and drawing both eyes.
    GpuEyes,
}

const PHASES: usize = 8;
const NAMES: [&str; PHASES] = [
    "wait",
    "advance",
    "copy",
    "record",
    "gpu",
    "end",
    "gpu upload",
    "gpu eyes",
];
const REPORT_EVERY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct FrameTiming {
    since: Option<Instant>,
    frames: u64,
    total: [f64; PHASES],
    max: [f64; PHASES],
    /// Display periods skipped between frames (predicted times too far apart).
    missed: u64,
    last_display: Option<i64>,
    uploads: u64,
    /// Loop time per frame (all phases but the wait), worst case.
    busy_max: f64,
    busy: f64,
}

impl FrameTiming {
    /// One frame's time in `phase`.
    pub fn add(&mut self, phase: Phase, ms: f64) {
        let i = phase as usize;
        self.total[i] += ms;
        self.max[i] = self.max[i].max(ms);
        if !matches!(phase, Phase::Wait | Phase::GpuUpload | Phase::GpuEyes) {
            self.busy += ms;
        }
    }

    /// Ends a frame shown at `display` (ns) with `period` between displays.
    pub fn frame(&mut self, display: i64, period: i64, uploaded: bool) {
        self.since.get_or_insert_with(Instant::now);
        if let Some(last) = self.last_display
            && period > 0
        {
            let periods = ((display - last) as f64 / period as f64).round() as i64;
            self.missed += (periods - 1).max(0) as u64;
        }
        self.last_display = Some(display);
        self.frames += 1;
        self.uploads += uploaded as u64;
        self.busy_max = self.busy_max.max(self.busy);
        self.busy = 0.0;
        if self.since.is_some_and(|s| s.elapsed() >= REPORT_EVERY) {
            self.report(period);
        }
    }

    /// Logs and restarts the counts (nothing when no frames passed).
    pub fn report(&mut self, period: i64) {
        if self.frames == 0 {
            return;
        }
        let n = self.frames as f64;
        let phases: Vec<String> = (0..PHASES)
            .map(|i| format!("{} {:.1}/{:.1}", NAMES[i], self.total[i] / n, self.max[i]))
            .collect();
        let seconds = self.since.map_or(0.0, |s| s.elapsed().as_secs_f64());
        eprintln!(
            "Timing: frames {} in {:.1} s ({:.0} Hz), {} display periods missed, {} pictures \
             uploaded; ms mean/max: {}; loop work max {:.1} ms",
            self.frames,
            seconds,
            1e9 / period.max(1) as f64,
            self.missed,
            self.uploads,
            phases.join(", "),
            self.busy_max,
        );
        *self = Self {
            last_display: self.last_display,
            ..Self::default()
        };
    }

    /// Forgets the last display time (after a pause in rendering).
    pub fn restart(&mut self) {
        self.last_display = None;
    }
}
