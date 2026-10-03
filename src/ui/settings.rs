//! The Settings screen: one row per setting, showing its value on the
//! right; clicking a row steps to the next preset (easy with a controller).

use super::browser::{Icon, Row};
use crate::config::Preferences;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    ShortJump,
    LongJump,
    VolumeStep,
    Volume,
    Resume,
}

pub const ALL: [Setting; 5] = [
    Setting::ShortJump,
    Setting::LongJump,
    Setting::VolumeStep,
    Setting::Volume,
    Setting::Resume,
];

const SHORT_JUMPS: &[u32] = &[5, 10, 15, 30];
const LONG_JUMPS: &[u32] = &[30, 60, 120, 300, 600];
const VOLUME_STEPS: &[u32] = &[5, 10, 20];
const VOLUMES: &[u32] = &[50, 75, 100, 125, 150];

/// "5 s", "1 min", "1 min 30 s".
pub fn format_jump(seconds: u32) -> String {
    match (seconds / 60, seconds % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// The preset after `current`; a value that isn't a preset (edited by
/// hand) goes to the next one up, and past the last back to the first.
fn next(presets: &[u32], current: u32) -> u32 {
    presets
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(presets[0])
}

impl Setting {
    pub fn row(self, p: &Preferences) -> Row {
        let (label, detail, value) = match self {
            Setting::ShortJump => (
                "Jump",
                "D-pad left/right or a sideways stick flick while playing",
                format_jump(p.short_jump),
            ),
            Setting::LongJump => (
                "Long jump",
                "The same with the grip held",
                format_jump(p.long_jump),
            ),
            Setting::VolumeStep => (
                "Volume step",
                "D-pad up/down while playing; hold to repeat",
                format!("{} %", p.volume_step),
            ),
            Setting::Volume => (
                "Volume",
                "Above 100 % boosts quiet videos. The headset's buttons set the overall level",
                format!("{} %", p.volume),
            ),
            Setting::Resume => (
                "Continue where I left off",
                "Start videos from where they were stopped",
                if p.resume { "On" } else { "Off" }.to_string(),
            ),
        };
        Row {
            detail: detail.into(),
            right: value,
            ..Row::new(Icon::Slider, label)
        }
    }

    /// Steps the setting to its next value.
    pub fn cycle(self, p: &mut Preferences) {
        match self {
            Setting::ShortJump => p.short_jump = next(SHORT_JUMPS, p.short_jump),
            Setting::LongJump => p.long_jump = next(LONG_JUMPS, p.long_jump),
            Setting::VolumeStep => p.volume_step = next(VOLUME_STEPS, p.volume_step),
            Setting::Volume => p.volume = next(VOLUMES, p.volume),
            Setting::Resume => p.resume = !p.resume,
        }
    }
}

/// The rows of the Settings screen.
pub fn rows(p: &Preferences) -> Vec<Row> {
    ALL.iter().map(|s| s.row(p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cycle_and_wrap() {
        let mut p = Preferences::default();
        let jumps: Vec<u32> = (0..5)
            .map(|_| {
                Setting::ShortJump.cycle(&mut p);
                p.short_jump
            })
            .collect();
        assert_eq!(jumps, [10, 15, 30, 5, 10]);
        p.long_jump = 45; // not a preset
        Setting::LongJump.cycle(&mut p);
        assert_eq!(p.long_jump, 60);
        p.long_jump = 600;
        Setting::LongJump.cycle(&mut p);
        assert_eq!(p.long_jump, 30);
        Setting::Resume.cycle(&mut p);
        assert!(!p.resume);
        assert_eq!(Setting::Resume.row(&p).right, "Off");
    }

    #[test]
    fn jump_lengths_read_naturally() {
        assert_eq!(format_jump(5), "5 s");
        assert_eq!(format_jump(60), "1 min");
        assert_eq!(format_jump(600), "10 min");
        assert_eq!(format_jump(90), "1 min 30 s");
    }
}
