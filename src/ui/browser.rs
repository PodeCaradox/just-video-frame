//! The headset browser panel: a list of servers, shares, folders or videos,
//! drawn into a canvas that is shown on an OpenXR quad layer.

use super::canvas::{Canvas, Fonts, Rgb};
use super::form::{self, Form};
use crate::playability::Verdict;
use crate::vr::{Layout, Projection, Stereo};

pub const WIDTH: u32 = 1600;
pub const HEIGHT: u32 = 1000;
const HEADER: f32 = 120.0;
const BOTTOM: f32 = 24.0;
const ROW: f32 = 88.0;
const PAD: f32 = 32.0;
const CRUMB_SIZE: f32 = 40.0;
const CRUMB_SEP: &str = "  ›  ";
/// Unlock button and row actions, from the right edge of a row.
const LOCK_W: f32 = 76.0;
const ACTION_W: f32 = 150.0;
const ICON_ACTION_W: f32 = 84.0;
/// The scrollbar's grab zone at the right edge of the list.
const SCROLL_W: f32 = 64.0;
/// Header tool buttons (e.g. "Select", "Delete 3").
const TOOL_W: f32 = 220.0;
const TOOL_GAP: f32 = 12.0;
const ICON_TOOL_W: f32 = 84.0;

/// Shown top right so installs can be told apart.
pub const BUILD: &str = env!("JUST_VIDEO_BUILD");

const BG: Rgb = [0x15, 0x17, 0x1c];
const ROW_BG: Rgb = [0x1d, 0x21, 0x28];
const HOVER: Rgb = [0x2c, 0x33, 0x40];
const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const FAINT: Rgb = [0x5f, 0x63, 0x68];
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const GREEN: Rgb = [0x34, 0xc7, 0x59];
const YELLOW: Rgb = [0xff, 0xcc, 0x00];
const ORANGE: Rgb = [0xff, 0x95, 0x00];
const RED: Rgb = [0xff, 0x45, 0x3a];
const GREY: Rgb = [0x5f, 0x63, 0x68];

#[derive(Clone, Debug, PartialEq)]
pub enum Icon {
    Server,
    Share,
    Folder,
    /// A video; `None` while its playability is still being checked.
    Video(Option<Verdict>),
    /// A 3D video for a flat screen (side by side or top/bottom): glasses.
    Video3d(Option<Verdict>),
    /// A VR180, VR360 or fisheye video: a headset.
    VideoVr(Option<Verdict>),
    /// A file that could not be read.
    Broken,
    /// A file that isn't a video.
    File,
    Add,
    /// A gear: the Settings screen.
    Settings,
    /// A slider: one setting.
    Slider,
}

