//! The playback control bar (opened by clicking the video) and its dialogs:
//!
//! ```text
//! [⏮][▶][⏭]                               [CC]    [▭]     [☀]
//!                                          English 180° 3D  Image
//! 12:34 / 1:32:10  ━━━━━━━━━━●──────────────────────────────────
//! ```
//!
//! CC: a click turns subtitles on/off, a long press opens the audio and
//! subtitle dialog (tracks, and a way into subtitle editing: the bar then
//! holds the size and position buttons while the screen and subtitle area
//! are outlined, with sample text). The screen button: a
//! click steps through the favourite formats, a long press opens the screen
//! dialog (every format, stars, curved screen, swap eyes). The image button
//! opens the image dialog (brightness, contrast, saturation, rotation).
//! Volume: D-pad up/down sets the headset's own volume (as its buttons do); the thumbstick click resets the
//! screen.

use super::canvas::{Canvas, Fonts, Rgb};
use crate::config::ImageAdjust;
use crate::vr::{Projection, Stereo};

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 264;
pub const DIALOG_WIDTH: u32 = 1200;
pub const DIALOG_HEIGHT: u32 = 640;

const BG: Rgb = [0x15, 0x17, 0x1c];
const BUTTON: Rgb = [0x26, 0x2b, 0x34];
const HOVER: Rgb = [0x35, 0x3d, 0x4a];
const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const FAINT: Rgb = [0x4a, 0x4f, 0x58];
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const TRACK: Rgb = [0x3a, 0x40, 0x4c];
const STAR: Rgb = [0xff, 0xcc, 0x00];

pub type Format = (Projection, Stereo);

/// Every format, as listed in the screen dialog.
pub const FORMATS: &[Format] = &[
    (Projection::Flat, Stereo::Mono),
    (Projection::Flat, Stereo::SideBySide),
    (Projection::Flat, Stereo::TopBottom),
    (Projection::Equirect180, Stereo::SideBySide),
    (Projection::Equirect180, Stereo::TopBottom),
    (Projection::Equirect180, Stereo::Mono),
    (Projection::Fisheye180, Stereo::SideBySide),
    (Projection::Equirect360, Stereo::Mono),
    (Projection::Equirect360, Stereo::TopBottom),
    (Projection::Equirect360, Stereo::SideBySide),
];

pub fn format_label((projection, stereo): Format) -> String {
    let shape = match projection {
        Projection::Flat => "Flat",
        Projection::Equirect180 => "180°",
        Projection::Equirect360 => "360°",
        Projection::Fisheye180 => "Fisheye",
    };
    let depth = match stereo {
        Stereo::Mono => "2D",
        Stereo::SideBySide => "3D SBS",
        Stereo::TopBottom => "3D TB",
    };
    format!("{shape} {depth}")
}

/// The favourite after `current` (the first one when `current` isn't a favourite).
pub fn next_favourite(current: Format, favourites: &[Format]) -> Option<Format> {
    let next = match favourites.iter().position(|f| *f == current) {
        Some(i) => (i + 1) % favourites.len(),
        None => 0,
    };
    favourites.get(next).copied()
}

/// A dialog shown above the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialog {
    Tracks,
    Screen,
    Image,
}

/// Steps of the image adjustments (the app clamps the results).
pub const BRIGHTNESS_STEP: f32 = 0.05;
pub const CONTRAST_STEP: f32 = 0.1;
pub const SATURATION_STEP: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Previous,
    PlayPause,
    Next,
    /// Fraction of the duration.
    Seek(f32),
    /// CC: click toggles subtitles, long press opens the track dialog.
    Captions,
    /// Click: next favourite format; long press: the screen dialog.
    Screen,
    /// Opens the image dialog.
    Image,
    // In the dialogs:
    AudioTrack(usize),
    /// A subtitle track, or None for off.
    SubtitleTrack(Option<usize>),
    /// The next page of a long subtitle list.
    MorePage,
    /// Subtitles smaller (-1) or larger (+1).
    CaptionSize(i8),
    /// Subtitles lower (-1) or higher (+1).
    CaptionMove(i8),
    /// Into subtitle editing (from the track dialog).
    CaptionEdit,
    /// Subtitle size and position back to the defaults.
    CaptionReset,
    /// Out of subtitle editing.
    CaptionDone,
    /// A format (index into [`FORMATS`]); long press stars it.
    Pick(usize),
    Curved,
    SwapEyes,
    Brightness(i8),
    Contrast(i8),
    Saturation(i8),
    /// Quarter turns clockwise.
    Rotate(u8),
    ResetImage,
    Close,
    Nothing,
}

