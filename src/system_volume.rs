//! The headset's own (SteamOS) output volume: the level its volume buttons
//! and Steam's slider set. Changed with `wpctl` on the default sink, off the
//! frame loop: presses go to a worker thread, which merges a burst into one
//! change and reads the level back.

use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

const SINK: &str = "@DEFAULT_AUDIO_SINK@";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    /// Percent (0..=100) and whether the output is muted.
    Volume { percent: u32, muted: bool },
    /// `wpctl` is missing or failed.
    Unavailable,
}

pub struct SystemVolume {
    steps: mpsc::Sender<i32>,
    /// The level after the latest change, until taken.
    changed: Arc<Mutex<Option<Level>>>,
}

impl SystemVolume {
    pub fn new() -> Self {
        let (steps, rx) = mpsc::channel::<i32>();
        let changed = Arc::new(Mutex::new(None));
        let shared = changed.clone();
        std::thread::Builder::new()
            .name("system volume".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Presses that came in meanwhile (held D-pad): one change.
                    let percent = first + rx.try_iter().sum::<i32>();
                    let level = change(percent);
                    *shared.lock().expect("volume") = Some(level);
                }
            })
            .expect("spawn system volume thread");
        Self { steps, changed }
    }

    /// Raises (positive) or lowers the volume by `percent`.
    pub fn step(&self, percent: i32) {
        let _ = self.steps.send(percent);
    }

    /// The level once a change has been made (once per change).
    pub fn take_changed(&self) -> Option<Level> {
        self.changed.lock().expect("volume").take()
    }
}

impl Default for SystemVolume {
    fn default() -> Self {
        Self::new()
    }
}

fn change(percent: i32) -> Level {
    if percent != 0 {
        let amount = format!("{}%{}", percent.abs(), if percent > 0 { '+' } else { '-' });
        // `-l 1.0`: never past 100 % (wpctl would otherwise go up to 150 %).
        if !wpctl(&["set-volume", "-l", "1.0", SINK, &amount]) {
            return Level::Unavailable;
        }
        // Turning it up should be heard, even if it was muted.
        if percent > 0 {
            wpctl(&["set-mute", SINK, "0"]);
        }
    }
    current()
}

/// The default sink's level now.
pub fn current() -> Level {
    match Command::new("wpctl").args(["get-volume", SINK]).output() {
        Ok(out) if out.status.success() => {
            parse(&String::from_utf8_lossy(&out.stdout)).unwrap_or(Level::Unavailable)
        }
        Ok(out) => {
            eprintln!(
                "Volume: wpctl get-volume failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            Level::Unavailable
        }
        Err(e) => {
            eprintln!("Volume: can't run wpctl: {e}");
            Level::Unavailable
        }
    }
}

fn wpctl(args: &[&str]) -> bool {
    match Command::new("wpctl").args(args).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            eprintln!(
                "Volume: wpctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            eprintln!("Volume: can't run wpctl: {e}");
            false
        }
    }
}

/// `wpctl get-volume` output: "Volume: 0.45" or "Volume: 0.00 [MUTED]".
fn parse(text: &str) -> Option<Level> {
    let rest = text.trim().strip_prefix("Volume:")?.trim();
    let number = rest.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(Level::Volume {
        percent: (number * 100.0).round().clamp(0.0, 999.0) as u32,
        muted: rest.contains("[MUTED]"),
    })
}

impl Level {
    /// The notice shown after a change: "Volume 45 %", "Volume 0 % · muted".
    pub fn notice(self) -> String {
        match self {
            Level::Volume {
                percent,
                muted: false,
            } => format!("Volume {percent} %"),
            Level::Volume {
                percent,
                muted: true,
            } => format!("Volume {percent} % · muted"),
            Level::Unavailable => "Can't change the headset volume".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wpctl_levels() {
        assert_eq!(
            parse("Volume: 0.45\n"),
            Some(Level::Volume {
                percent: 45,
                muted: false
            })
        );
        assert_eq!(
            parse("Volume: 0.00 [MUTED]\n"),
            Some(Level::Volume {
                percent: 0,
                muted: true
            })
        );
        assert_eq!(parse("Volume: 1.00").unwrap().notice(), "Volume 100 %");
        assert_eq!(parse("nonsense"), None);
    }
}