impl Icon {
    /// A video's mark: its shape says flat, 3D or VR, its colour how well it plays.
    pub fn video(verdict: Option<Verdict>, layout: Option<&Layout>) -> Icon {
        match layout {
            Some(l) if l.projection != Projection::Flat => Icon::VideoVr(verdict),
            Some(l) if l.stereo != Stereo::Mono => Icon::Video3d(verdict),
            _ => Icon::Video(verdict),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Rename,
    /// Delete a file or folder.
    Delete,
    Edit,
    /// Remove a saved server.
    Remove,
}

impl Action {
    /// Rename is a pencil, Delete and Remove are trash cans; the rest are labelled.
    fn is_icon(self) -> bool {
        matches!(self, Action::Rename | Action::Delete | Action::Remove)
    }

    fn width(self) -> f32 {
        if self.is_icon() {
            ICON_ACTION_W
        } else {
            ACTION_W
        }
    }

    fn label(self) -> &'static str {
        match self {
            Action::Rename => "Rename",
            Action::Delete => "Delete",
            Action::Edit => "Edit",
            Action::Remove => "Remove",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub icon: Icon,
    pub label: String,
    pub detail: String,
    pub right: String,
    /// A lock toggle (servers): `Some(unlocked)`.
    pub lock: Option<bool>,
    /// Buttons at the right end, left to right.
    pub actions: Vec<Action>,
    /// A checkbox in place of the icon (selecting what to delete).
    pub checked: Option<bool>,
    /// Greyed out: listed, but not something to open (non-video files).
    pub dimmed: bool,
    /// Outlined: the entry just come back out of.
    pub outlined: bool,
}

impl Row {
    pub fn new(icon: Icon, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            detail: String::new(),
            right: String::new(),
            lock: None,
            actions: Vec::new(),
            checked: None,
            dimmed: false,
            outlined: false,
        }
    }
}

/// A button in the header, right-aligned.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub label: String,
    pub danger: bool,
    /// Drawn as a square icon button instead of the label.
    pub icon: Option<ToolIcon>,
    /// Toggled on (e.g. edit mode).
    pub active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolIcon {
    /// Edit mode: rename and delete on each row.
    Edit,
    /// Select several entries to delete.
    Select,
}

impl Tool {
    pub fn text(label: impl Into<String>, danger: bool) -> Self {
        Self {
            label: label.into(),
            danger,
            icon: None,
            active: false,
        }
    }

    pub fn icon(icon: ToolIcon, active: bool) -> Self {
        Self {
            label: String::new(),
            danger: false,
            icon: Some(icon),
            active,
        }
    }

    fn width(&self) -> f32 {
        if self.icon.is_some() {
            ICON_TOOL_W
        } else {
            TOOL_W
        }
    }
}

#[derive(Clone, Debug)]
pub struct Dialog {
    pub title: String,
    pub body: Vec<String>,
    /// Left to right; the last one is the primary action.
    pub buttons: Vec<String>,
    /// The primary button is destructive (drawn red).
    pub danger: bool,
}

#[derive(Clone, Debug, Default)]
pub struct View {
    /// Breadcrumb path; the last entry is the current place.
    pub crumbs: Vec<String>,
    pub rows: Vec<Row>,
    /// Shown instead of rows (loading, errors, empty folders).
    pub status: Option<String>,
    /// Small line at the bottom (e.g. "Opening …").
    pub notice: Option<String>,
    pub dialog: Option<Dialog>,
    pub form: Option<Form>,
    pub tools: Vec<Tool>,
    /// First visible row (fractional while scrolling).
    pub scroll: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Row(usize),
    Crumb(usize),
    Lock(usize),
    RowAction(usize, Action),
    Tool(usize),
    /// The scrollbar: scroll with [`scroll_at`].
    ScrollBar,
    DialogButton(usize),
    Form(form::Hit),
    Nothing,
}

pub fn visible_rows() -> f32 {
    (HEIGHT as f32 - HEADER - BOTTOM) / ROW
}

impl View {
    pub fn max_scroll(&self) -> f32 {
        (self.rows.len() as f32 - visible_rows()).max(0.0)
    }

    pub fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }
}

type Rect = (f32, f32, f32, f32);

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

fn dialog_buttons(n: usize) -> Vec<Rect> {
    let (w, h, gap) = (280.0, 80.0, 24.0);
    let total = n as f32 * w + (n.saturating_sub(1)) as f32 * gap;
    let x0 = WIDTH as f32 / 2.0 - total / 2.0;
    (0..n)
        .map(|i| (x0 + i as f32 * (w + gap), HEIGHT as f32 - 250.0, w, h))
        .collect()
}

fn tools_width(view: &View) -> f32 {
    view.tools.iter().map(|t| t.width() + TOOL_GAP).sum()
}

fn tool_rect(view: &View, k: usize) -> Rect {
    let right: f32 = view.tools[k..].iter().map(|t| t.width() + TOOL_GAP).sum();
    (
        WIDTH as f32 - PAD - right + TOOL_GAP,
        36.0,
        view.tools[k].width(),
        62.0,
    )
}

fn scroll_track() -> (f32, f32) {
    (HEADER, HEIGHT as f32 - BOTTOM - HEADER)
}

fn thumb_height(view: &View) -> f32 {
    (visible_rows() / view.rows.len().max(1) as f32 * scroll_track().1).max(60.0)
}

/// The scroll position that puts the scrollbar's thumb under canvas `y`.
pub fn scroll_at(view: &View, y: f32) -> f32 {
    let (top, track) = scroll_track();
    let thumb = thumb_height(view);
    let f = ((y - top - thumb / 2.0) / (track - thumb).max(1.0)).clamp(0.0, 1.0);
    f * view.max_scroll()
}

/// Rows moved by a vertical drag of `dy` canvas pixels.
pub fn rows_for_drag(dy: f32) -> f32 {
    dy / ROW
}

/// Horizontal extent of each breadcrumb, as drawn.
pub fn crumb_spans(view: &View, fonts: &mut Fonts) -> Vec<(f32, f32)> {
    let max_w = crumb_limit(view, fonts);
    let mut x = PAD;
    let mut spans = Vec::new();
    for (i, crumb) in view.crumbs.iter().enumerate() {
        let w = fonts.measure(crumb, CRUMB_SIZE).min(max_w);
        spans.push((x, x + w));
        x += w;
        if i + 1 < view.crumbs.len() {
            x += fonts.measure(CRUMB_SEP, CRUMB_SIZE);
        }
    }
    spans
}

fn crumb_limit(view: &View, fonts: &mut Fonts) -> f32 {
    // Room left of the tools and build label, shared by at most a few long names.
    let right = (fonts.measure(BUILD, 20.0) + 40.0).max(tools_width(view) + 24.0);
    ((WIDTH as f32 - 2.0 * PAD - right) / 3.0).max(160.0)
}

fn row_rect(view: &View, i: usize) -> Rect {
    let y = HEADER + (i as f32 - view.scroll) * ROW;
    (PAD, y + 4.0, WIDTH as f32 - 2.0 * PAD - SCROLL_W, ROW - 8.0)
}

fn lock_rect(view: &View, i: usize) -> Rect {
    let (x, y, w, h) = row_rect(view, i);
    (x + w - LOCK_W - 8.0, y + 6.0, LOCK_W, h - 12.0)
}

/// The `k`th action button of row `i` (buttons sit left of the lock, if any).
fn action_rect(view: &View, i: usize, k: usize) -> Rect {
    let (x, y, w, h) = row_rect(view, i);
    let right = if view.rows[i].lock.is_some() {
        lock_rect(view, i).0 - 10.0
    } else {
        x + w - 8.0
    };
    let actions = &view.rows[i].actions;
    let from_right: f32 = actions[k..].iter().map(|a| a.width() + 10.0).sum();
    (
        right - from_right + 10.0,
        y + 6.0,
        actions[k].width(),
        h - 12.0,
    )
}

/// What the pointer at canvas pixel (x, y) would activate.
pub fn hit(view: &View, fonts: &mut Fonts, x: f32, y: f32) -> Hit {
    if let Some(f) = &view.form {
        return Hit::Form(form::hit(f, fonts, WIDTH as f32, x, y));
    }
    let crumbs = crumb_spans(view, fonts);
    if let Some(dialog) = &view.dialog {
        return dialog_buttons(dialog.buttons.len())
            .into_iter()
            .position(|r| inside(r, x, y))
            .map_or(Hit::Nothing, Hit::DialogButton);
    }
    if y < HEADER - 10.0 {
        if let Some(k) = (0..view.tools.len()).find(|&k| inside(tool_rect(view, k), x, y)) {
            return Hit::Tool(k);
        }
        // The last crumb is where we are: not a link.
        let last = view.crumbs.len().saturating_sub(1);
        return crumbs
            .iter()
            .position(|&(a, b)| x >= a - 8.0 && x <= b + 8.0 && y > 30.0)
            .filter(|&i| i < last)
            .map_or(Hit::Nothing, Hit::Crumb);
    }
    if view.status.is_some() || y > HEIGHT as f32 - BOTTOM || x < PAD || x > WIDTH as f32 - PAD {
        return Hit::Nothing;
    }
    if x > WIDTH as f32 - PAD - SCROLL_W + 8.0 {
        return if view.max_scroll() > 0.0 {
            Hit::ScrollBar
        } else {
            Hit::Nothing
        };
    }
    let index = ((y - HEADER) / ROW + view.scroll).floor();
    if index < 0.0 || index as usize >= view.rows.len() {
        return Hit::Nothing;
    }
    let i = index as usize;
    let row = &view.rows[i];
    if row.lock.is_some() && inside(lock_rect(view, i), x, y) {
        return Hit::Lock(i);
    }
    for (k, action) in row.actions.iter().enumerate() {
        if inside(action_rect(view, i, k), x, y) {
            return Hit::RowAction(i, *action);
        }
    }
    Hit::Row(i)
}

/// `bg` is the row behind the icon, for cut-outs.
fn draw_icon(canvas: &mut Canvas, icon: &Icon, cx: f32, cy: f32, bg: Rgb) {
    match icon {
        Icon::Server => {
            canvas.rect(cx - 22.0, cy - 20.0, 44.0, 16.0, 4.0, ACCENT);
            canvas.rect(cx - 22.0, cy + 2.0, 44.0, 16.0, 4.0, ACCENT);
            canvas.circle(cx + 13.0, cy - 12.0, 3.0, BG);
            canvas.circle(cx + 13.0, cy + 10.0, 3.0, BG);
        }
        Icon::Share | Icon::Folder => {
            let color = if *icon == Icon::Share {
                ACCENT
            } else {
                [0x8a, 0xb4, 0xf8]
            };
            canvas.rect(cx - 24.0, cy - 18.0, 20.0, 10.0, 3.0, color);
            canvas.rect(cx - 24.0, cy - 12.0, 48.0, 32.0, 4.0, color);
        }
        Icon::Video(verdict) => canvas.circle(cx, cy, 16.0, verdict_color(*verdict)),
        Icon::Video3d(verdict) => {
            // Glasses: two lenses on a bar, with short arms.
            let color = verdict_color(*verdict);
            canvas.rect(cx - 26.0, cy - 10.0, 52.0, 5.0, 2.0, color);
            canvas.rect(cx - 24.0, cy - 10.0, 21.0, 19.0, 6.0, color);
            canvas.rect(cx + 3.0, cy - 10.0, 21.0, 19.0, 6.0, color);
        }
        Icon::VideoVr(verdict) => {
            // A headset from the front: visor with two lenses and a nose gap,
            // and the strap at the sides.
            let color = verdict_color(*verdict);
            canvas.rect(cx - 29.0, cy - 6.0, 58.0, 8.0, 3.0, color);
            canvas.rect(cx - 24.0, cy - 17.0, 48.0, 33.0, 10.0, color);
            canvas.circle(cx - 11.0, cy - 2.0, 7.0, bg);
            canvas.circle(cx + 11.0, cy - 2.0, 7.0, bg);
            canvas.circle(cx, cy + 17.0, 7.0, bg);
        }
        Icon::Broken => {
            canvas.circle(cx, cy, 16.0, RED);
            canvas.rect(cx - 9.0, cy - 3.0, 18.0, 6.0, 2.0, BG);
        }
        Icon::File => {
            // A page with a folded top-right corner.
            canvas.rect(cx - 16.0, cy - 20.0, 32.0, 40.0, 4.0, FAINT);
            canvas.rect(cx - 12.0, cy - 16.0, 24.0, 32.0, 2.0, ROW_BG);
            canvas.rect(cx + 2.0, cy - 21.0, 15.0, 15.0, 0.0, ROW_BG);
            canvas.rect(cx + 2.0, cy - 20.0, 4.0, 14.0, 1.0, FAINT);
            canvas.rect(cx + 2.0, cy - 10.0, 14.0, 4.0, 1.0, FAINT);
        }
        Icon::Add => {
            canvas.rect(cx - 3.0, cy - 18.0, 6.0, 36.0, 3.0, ACCENT);
            canvas.rect(cx - 18.0, cy - 3.0, 36.0, 6.0, 3.0, ACCENT);
        }
        Icon::Settings => {
            // Eight teeth around a wheel with a hole.
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let (x, y) = (cx + 16.0 * a.cos(), cy + 16.0 * a.sin());
                canvas.rect(x - 5.0, y - 5.0, 10.0, 10.0, 2.0, SUBTLE);
            }
            canvas.circle(cx, cy, 15.0, SUBTLE);
            canvas.circle(cx, cy, 6.0, bg);
        }
        Icon::Slider => {
            canvas.rect(cx - 20.0, cy - 2.0, 40.0, 4.0, 2.0, FAINT);
            canvas.rect(cx - 20.0, cy - 2.0, 22.0, 4.0, 2.0, ACCENT);
            canvas.circle(cx + 2.0, cy, 8.0, ACCENT);
        }
    }
}