impl Hit {
    /// Hits with a long-press action (the rest act on release).
    pub fn has_long_press(self) -> bool {
        matches!(self, Hit::Captions | Hit::Screen | Hit::Pick(_))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    pub has_previous: bool,
    pub has_next: bool,
    /// None when the curve toggle does not apply (VR180/360).
    pub curved: Option<bool>,
    pub format: Format,
    pub favourites: Vec<Format>,
    pub swap_eyes: bool,
    /// Names of the subtitle tracks (empty: none), and the one shown.
    pub subtitle_tracks: Vec<String>,
    pub subtitle: Option<usize>,
    /// Names of the audio tracks, and the one playing.
    pub audio_tracks: Vec<String>,
    pub audio: Option<usize>,
    /// Page of the subtitle list.
    pub list_page: usize,
    pub image: ImageAdjust,
    pub dialog: Option<Dialog>,
    /// Editing subtitle size and position: the bar holds only those buttons.
    pub caption_edit: bool,
}

type Rect = (f32, f32, f32, f32);

const PREVIOUS: Rect = (24.0, 16.0, 96.0, 92.0);
const PLAY: Rect = (136.0, 16.0, 96.0, 92.0);
const NEXT: Rect = (248.0, 16.0, 96.0, 92.0);
const CAPTIONS: Rect = (818.0, 16.0, 96.0, 92.0);
const SCREEN: Rect = (948.0, 16.0, 96.0, 92.0);
const IMAGE: Rect = (1078.0, 16.0, 96.0, 92.0);
const SEEK: Rect = (330.0, 164.0, 846.0, 80.0);

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

/// The bar's buttons while editing subtitles, with their labels.
const EDIT_BUTTONS: [(Hit, &str); 6] = [
    (Hit::CaptionSize(-1), "Smaller"),
    (Hit::CaptionSize(1), "Larger"),
    (Hit::CaptionMove(-1), "Lower"),
    (Hit::CaptionMove(1), "Higher"),
    (Hit::CaptionReset, "Reset"),
    (Hit::CaptionDone, "Done"),
];

fn edit_rect(i: usize) -> Rect {
    (24.0 + i as f32 * 194.0, 120.0, 180.0, 112.0)
}

/// What the bar would do for a pointer at (x, y).
pub fn hit(state: &State, x: f32, y: f32) -> Hit {
    if state.caption_edit {
        return EDIT_BUTTONS
            .iter()
            .enumerate()
            .find(|(i, _)| inside(edit_rect(*i), x, y))
            .map_or(Hit::Nothing, |(_, (hit, _))| *hit);
    }
    let can_caption = !state.subtitle_tracks.is_empty() || state.audio_tracks.len() > 1;
    let buttons = [
        (PREVIOUS, Hit::Previous, state.has_previous),
        (PLAY, Hit::PlayPause, true),
        (NEXT, Hit::Next, state.has_next),
        (CAPTIONS, Hit::Captions, can_caption),
        (SCREEN, Hit::Screen, true),
        (IMAGE, Hit::Image, true),
    ];
    for (rect, hit, enabled) in buttons {
        if enabled && inside(rect, x, y) {
            return hit;
        }
    }
    // The seek bar is easy to hit: its whole row, a little beyond its ends.
    let (sx, sy, sw, sh) = SEEK;
    if state.duration > 0.0
        && y >= sy - 10.0
        && y <= sy + sh + 10.0
        && x >= sx - 20.0
        && x <= sx + sw + 20.0
    {
        return Hit::Seek(((x - sx) / sw).clamp(0.0, 1.0));
    }
    Hit::Nothing
}

// ---- Dialogs (DIALOG_WIDTH × DIALOG_HEIGHT) ----

const CLOSE: Rect = (1040.0, 16.0, 136.0, 60.0);
const COL_W: f32 = 276.0;
const ROW_H: f32 = 60.0;

/// A 4-column grid of buttons starting at `top`.
fn grid(hits: Vec<Hit>, top: f32) -> Vec<(Hit, Rect)> {
    hits.into_iter()
        .enumerate()
        .map(|(i, hit)| {
            let (col, row) = ((i % 4) as f32, (i / 4) as f32);
            (
                hit,
                (
                    24.0 + col * (COL_W + 16.0),
                    top + row * (ROW_H + 12.0),
                    COL_W,
                    ROW_H,
                ),
            )
        })
        .collect()
}

/// Subtitle choices per page (3 rows of 4, less the More button).
const PER_PAGE: usize = 11;

/// Pages needed for `items` subtitle choices (Off counts).
pub fn pages(items: usize) -> usize {
    if items <= 12 {
        1
    } else {
        items.div_ceil(PER_PAGE)
    }
}

/// Every button of the open dialog, with its rectangle and whether it is usable.
fn dialog_buttons(state: &State) -> Vec<(Hit, Rect, bool)> {
    let mut out: Vec<(Hit, Rect, bool)> = vec![(Hit::Close, CLOSE, true)];
    let mut add = |items: Vec<(Hit, Rect)>, enabled: bool| {
        out.extend(items.into_iter().map(|(h, r)| (h, r, enabled)))
    };
    match state.dialog {
        Some(Dialog::Tracks) => {
            add(
                grid(
                    (0..state.audio_tracks.len().min(4))
                        .map(Hit::AudioTrack)
                        .collect(),
                    132.0,
                ),
                true,
            );
            let mut subs = vec![Hit::SubtitleTrack(None)];
            subs.extend((0..state.subtitle_tracks.len()).map(|i| Hit::SubtitleTrack(Some(i))));
            let subs = if pages(subs.len()) == 1 {
                subs
            } else {
                let mut page: Vec<Hit> = subs
                    .into_iter()
                    .skip(state.list_page * PER_PAGE)
                    .take(PER_PAGE)
                    .collect();
                page.push(Hit::MorePage);
                page
            };
            add(grid(subs, 246.0), true);
            out.push((Hit::CaptionEdit, (24.0, 548.0, 568.0, ROW_H), true));
        }
        Some(Dialog::Screen) => {
            add(
                grid((0..FORMATS.len()).map(Hit::Pick).collect(), 132.0),
                true,
            );
            out.push((
                Hit::Curved,
                (24.0, 420.0, 568.0, 72.0),
                state.curved.is_some(),
            ));
            out.push((
                Hit::SwapEyes,
                (608.0, 420.0, 568.0, 72.0),
                state.format.1 != Stereo::Mono,
            ));
        }
        Some(Dialog::Image) => {
            for (i, (minus, plus)) in [
                (Hit::Brightness(-1), Hit::Brightness(1)),
                (Hit::Contrast(-1), Hit::Contrast(1)),
                (Hit::Saturation(-1), Hit::Saturation(1)),
            ]
            .into_iter()
            .enumerate()
            {
                let y = 104.0 + i as f32 * 88.0;
                out.push((minus, (300.0, y, 96.0, 68.0), true));
                out.push((plus, (1080.0, y, 96.0, 68.0), true));
            }
            for turns in 0..4u8 {
                out.push((
                    Hit::Rotate(turns),
                    (300.0 + turns as f32 * 150.0, 392.0, 136.0, 68.0),
                    true,
                ));
            }
            out.push((Hit::ResetImage, (24.0, 540.0, COL_W, 72.0), true));
        }
        None => {}
    }
    out
}

/// The open dialog's usable buttons with their centres (for the D-pad).
pub fn dialog_targets(state: &State) -> Vec<(Hit, (f32, f32))> {
    dialog_buttons(state)
        .into_iter()
        .filter(|(_, _, enabled)| *enabled)
        .map(|(hit, (x, y, w, h), _)| (hit, (x + w / 2.0, y + h / 2.0)))
        .collect()
}

/// What the open dialog would do for a pointer at (x, y).
pub fn dialog_hit(state: &State, x: f32, y: f32) -> Hit {
    dialog_buttons(state)
        .into_iter()
        .find(|(_, r, enabled)| *enabled && inside(*r, x, y))
        .map_or(Hit::Nothing, |(hit, _, _)| hit)
}

pub fn format_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn centered(c: &mut Canvas, fonts: &mut Fonts, label: &str, r: Rect, size: f32, color: Rgb) {
    // Shrink long labels to fit.
    let mut size = size;
    while size > 16.0 && fonts.measure(label, size) > r.2 - 16.0 {
        size -= 2.0;
    }
    let w = fonts.measure(label, size);
    fonts.draw(
        c,
        label,
        r.0 + (r.2 - w) / 2.0,
        r.1 + r.3 / 2.0 + size * 0.36,
        size,
        color,
        r.2,
    );
}

fn button(c: &mut Canvas, r: Rect, hovered: bool, active: bool) {
    let color = if active {
        ACCENT
    } else if hovered {
        HOVER
    } else {
        BUTTON
    };
    c.rect(r.0, r.1, r.2, r.3, 18.0, color);
}

/// A right-pointing triangle from vertical strips, `h` tall at its base.
fn triangle(c: &mut Canvas, x: f32, cy: f32, w: f32, h: f32, flip: bool, color: Rgb) {
    let steps = (w * 2.0) as usize;
    for i in 0..steps {
        let t = i as f32 / steps as f32;
        let half = h / 2.0 * (1.0 - t);
        let px = if flip { x + w - t * w } else { x + t * w };
        c.rect(px, cy - half, 1.5, half * 2.0, 0.0, color);
    }
}

/// A five-pointed star (favourite marker) of outer radius `r`.
fn star(c: &mut Canvas, cx: f32, cy: f32, r: f32, color: Rgb) {
    let points: Vec<(f32, f32)> = (0..10)
        .map(|i| {
            let a = std::f32::consts::PI * (i as f32 / 5.0 - 0.5);
            let radius = if i % 2 == 0 { r } else { r * 0.45 };
            (cx + radius * a.cos(), cy + radius * a.sin())
        })
        .collect();
    // Even-odd fill, one pixel at a time (the star is small).
    for py in (cy - r) as i32..=(cy + r) as i32 {
        for px in (cx - r) as i32..=(cx + r) as i32 {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let mut inside = false;
            for i in 0..points.len() {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                if (a.1 > y) != (b.1 > y) && x < a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                    inside = !inside;
                }
            }
            if inside {
                c.rect(px as f32, py as f32, 1.0, 1.0, 0.0, color);
            }
        }
    }
}