/// How well a video plays; grey while that is being checked.
fn verdict_color(verdict: Option<Verdict>) -> Rgb {
    match verdict {
        Some(Verdict::Hardware) => GREEN,
        Some(Verdict::Software) => YELLOW,
        Some(Verdict::SoftwareMarginal) => ORANGE,
        Some(Verdict::Unplayable) => RED,
        None => GREY,
    }
}

/// A trash can: lid with handle, and a body with three slots.
fn draw_trash(canvas: &mut Canvas, cx: f32, cy: f32, color: Rgb) {
    canvas.rect(cx - 6.0, cy - 21.0, 12.0, 5.0, 2.0, color);
    canvas.rect(cx - 17.0, cy - 16.0, 34.0, 5.0, 2.0, color);
    canvas.rect(cx - 13.0, cy - 8.0, 26.0, 29.0, 4.0, color);
    for dx in [-6.0, 0.0, 6.0] {
        canvas.rect(cx + dx - 1.5, cy - 3.0, 3.0, 19.0, 1.5, [0x6a, 0x1d, 0x19]);
    }
}

/// A pencil pointing down-left.
fn draw_pencil(canvas: &mut Canvas, cx: f32, cy: f32, color: Rgb) {
    for i in 0..26 {
        let t = i as f32;
        canvas.rect(cx - 13.0 + t, cy + 13.0 - t - 4.0, 9.0, 9.0, 2.0, color);
    }
    // Tip.
    canvas.rect(cx - 19.0, cy + 13.0, 6.0, 6.0, 1.0, color);
}

fn draw_checkbox(canvas: &mut Canvas, cx: f32, cy: f32, checked: bool) {
    if checked {
        canvas.rect(cx - 18.0, cy - 18.0, 36.0, 36.0, 7.0, RED);
        // A tick from two strokes of small squares.
        for i in 0..8 {
            let t = i as f32;
            canvas.rect(cx - 11.0 + t, cy - 1.0 + t, 5.0, 5.0, 1.0, TEXT);
        }
        for i in 0..14 {
            let t = i as f32;
            canvas.rect(cx - 4.0 + t, cy + 6.0 - t * 1.2, 5.0, 5.0, 1.0, TEXT);
        }
    } else {
        canvas.rect(cx - 18.0, cy - 18.0, 36.0, 36.0, 7.0, SUBTLE);
        canvas.rect(cx - 14.0, cy - 14.0, 28.0, 28.0, 5.0, ROW_BG);
    }
}

/// A padlock, open or closed.
fn draw_lock(canvas: &mut Canvas, cx: f32, cy: f32, open: bool, color: Rgb) {
    canvas.rect(cx - 16.0, cy - 2.0, 32.0, 24.0, 5.0, color);
    let shackle_x = if open { cx + 2.0 } else { cx - 11.0 };
    // Shackle: an arch from two posts and a top bar.
    canvas.rect(shackle_x, cy - 20.0, 5.0, 20.0, 2.0, color);
    canvas.rect(
        shackle_x + 17.0,
        cy - (if open { 26.0 } else { 20.0 }),
        5.0,
        if open { 14.0 } else { 20.0 },
        2.0,
        color,
    );
    canvas.rect(shackle_x, cy - 22.0, 22.0, 5.0, 2.0, color);
    canvas.circle(cx, cy + 9.0, 3.5, BG);
}