/// A screen: a frame on a stand.
fn screen_icon(c: &mut Canvas, cx: f32, cy: f32, color: Rgb, fill: Rgb) {
    c.rect(cx - 26.0, cy - 20.0, 52.0, 32.0, 4.0, color);
    c.rect(cx - 21.0, cy - 15.0, 42.0, 22.0, 2.0, fill);
    c.rect(cx - 3.0, cy + 12.0, 6.0, 8.0, 0.0, color);
    c.rect(cx - 14.0, cy + 19.0, 28.0, 5.0, 2.0, color);
}

/// Image: a sun (brightness).
fn image_icon(c: &mut Canvas, cx: f32, cy: f32, color: Rgb) {
    c.circle(cx, cy, 11.0, color);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (s, co) = a.sin_cos();
        c.rect(
            cx + co * 20.0 - 3.0,
            cy + s * 20.0 - 3.0,
            6.0,
            6.0,
            3.0,
            color,
        );
    }
}

/// The bar.
pub fn render(state: &State, fonts: &mut Fonts, hover: Hit) -> Canvas {
    let mut c = Canvas::new(WIDTH, HEIGHT);
    c.clear(BG);
    if state.caption_edit {
        fonts.draw(
            &mut c,
            "Subtitle size and position",
            24.0,
            64.0,
            40.0,
            TEXT,
            800.0,
        );
        fonts.draw(&mut c, "B: done", 1040.0, 64.0, 26.0, SUBTLE, 140.0);
        for (i, (hit, label)) in EDIT_BUTTONS.iter().enumerate() {
            let r = edit_rect(i);
            button(&mut c, r, hover == *hit, *hit == Hit::CaptionDone);
            centered(&mut c, fonts, label, r, 32.0, TEXT);
        }
        return c;
    }

    // Previous / play-pause / next.
    for (r, hit, enabled) in [
        (PREVIOUS, Hit::Previous, state.has_previous),
        (PLAY, Hit::PlayPause, true),
        (NEXT, Hit::Next, state.has_next),
    ] {
        button(&mut c, r, enabled && hover == hit, false);
        let color = if enabled { TEXT } else { FAINT };
        let (cx, cy) = (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0);
        match hit {
            Hit::PlayPause if state.paused => {
                triangle(&mut c, cx - 14.0, cy, 36.0, 48.0, false, color)
            }
            Hit::PlayPause => {
                c.rect(cx - 17.0, cy - 22.0, 12.0, 44.0, 3.0, color);
                c.rect(cx + 5.0, cy - 22.0, 12.0, 44.0, 3.0, color);
            }
            Hit::Previous => {
                c.rect(cx - 20.0, cy - 18.0, 7.0, 36.0, 2.0, color);
                triangle(&mut c, cx - 12.0, cy, 30.0, 36.0, true, color);
            }
            _ => {
                triangle(&mut c, cx - 18.0, cy, 30.0, 36.0, false, color);
                c.rect(cx + 13.0, cy - 18.0, 7.0, 36.0, 2.0, color);
            }
        }
    }

    // The three setting buttons, each with a small label underneath.
    let label_under = |c: &mut Canvas, fonts: &mut Fonts, r: Rect, text: &str| {
        let area = (r.0 - 17.0, r.1 + r.3 + 4.0, r.2 + 34.0, 30.0);
        centered(c, fonts, text, area, 22.0, SUBTLE);
    };
    let can_caption = !state.subtitle_tracks.is_empty() || state.audio_tracks.len() > 1;
    let subtitles_on = state.subtitle.is_some();
    button(
        &mut c,
        CAPTIONS,
        can_caption && hover == Hit::Captions,
        subtitles_on,
    );
    centered(
        &mut c,
        fonts,
        "CC",
        CAPTIONS,
        34.0,
        if can_caption { TEXT } else { FAINT },
    );
    let caption_label = match (state.subtitle, state.subtitle_tracks.is_empty()) {
        (Some(i), _) => state.subtitle_tracks[i].clone(),
        (None, false) => "Off".to_string(),
        (None, true) => "No subtitles".to_string(),
    };
    label_under(&mut c, fonts, CAPTIONS, &caption_label);

    let screen_fill = if hover == Hit::Screen { HOVER } else { BUTTON };
    button(&mut c, SCREEN, hover == Hit::Screen, false);
    screen_icon(
        &mut c,
        SCREEN.0 + SCREEN.2 / 2.0,
        SCREEN.1 + SCREEN.3 / 2.0,
        TEXT,
        screen_fill,
    );
    label_under(&mut c, fonts, SCREEN, &format_label(state.format));

    let adjusted = state.image != ImageAdjust::default();
    button(&mut c, IMAGE, hover == Hit::Image, adjusted);
    image_icon(
        &mut c,
        IMAGE.0 + IMAGE.2 / 2.0,
        IMAGE.1 + IMAGE.3 / 2.0,
        TEXT,
    );
    label_under(&mut c, fonts, IMAGE, "Image");

    // Time and seek bar.
    let time = format!(
        "{} / {}",
        format_time(state.position),
        format_time(state.duration)
    );
    fonts.draw(
        &mut c,
        &time,
        24.0,
        SEEK.1 + SEEK.3 / 2.0 + 12.0,
        34.0,
        TEXT,
        SEEK.0 - 40.0,
    );
    let (sx, sy, sw, sh) = SEEK;
    let track_y = sy + sh / 2.0;
    c.rect(sx, track_y - 6.0, sw, 12.0, 6.0, TRACK);
    let fraction = if state.duration > 0.0 {
        (state.position / state.duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    c.rect(sx, track_y - 6.0, sw * fraction, 12.0, 6.0, ACCENT);
    if let Hit::Seek(f) = hover {
        c.rect(sx + sw * f - 2.0, track_y - 22.0, 4.0, 44.0, 2.0, SUBTLE);
        let label = format_time(f as f64 * state.duration);
        let w = fonts.measure(&label, 24.0);
        let lx = (sx + sw * f - w / 2.0).clamp(sx, sx + sw - w);
        fonts.draw(&mut c, &label, lx, sy + 6.0, 24.0, SUBTLE, w + 4.0);
    }
    c.circle(sx + sw * fraction, track_y, 14.0, TEXT);
    c
}

/// A slider's value as a bar between its − and + buttons.
fn value_bar(c: &mut Canvas, fonts: &mut Fonts, y: f32, fraction: f32, label: &str) {
    let (x, w) = (420.0, 640.0);
    c.rect(x, y + 28.0, w, 12.0, 6.0, TRACK);
    c.rect(x, y + 28.0, w * fraction.clamp(0.0, 1.0), 12.0, 6.0, ACCENT);
    c.rect(x + w * 0.5 - 1.0, y + 20.0, 2.0, 28.0, 1.0, SUBTLE);
    let lw = fonts.measure(label, 22.0);
    fonts.draw(c, label, x + w - lw, y + 16.0, 22.0, SUBTLE, lw + 4.0);
}

/// The open dialog.
pub fn render_dialog(state: &State, fonts: &mut Fonts, hover: Hit) -> Canvas {
    let mut c = Canvas::new(DIALOG_WIDTH, DIALOG_HEIGHT);
    c.clear(BG);
    let title = match state.dialog {
        Some(Dialog::Tracks) => "Audio and subtitles",
        Some(Dialog::Screen) => "Screen",
        Some(Dialog::Image) => "Image",
        None => "",
    };
    fonts.draw(&mut c, title, 24.0, 60.0, 40.0, TEXT, 900.0);
    let section = |c: &mut Canvas, fonts: &mut Fonts, text: &str, y: f32| {
        fonts.draw(c, text, 24.0, y, 26.0, SUBTLE, 1150.0);
    };
    match state.dialog {
        Some(Dialog::Tracks) => {
            section(&mut c, fonts, "Audio", 118.0);
            section(&mut c, fonts, "Subtitles", 232.0);
            section(&mut c, fonts, "Subtitle size and position", 534.0);
        }
        Some(Dialog::Screen) => {
            section(
                &mut c,
                fonts,
                "Format  ·  click to use, long press to star a favourite",
                118.0,
            );
            section(&mut c, fonts, "Screen", 406.0);
        }
        Some(Dialog::Image) => {
            let image = &state.image;
            let rows = [
                (
                    "Brightness",
                    (image.brightness + 0.5) / 1.0,
                    format!("{:+.0}%", image.brightness * 100.0),
                ),
                (
                    "Contrast",
                    // 50%..100% on the left half, 100%..200% on the right.
                    if image.contrast < 1.0 {
                        image.contrast - 0.5
                    } else {
                        0.5 + (image.contrast - 1.0) / 2.0
                    },
                    format!("{:.0}%", image.contrast * 100.0),
                ),
                (
                    "Saturation",
                    image.saturation / 2.0,
                    format!("{:.0}%", image.saturation * 100.0),
                ),
            ];
            for (i, (name, fraction, value)) in rows.iter().enumerate() {
                let y = 104.0 + i as f32 * 88.0;
                fonts.draw(&mut c, name, 24.0, y + 46.0, 32.0, TEXT, 260.0);
                value_bar(&mut c, fonts, y, *fraction, value);
            }
            fonts.draw(&mut c, "Rotate", 24.0, 392.0 + 46.0, 32.0, TEXT, 260.0);
        }
        None => {}
    }
    for (hit, r, enabled) in dialog_buttons(state) {
        let hovered = enabled && hover == hit;
        let (label, active) = match hit {
            Hit::Close => ("Close".to_string(), false),
            Hit::AudioTrack(i) => (state.audio_tracks[i].clone(), state.audio == Some(i)),
            Hit::SubtitleTrack(None) => ("Off".to_string(), state.subtitle.is_none()),
            Hit::SubtitleTrack(Some(i)) => {
                (state.subtitle_tracks[i].clone(), state.subtitle == Some(i))
            }
            Hit::MorePage => {
                let pages = pages(state.subtitle_tracks.len() + 1);
                (format!("More ({}/{pages})", state.list_page + 1), false)
            }
            Hit::CaptionEdit => ("Adjust size and position…".to_string(), false),
            Hit::Pick(i) => (format_label(FORMATS[i]), FORMATS[i] == state.format),
            Hit::Curved => (
                (if state.curved == Some(true) {
                    "Curved screen: on"
                } else {
                    "Curved screen: off"
                })
                .to_string(),
                state.curved == Some(true),
            ),
            Hit::SwapEyes => ("Swap eyes".to_string(), state.swap_eyes),
            Hit::Brightness(d) | Hit::Contrast(d) | Hit::Saturation(d) => {
                ((if d < 0 { "−" } else { "+" }).to_string(), false)
            }
            Hit::Rotate(t) => (format!("{}°", t as u32 * 90), state.image.rotation == t),
            Hit::ResetImage => ("Reset".to_string(), false),
            _ => (String::new(), false),
        };
        button(&mut c, r, hovered, active && enabled);
        let size = if matches!(
            hit,
            Hit::Brightness(_) | Hit::Contrast(_) | Hit::Saturation(_)
        ) {
            44.0
        } else {
            28.0
        };
        centered(
            &mut c,
            fonts,
            &label,
            r,
            size,
            if enabled { TEXT } else { FAINT },
        );
        if let Hit::Pick(i) = hit
            && state.favourites.contains(&FORMATS[i])
        {
            star(&mut c, r.0 + r.2 - 22.0, r.1 + 20.0, 11.0, STAR);
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            paused: false,
            position: 30.0,
            duration: 120.0,
            has_previous: true,
            has_next: false,
            curved: Some(false),
            format: FORMATS[0],
            favourites: vec![FORMATS[0], FORMATS[3]],
            swap_eyes: false,
            subtitle_tracks: Vec::new(),
            subtitle: None,
            audio_tracks: vec!["Korean 5.1".into()],
            audio: Some(0),
            list_page: 0,
            image: ImageAdjust::default(),
            dialog: None,
            caption_edit: false,
        }
    }

    fn center(r: Rect) -> (f32, f32) {
        (r.0 + r.2 / 2.0, r.1 + r.3 / 2.0)
    }

    #[test]
    fn bar_buttons_and_seek() {
        let s = state();
        assert_eq!(hit(&s, SEEK.0, SEEK.1 + 10.0), Hit::Seek(0.0));
        assert_eq!(
            hit(&s, SEEK.0 + SEEK.2 + 5.0, SEEK.1 + 10.0),
            Hit::Seek(1.0)
        );
        for (r, h) in [
            (PREVIOUS, Hit::Previous),
            (PLAY, Hit::PlayPause),
            (NEXT, Hit::Nothing),     // no next video
            (CAPTIONS, Hit::Nothing), // no subtitles, one audio track
            (SCREEN, Hit::Screen),
            (IMAGE, Hit::Image),
        ] {
            let (x, y) = center(r);
            assert_eq!(hit(&s, x, y), h);
            assert!(r.0 + r.2 <= WIDTH as f32 && r.1 + r.3 <= HEIGHT as f32);
        }
        let with_subs = State {
            subtitle_tracks: vec!["English".into()],
            ..s
        };
        let (x, y) = center(CAPTIONS);
        assert_eq!(hit(&with_subs, x, y), Hit::Captions);
    }

    #[test]
    fn dialogs_fit_and_hit() {
        for dialog in [Dialog::Tracks, Dialog::Screen, Dialog::Image] {
            let s = State {
                dialog: Some(dialog),
                subtitle_tracks: (0..20).map(|i| format!("Track {i}")).collect(),
                audio_tracks: vec!["A".into(), "B".into()],
                format: FORMATS[3],
                ..state()
            };
            for (h, r, enabled) in dialog_buttons(&s) {
                assert!(
                    r.0 >= 0.0
                        && r.0 + r.2 <= DIALOG_WIDTH as f32
                        && r.1 + r.3 <= DIALOG_HEIGHT as f32,
                    "{h:?} outside"
                );
                let (x, y) = center(r);
                assert_eq!(
                    dialog_hit(&s, x, y),
                    if enabled { h } else { Hit::Nothing },
                    "{dialog:?} {h:?}"
                );
            }
        }
    }

    #[test]
    fn caption_editing_replaces_the_bar() {
        let s = State {
            caption_edit: true,
            ..state()
        };
        for (i, (h, _)) in EDIT_BUTTONS.iter().enumerate() {
            let r = edit_rect(i);
            assert!(r.0 + r.2 <= WIDTH as f32 && r.1 + r.3 <= HEIGHT as f32);
            let (x, y) = center(r);
            assert_eq!(hit(&s, x, y), *h);
        }
        let (x, y) = center(PLAY);
        assert_eq!(hit(&s, x, y), Hit::Nothing);
    }

    #[test]
    fn long_subtitle_lists_page() {
        assert_eq!(pages(21), 2);
        assert_eq!(pages(12), 1, "Off + 11 tracks fit");
        let s = State {
            dialog: Some(Dialog::Tracks),
            subtitle_tracks: (0..20).map(|i| format!("Track {i}")).collect(),
            list_page: 1,
            ..state()
        };
        let subs: Vec<Hit> = dialog_buttons(&s)
            .into_iter()
            .map(|(h, _, _)| h)
            .filter(|h| matches!(h, Hit::SubtitleTrack(_) | Hit::MorePage))
            .collect();
        assert_eq!(subs.first(), Some(&Hit::SubtitleTrack(Some(10))));
        assert_eq!(subs.last(), Some(&Hit::MorePage));
    }

    #[test]
    fn format_button_steps_through_favourites() {
        let favourites = [FORMATS[0], FORMATS[3], FORMATS[7]];
        assert_eq!(next_favourite(FORMATS[0], &favourites), Some(FORMATS[3]));
        assert_eq!(next_favourite(FORMATS[7], &favourites), Some(FORMATS[0]));
        assert_eq!(
            next_favourite(FORMATS[5], &favourites),
            Some(FORMATS[0]),
            "not a favourite: start over"
        );
        assert_eq!(next_favourite(FORMATS[5], &[]), None);
    }

    #[test]
    fn times_read_naturally() {
        assert_eq!(format_time(75.0), "1:15");
        assert_eq!(format_time(5530.0), "1:32:10");
    }
}