/// Renders the panel; `pointer` highlights what it hovers and, with
/// `draw_cursor`, marks its position (previews; the headset has a cursor layer).
pub fn render(
    view: &View,
    fonts: &mut Fonts,
    pointer: Option<(f32, f32)>,
    draw_cursor: bool,
) -> Canvas {
    let mut canvas = Canvas::new(WIDTH, HEIGHT);
    canvas.clear(BG);
    let w = WIDTH as f32;
    let crumbs = crumb_spans(view, fonts);
    let hover = pointer.map(|(x, y)| hit(view, fonts, x, y));
    let bottom = HEIGHT as f32 - BOTTOM;

    if let Some(f) = &view.form {
        let form_hover = match hover {
            Some(Hit::Form(h)) => h,
            _ => form::Hit::Nothing,
        };
        fonts.draw(&mut canvas, &f.title, PAD, 78.0, 46.0, TEXT, w - 2.0 * PAD);
        form::render(&mut canvas, fonts, f, form_hover);
    } else if let Some(status) = &view.status {
        let lines = fonts.wrap(status, 36.0, w - 4.0 * PAD);
        for (i, line) in lines.iter().enumerate() {
            fonts.draw(
                &mut canvas,
                line,
                2.0 * PAD,
                HEADER + 90.0 + i as f32 * 52.0,
                36.0,
                SUBTLE,
                w - 4.0 * PAD,
            );
        }
    } else {
        let first = view.scroll.floor() as usize;
        for (i, row) in view.rows.iter().enumerate().skip(first) {
            let (rx, ry, rw, rh) = row_rect(view, i);
            if ry > bottom {
                break;
            }
            // A dimmed row opens nothing, so it only lights up while selecting.
            let hovered = matches!(hover, Some(Hit::Row(r)) if r == i)
                && (!row.dimmed || row.checked.is_some());
            let label_color = if row.dimmed { FAINT } else { TEXT };
            let detail_color = if row.dimmed { FAINT } else { SUBTLE };
            if row.outlined {
                canvas.rect(rx - 3.0, ry - 3.0, rw + 6.0, rh + 6.0, 17.0, ACCENT);
            }
            let bg = if hovered { HOVER } else { ROW_BG };
            canvas.rect(rx, ry, rw, rh, 14.0, bg);
            let (icon_x, icon_y) = (PAD + 48.0, ry - 4.0 + ROW / 2.0);
            match row.checked {
                Some(checked) => draw_checkbox(&mut canvas, icon_x, icon_y, checked),
                None => draw_icon(&mut canvas, &row.icon, icon_x, icon_y, bg),
            }
            let mut right_edge = rx + rw - 24.0;
            if let Some(unlocked) = row.lock {
                let lock = lock_rect(view, i);
                let lock_hover = hover == Some(Hit::Lock(i));
                if lock_hover || unlocked {
                    let fill = if unlocked { [0x3a, 0x2c, 0x14] } else { HOVER };
                    canvas.rect(lock.0, lock.1, lock.2, lock.3, 12.0, fill);
                }
                let color = if unlocked {
                    ORANGE
                } else if lock_hover {
                    TEXT
                } else {
                    FAINT
                };
                draw_lock(
                    &mut canvas,
                    lock.0 + lock.2 / 2.0,
                    lock.1 + lock.3 / 2.0 + 2.0,
                    unlocked,
                    color,
                );
                right_edge = lock.0 - 16.0;
            }
            for (k, action) in row.actions.iter().enumerate() {
                let (ax, ay, aw, ah) = action_rect(view, i, k);
                let color = if matches!(action, Action::Remove | Action::Delete) {
                    RED
                } else {
                    ACCENT
                };
                let hovered = hover == Some(Hit::RowAction(i, *action));
                let fill = if hovered {
                    color
                } else {
                    [color[0] / 3, color[1] / 3, color[2] / 3]
                };
                canvas.rect(ax, ay, aw, ah, 12.0, fill);
                if *action == Action::Rename {
                    draw_pencil(&mut canvas, ax + aw / 2.0, ay + ah / 2.0, TEXT);
                } else if action.is_icon() {
                    draw_trash(&mut canvas, ax + aw / 2.0, ay + ah / 2.0, TEXT);
                } else {
                    let label = action.label();
                    let lw = fonts.measure(label, 30.0);
                    // Edit: a pencil before the word, like Rename's.
                    let pencil_w = if *action == Action::Edit { 50.0 } else { 0.0 };
                    let lx = ax + (aw - lw - pencil_w) / 2.0 + pencil_w;
                    if pencil_w > 0.0 {
                        let pencil_x = lx - pencil_w / 2.0 - 6.0;
                        draw_pencil(&mut canvas, pencil_x, ay + ah / 2.0, TEXT);
                    }
                    fonts.draw(&mut canvas, label, lx, ay + ah / 2.0 + 11.0, 30.0, TEXT, aw);
                }
                if k == 0 {
                    right_edge = ax - 16.0;
                }
            }
            let unlocked = !row.actions.is_empty();
            let right_w = if row.right.is_empty() || unlocked {
                0.0
            } else {
                fonts.measure(&row.right, 28.0) + 24.0
            };
            let text_w = right_edge - right_w - (PAD + 96.0);
            if row.detail.is_empty() {
                fonts.draw(
                    &mut canvas,
                    &row.label,
                    PAD + 96.0,
                    ry + 52.0,
                    36.0,
                    label_color,
                    text_w,
                );
            } else {
                fonts.draw(
                    &mut canvas,
                    &row.label,
                    PAD + 96.0,
                    ry + 38.0,
                    34.0,
                    label_color,
                    text_w,
                );
                fonts.draw(
                    &mut canvas,
                    &row.detail,
                    PAD + 96.0,
                    ry + 70.0,
                    24.0,
                    detail_color,
                    text_w,
                );
            }
            if right_w > 0.0 {
                fonts.draw(
                    &mut canvas,
                    &row.right,
                    right_edge - right_w + 12.0,
                    ry + 52.0,
                    28.0,
                    detail_color,
                    right_w,
                );
            }
        }
        // Cover rows that scrolled under the header or off the bottom.
        canvas.rect(0.0, 0.0, w, HEADER - 8.0, 0.0, BG);
        canvas.rect(0.0, bottom, w, BOTTOM, 0.0, BG);
        let max = view.max_scroll();
        if max > 0.0 {
            let (track_top, track) = scroll_track();
            let thumb = thumb_height(view);
            let top = track_top + view.scroll / max * (track - thumb);
            let hovered = hover == Some(Hit::ScrollBar);
            let (bar_w, color) = if hovered {
                (18.0, TEXT)
            } else {
                (10.0, SUBTLE)
            };
            let x = w - PAD - SCROLL_W / 2.0 + 4.0 - bar_w / 2.0;
            canvas.rect(x, track_top, bar_w, track, bar_w / 2.0, [0x2a, 0x2f, 0x38]);
            canvas.rect(x, top, bar_w, thumb, bar_w / 2.0, color);
        }
    }

    // Header: breadcrumbs (links except the last) and the build number.
    if view.form.is_none() {
        for (k, tool) in view.tools.iter().enumerate() {
            let (x, y, tw, th) = tool_rect(view, k);
            let hovered = hover == Some(Hit::Tool(k));
            let fill = match (tool.danger, tool.active, hovered) {
                (true, _, true) => [0xff, 0x6b, 0x60],
                (true, _, false) => RED,
                (false, true, true) => [0x6b, 0xa0, 0xff],
                (false, true, false) => ACCENT,
                (false, false, true) => HOVER,
                (false, false, false) => [0x2a, 0x2f, 0x38],
            };
            canvas.rect(x, y, tw, th, 14.0, fill);
            let (cx, cy) = (x + tw / 2.0, y + th / 2.0);
            match tool.icon {
                Some(ToolIcon::Edit) => draw_pencil(&mut canvas, cx, cy, TEXT),
                Some(ToolIcon::Select) => {
                    // A ticked box beside two list lines.
                    draw_checkbox(&mut canvas, cx - 12.0, cy, true);
                    canvas.rect(cx + 12.0, cy - 12.0, 16.0, 5.0, 2.0, TEXT);
                    canvas.rect(cx + 12.0, cy + 7.0, 16.0, 5.0, 2.0, TEXT);
                }
                None => {
                    let lw = fonts.measure(&tool.label, 30.0);
                    fonts.draw(
                        &mut canvas,
                        &tool.label,
                        cx - lw / 2.0,
                        cy + 11.0,
                        30.0,
                        TEXT,
                        tw,
                    );
                }
            }
        }
        let limit = crumb_limit(view, fonts);
        let last = view.crumbs.len().saturating_sub(1);
        for (i, crumb) in view.crumbs.iter().enumerate() {
            let (x0, x1) = crumbs[i];
            let hovered = hover == Some(Hit::Crumb(i));
            if hovered {
                canvas.rect(x0 - 10.0, 34.0, x1 - x0 + 20.0, 60.0, 10.0, HOVER);
            }
            let color = if i == last || hovered { TEXT } else { SUBTLE };
            fonts.draw(&mut canvas, crumb, x0, 78.0, CRUMB_SIZE, color, limit);
            if i < last {
                fonts.draw(&mut canvas, CRUMB_SEP, x1, 78.0, CRUMB_SIZE, FAINT, 200.0);
            }
        }
    }
    // Above the tools, so both fit.
    let build_w = fonts.measure(BUILD, 20.0);
    let build_y = if view.tools.is_empty() { 44.0 } else { 26.0 };
    fonts.draw(
        &mut canvas,
        BUILD,
        w - PAD - build_w,
        build_y,
        20.0,
        FAINT,
        build_w + 2.0,
    );
    if view.form.is_none() {
        canvas.rect(
            PAD,
            HEADER - 8.0,
            w - 2.0 * PAD,
            2.0,
            0.0,
            [0x2a, 0x2f, 0x38],
        );
    }
    if let Some(notice) = &view.notice {
        canvas.rect(0.0, HEIGHT as f32 - 60.0, w, 60.0, 0.0, BG);
        fonts.draw(
            &mut canvas,
            notice,
            PAD,
            HEIGHT as f32 - 22.0,
            26.0,
            SUBTLE,
            w - 2.0 * PAD,
        );
    }

    if let Some(dialog) = &view.dialog {
        // Dim the list behind the dialog.
        for px in canvas.pixels.chunks_exact_mut(4) {
            for c in &mut px[..3] {
                *c /= 3;
            }
        }
        let (dx, dy, dw, dh) = (160.0, 120.0, w - 320.0, HEIGHT as f32 - 240.0);
        canvas.rect(dx, dy, dw, dh, 24.0, [0x22, 0x26, 0x2e]);
        fonts.draw(
            &mut canvas,
            &dialog.title,
            dx + 48.0,
            dy + 84.0,
            42.0,
            TEXT,
            dw - 96.0,
        );
        let mut y = dy + 150.0;
        for paragraph in &dialog.body {
            for line in fonts.wrap(paragraph, 30.0, dw - 96.0) {
                fonts.draw(&mut canvas, &line, dx + 48.0, y, 30.0, SUBTLE, dw - 96.0);
                y += 44.0;
            }
            y += 16.0;
        }
        let rects = dialog_buttons(dialog.buttons.len());
        for (i, (label, rect)) in dialog.buttons.iter().zip(&rects).enumerate() {
            let primary = i + 1 == dialog.buttons.len();
            let hovered = hover == Some(Hit::DialogButton(i));
            let color = match (primary, dialog.danger, hovered) {
                (true, true, true) => [0xff, 0x6b, 0x60],
                (true, true, false) => RED,
                (true, false, true) => [0x6b, 0xa0, 0xff],
                (true, false, false) => ACCENT,
                (false, _, true) => HOVER,
                (false, _, false) => [0x2a, 0x2f, 0x38],
            };
            canvas.rect(rect.0, rect.1, rect.2, rect.3, 16.0, color);
            let label_w = fonts.measure(label, 34.0);
            fonts.draw(
                &mut canvas,
                label,
                rect.0 + (rect.2 - label_w) / 2.0,
                rect.1 + 53.0,
                34.0,
                TEXT,
                rect.2,
            );
        }
    }

    if let Some((x, y)) = pointer.filter(|_| draw_cursor) {
        canvas.circle(x, y, 12.0, [0xff, 0xff, 0xff]);
        canvas.circle(x, y, 8.0, ACCENT);
    }
    canvas
}

/// Human-readable size, e.g. "4.7 GB".
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            crumbs: vec!["Just Video".into(), "PC".into(), "media".into()],
            rows: (0..30)
                .map(|i| Row::new(Icon::Folder, format!("{i}")))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn hit_testing_follows_scroll_and_dialog() {
        let mut view = view();
        let mut fonts = Fonts::load().expect("fonts");
        let spans = crumb_spans(&view, &mut fonts);
        let last = (spans[2].0 + spans[2].1) / 2.0;
        assert_eq!(hit(&view, &mut fonts, 400.0, HEADER + 10.0), Hit::Row(0));
        view.scroll = 5.0;
        assert_eq!(
            hit(&view, &mut fonts, 400.0, HEADER + ROW + 10.0),
            Hit::Row(6)
        );
        assert_eq!(hit(&view, &mut fonts, 100.0, 70.0), Hit::Crumb(0));
        assert_eq!(
            hit(&view, &mut fonts, last, 70.0),
            Hit::Nothing,
            "current place is not a link"
        );
        view.dialog = Some(Dialog {
            title: String::new(),
            body: vec![],
            buttons: vec!["Cancel".into(), "Delete".into()],
            danger: true,
        });
        let rects = dialog_buttons(2);
        let (x, y, w, h) = rects[1];
        assert_eq!(
            hit(&view, &mut fonts, x + w / 2.0, y + h / 2.0),
            Hit::DialogButton(1)
        );
        assert_eq!(hit(&view, &mut fonts, 400.0, HEADER + 10.0), Hit::Nothing);
    }

    #[test]
    fn icon_cut_outs_show_the_row_behind() {
        let view = View {
            rows: vec![Row::new(Icon::Settings, "Settings")],
            ..Default::default()
        };
        let mut fonts = Fonts::load().expect("fonts");
        let (_, ry, _, _) = row_rect(&view, 0);
        let (cx, cy) = (PAD + 48.0, ry - 4.0 + ROW / 2.0);
        let centre = |canvas: &Canvas| {
            let i = ((cy as u32 * canvas.width + cx as u32) * 4) as usize;
            [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
        };
        assert_eq!(centre(&render(&view, &mut fonts, None, false)), ROW_BG);
        let hovered = render(&view, &mut fonts, Some((400.0, cy)), false);
        assert_eq!(centre(&hovered), HOVER, "the gear's hole");
    }

    #[test]
    fn scrollbar_is_not_a_row() {
        let view = view();
        let x = WIDTH as f32 - PAD - SCROLL_W / 2.0;
        let mut fonts = Fonts::load().expect("fonts");
        assert_eq!(hit(&view, &mut fonts, x, HEADER + 10.0), Hit::ScrollBar);
        assert_eq!(scroll_at(&view, 0.0), 0.0);
        assert_eq!(scroll_at(&view, HEIGHT as f32), view.max_scroll());
        let short = View {
            rows: view.rows[..3].to_vec(),
            ..view.clone()
        };
        assert_eq!(hit(&short, &mut fonts, x, HEADER + 10.0), Hit::Nothing);
    }

    #[test]
    fn locks_actions_and_tools() {
        let mut view = view();
        let mut fonts = Fonts::load().expect("fonts");
        // A server row: Edit and Remove beside its lock, locked or not.
        view.rows[2].lock = Some(false);
        view.rows[2].actions = vec![Action::Edit, Action::Remove];
        let (x, y, w, h) = lock_rect(&view, 2);
        assert_eq!(
            hit(&view, &mut fonts, x + w / 2.0, y + h / 2.0),
            Hit::Lock(2)
        );
        for (k, action) in [Action::Edit, Action::Remove].into_iter().enumerate() {
            let (ax, ay, aw, ah) = action_rect(&view, 2, k);
            assert_eq!(
                hit(&view, &mut fonts, ax + aw / 2.0, ay + ah / 2.0),
                Hit::RowAction(2, action)
            );
        }
        let (ax, ay, aw, ah) = action_rect(&view, 2, 0);
        assert_eq!(
            hit(&view, &mut fonts, ax + aw / 2.0, ay + ah / 2.0 + ROW),
            Hit::Row(3),
            "no actions on row 3"
        );
        view.tools = vec![
            Tool::icon(ToolIcon::Edit, true),
            Tool::text("Cancel", false),
            Tool::text("Delete 2", true),
        ];
        for k in 0..3 {
            let (tx, ty, tw, th) = tool_rect(&view, k);
            assert_eq!(
                hit(&view, &mut fonts, tx + tw / 2.0, ty + th / 2.0),
                Hit::Tool(k)
            );
        }
        let (tx, _, tw, _) = tool_rect(&view, 2);
        assert!(tx + tw <= WIDTH as f32 - PAD + 0.5);
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(5_097_390_883), "5.1 GB");
    }
}
