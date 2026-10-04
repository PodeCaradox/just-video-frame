//! A small form (text fields) with an on-panel keyboard, drawn into the
//! browser panel and operated by pointing: used to add servers and rename files.

use super::canvas::{Canvas, Fonts, Rgb};

const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const FIELD: Rgb = [0x1d, 0x21, 0x28];
const FIELD_FOCUS: Rgb = [0x26, 0x2f, 0x40];
const KEY: Rgb = [0x2a, 0x2f, 0x38];
const KEY_HOVER: Rgb = [0x3a, 0x42, 0x50];
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const ERROR: Rgb = [0xff, 0x6b, 0x60];

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub label: String,
    pub value: String,
    /// Shown as dots (passwords).
    pub secret: bool,
    pub placeholder: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Lower,
    Upper,
    Symbols,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    pub focused: usize,
    /// Caret in the focused field, in characters from its start.
    pub cursor: usize,
    pub layer: Layer,
    pub error: Option<String>,
    pub busy: Option<String>,
    pub submit: String,
    /// A checkbox beside the last field: its label, and whether it's ticked.
    pub toggle: Option<(String, bool)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Shift,
    Symbols,
    Backspace,
    /// Caret one character left or right.
    Left,
    Right,
    Space,
    Cancel,
    Submit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A field, and the caret position under the pointer.
    Field(usize, usize),
    Key(Key),
    Toggle,
    Nothing,
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<Field>, submit: impl Into<String>) -> Self {
        let cursor = fields.first().map_or(0, |f| f.value.chars().count());
        Self {
            title: title.into(),
            fields,
            focused: 0,
            cursor,
            layer: Layer::Lower,
            error: None,
            busy: None,
            submit: submit.into(),
            toggle: None,
        }
    }

    pub fn value(&self, i: usize) -> &str {
        &self.fields[i].value
    }

    /// Focuses field `i` with the caret at `cursor` (None: at the end).
    pub fn focus(&mut self, i: usize, cursor: Option<usize>) {
        self.focused = i;
        let len = self.fields[i].value.chars().count();
        self.cursor = cursor.unwrap_or(len).min(len);
    }

    /// Applies a key; returns Cancel/Submit for the owner to act on.
    pub fn press(&mut self, key: Key) -> Option<Key> {
        self.error = None;
        let field = &mut self.fields[self.focused];
        let len = field.value.chars().count();
        self.cursor = self.cursor.min(len);
        // Byte offset of the caret.
        let at = |value: &str, chars: usize| {
            value
                .char_indices()
                .nth(chars)
                .map_or(value.len(), |(i, _)| i)
        };
        match key {
            Key::Char(_) | Key::Space => {
                let c = match key {
                    Key::Char(c) => c,
                    _ => ' ',
                };
                field.value.insert(at(&field.value, self.cursor), c);
                self.cursor += 1;
                if key != Key::Space && self.layer == Layer::Upper {
                    self.layer = Layer::Lower; // one-shot shift
                }
            }
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                field.value.remove(at(&field.value, self.cursor));
            }
            Key::Backspace => {}
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(len),
            Key::Shift => {
                self.layer = if self.layer == Layer::Upper {
                    Layer::Lower
                } else {
                    Layer::Upper
                };
            }
            Key::Symbols => {
                self.layer = if self.layer == Layer::Symbols {
                    Layer::Lower
                } else {
                    Layer::Symbols
                };
            }
            Key::Cancel | Key::Submit => return Some(key),
        }
        None
    }
}

// Layout inside the 1600×1000 browser panel.
const X0: f32 = 32.0;
const FIELD_Y: f32 = 132.0;
const FIELD_H: f32 = 72.0;
const FIELD_GAP: f32 = 10.0;
const KEY_H: f32 = 84.0;
const KEY_GAP: f32 = 10.0;

fn rows(layer: Layer) -> [&'static str; 4] {
    match layer {
        Layer::Lower => ["1234567890", "qwertyuiop", "asdfghjkl@", "zxcvbnm.-_"],
        Layer::Upper => ["1234567890", "QWERTYUIOP", "ASDFGHJKL@", "ZXCVBNM.-_"],
        Layer::Symbols => ["!#$%&*()+=", "\\/:;,'\"?~^", "[]{}<>|`€£", "§°¨´¤½.-_ "],
    }
}

pub type Rect = (f32, f32, f32, f32);

fn keyboard_top(form: &Form) -> f32 {
    FIELD_Y + form.fields.len() as f32 * (FIELD_H + FIELD_GAP) + 24.0
}

/// Room the toggle takes from the right of the last field.
const TOGGLE_W: f32 = 520.0;

fn field_rect(form: &Form, i: usize) -> Rect {
    let beside_toggle = form.toggle.is_some() && i + 1 == form.fields.len();
    (
        X0 + 260.0,
        FIELD_Y + i as f32 * (FIELD_H + FIELD_GAP),
        1600.0 - 2.0 * X0 - 260.0 - if beside_toggle { TOGGLE_W + 16.0 } else { 0.0 },
        FIELD_H,
    )
}

fn toggle_rect(form: &Form) -> Option<Rect> {
    form.toggle.as_ref()?;
    let (_, y, _, h) = field_rect(form, form.fields.len().checked_sub(1)?);
    Some((1600.0 - X0 - TOGGLE_W, y, TOGGLE_W, h))
}

/// Every key with its rectangle.
fn keys(form: &Form, panel_width: f32) -> Vec<(Key, Rect)> {
    let top = keyboard_top(form);
    let width = panel_width - 2.0 * X0;
    let mut out = Vec::new();
    // Four character rows of 10 keys; the wide keys sit on the sides.
    for (r, chars) in rows(form.layer).iter().enumerate() {
        let y = top + r as f32 * (KEY_H + KEY_GAP);
        let side = 180.0;
        let key_w = (width - 2.0 * (side + KEY_GAP) - 9.0 * KEY_GAP) / 10.0;
        let (left, right) = match r {
            0 => (None, Some(Key::Backspace)),
            1 => (Some(Key::Left), Some(Key::Right)),
            2 => (Some(Key::Shift), None),
            3 => (Some(Key::Symbols), None),
            _ => (None, None),
        };
        if let Some(k) = left {
            out.push((k, (X0, y, side, KEY_H)));
        }
        for (i, c) in chars.chars().enumerate() {
            let x = X0 + side + KEY_GAP + i as f32 * (key_w + KEY_GAP);
            out.push((
                if c == ' ' { Key::Space } else { Key::Char(c) },
                (x, y, key_w, KEY_H),
            ));
        }
        if let Some(k) = right {
            out.push((k, (X0 + width - side, y, side, KEY_H)));
        }
    }
    // Bottom row: cancel, space, submit.
    let y = top + 4.0 * (KEY_H + KEY_GAP);
    out.push((Key::Cancel, (X0, y, 300.0, KEY_H)));
    out.push((Key::Space, (X0 + 310.0, y, width - 620.0, KEY_H)));
    out.push((Key::Submit, (X0 + width - 300.0, y, 300.0, KEY_H)));
    out
}

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

/// The text shown in a field (dots for secrets).
fn shown(field: &Field) -> Vec<char> {
    if field.secret {
        vec!['•'; field.value.chars().count()]
    } else {
        field.value.chars().collect()
    }
}

const TEXT_SIZE: f32 = 34.0;

/// First character shown in field `i`: from the start if it all fits,
/// otherwise so the end is visible, or, if the caret is further left,
/// from the caret. Clicking inside the shown text never scrolls it.
fn first_shown(form: &Form, i: usize, fonts: &mut Fonts) -> usize {
    let chars = shown(&form.fields[i]);
    let room = field_rect(form, i).2 - 60.0;
    let mut start = 0;
    while start + 1 < chars.len()
        && fonts.measure(&chars[start..].iter().collect::<String>(), TEXT_SIZE) > room
    {
        start += 1;
    }
    if i == form.focused {
        start.min(form.cursor)
    } else {
        start
    }
}

/// The caret position nearest to canvas x in field `i`.
fn caret_at(form: &Form, i: usize, x: f32, fonts: &mut Fonts) -> usize {
    let chars = shown(&form.fields[i]);
    let start = first_shown(form, i, fonts);
    let mut left = field_rect(form, i).0 + 20.0;
    for (k, c) in chars.iter().enumerate().skip(start) {
        let w = fonts.measure(&c.to_string(), TEXT_SIZE);
        if x < left + w / 2.0 {
            return k;
        }
        left += w;
    }
    chars.len()
}

/// What the D-pad moves between: the fields (caret at the end) and the keys.
pub fn targets(form: &Form, panel_width: f32) -> Vec<(Hit, Rect)> {
    if form.busy.is_some() {
        return Vec::new();
    }
    let fields = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (Hit::Field(i, f.value.chars().count()), field_rect(form, i)));
    let toggle = toggle_rect(form).map(|r| (Hit::Toggle, r));
    let keys = keys(form, panel_width)
        .into_iter()
        .map(|(k, r)| (Hit::Key(k), r));
    fields.chain(toggle).chain(keys).collect()
}

pub fn hit(form: &Form, fonts: &mut Fonts, panel_width: f32, x: f32, y: f32) -> Hit {
    if form.busy.is_some() {
        return Hit::Nothing;
    }
    for i in 0..form.fields.len() {
        if inside(field_rect(form, i), x, y) {
            return Hit::Field(i, caret_at(form, i, x, fonts));
        }
    }
    if toggle_rect(form).is_some_and(|r| inside(r, x, y)) {
        return Hit::Toggle;
    }
    keys(form, panel_width)
        .into_iter()
        .find(|(_, r)| inside(*r, x, y))
        .map_or(Hit::Nothing, |(k, _)| Hit::Key(k))
}

fn key_label(key: Key, form: &Form) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::Shift => {
            if form.layer == Layer::Upper {
                "SHIFT".into()
            } else {
                "Shift".into()
            }
        }
        Key::Symbols => {
            if form.layer == Layer::Symbols {
                "abc".into()
            } else {
                "#+=".into()
            }
        }
        Key::Backspace => "Delete".into(),
        // Drawn as triangles.
        Key::Left | Key::Right => String::new(),
        Key::Space => "space".into(),
        Key::Cancel => "Cancel".into(),
        Key::Submit => form.submit.clone(),
    }
}

pub fn render(canvas: &mut Canvas, fonts: &mut Fonts, form: &Form, hover: Hit) {
    let w = canvas.width as f32;
    for (i, field) in form.fields.iter().enumerate() {
        let (fx, fy, fw, fh) = field_rect(form, i);
        fonts.draw(canvas, &field.label, X0, fy + 47.0, 30.0, SUBTLE, 250.0);
        let focused = i == form.focused;
        let hovered = matches!(hover, Hit::Field(f, _) if f == i);
        canvas.rect(
            fx,
            fy,
            fw,
            fh,
            12.0,
            if focused {
                FIELD_FOCUS
            } else if hovered {
                KEY_HOVER
            } else {
                FIELD
            },
        );
        if focused {
            canvas.rect(fx, fy + fh - 4.0, fw, 4.0, 2.0, ACCENT);
        }
        let chars = shown(field);
        if chars.is_empty() {
            fonts.draw(
                canvas,
                &field.placeholder,
                fx + 20.0,
                fy + 48.0,
                32.0,
                [0x5f, 0x63, 0x68],
                fw - 40.0,
            );
        }
        let start = first_shown(form, i, fonts);
        let text: String = chars[start..].iter().collect();
        fonts.draw(
            canvas,
            &text,
            fx + 20.0,
            fy + 48.0,
            TEXT_SIZE,
            TEXT,
            fw - 40.0,
        );
        if focused {
            let before: String = chars[start..form.cursor.clamp(start, chars.len())]
                .iter()
                .collect();
            let x = fx + 20.0 + fonts.measure(&before, TEXT_SIZE);
            canvas.rect(x, fy + 16.0, 3.0, fh - 32.0, 1.0, ACCENT);
        }
    }
    if let (Some((label, on)), Some((x, y, tw, th))) = (&form.toggle, toggle_rect(form)) {
        let fill = if hover == Hit::Toggle {
            KEY_HOVER
        } else {
            FIELD
        };
        canvas.rect(x, y, tw, th, 12.0, fill);
        let (cx, cy) = (x + 40.0, y + th / 2.0);
        super::browser::draw_checkbox(canvas, cx, cy, *on, ACCENT, fill);
        fonts.draw(canvas, label, x + 76.0, y + 46.0, 28.0, TEXT, tw - 90.0);
    }
    let status_y = keyboard_top(form) - 6.0;
    if let Some(busy) = &form.busy {
        fonts.draw(canvas, busy, X0, status_y, 26.0, SUBTLE, w - 2.0 * X0);
    } else if let Some(error) = &form.error {
        fonts.draw(canvas, error, X0, status_y, 26.0, ERROR, w - 2.0 * X0);
    }
    for (key, (x, y, kw, kh)) in keys(form, w) {
        let hovered = hover == Hit::Key(key);
        let color = match key {
            Key::Submit => {
                if hovered {
                    [0x6b, 0xa0, 0xff]
                } else {
                    ACCENT
                }
            }
            _ if hovered => KEY_HOVER,
            _ => KEY,
        };
        canvas.rect(x, y, kw, kh, 12.0, color);
        let label = key_label(key, form);
        let size = if matches!(key, Key::Char(_)) {
            40.0
        } else {
            32.0
        };
        if matches!(key, Key::Left | Key::Right) {
            // A triangle from vertical strips, pointing its way.
            let (cx, cy) = (x + kw / 2.0, y + kh / 2.0);
            for i in 0..28 {
                let t = i as f32;
                let half = 16.0 * t / 28.0;
                let px = if key == Key::Left {
                    cx - 14.0 + t
                } else {
                    cx + 14.0 - t
                };
                canvas.rect(px, cy - half, 1.5, half * 2.0, 0.0, TEXT);
            }
        }
        let lw = fonts.measure(&label, size);
        fonts.draw(
            canvas,
            &label,
            x + (kw - lw) / 2.0,
            y + kh / 2.0 + size * 0.36,
            size,
            TEXT,
            kw,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> Form {
        Form::new(
            "Add server",
            vec![
                Field {
                    label: "Address".into(),
                    value: String::new(),
                    secret: false,
                    placeholder: String::new(),
                },
                Field {
                    label: "Password".into(),
                    value: String::new(),
                    secret: true,
                    placeholder: String::new(),
                },
            ],
            "Save",
        )
    }

    #[test]
    fn typing_shift_and_backspace() {
        let mut f = form();
        f.press(Key::Shift);
        f.press(Key::Char('N'));
        f.press(Key::Char('a'));
        f.press(Key::Char('s'));
        f.press(Key::Backspace);
        assert_eq!(f.value(0), "Na");
        assert_eq!(f.layer, Layer::Lower, "shift is one-shot");
        assert_eq!(f.press(Key::Submit), Some(Key::Submit));
    }

    #[test]
    fn editing_in_the_middle() {
        let mut f = form();
        f.fields[0].value = "clip.mp4".into();
        f.focus(0, Some(4));
        f.press(Key::Char('2'));
        assert_eq!(f.value(0), "clip2.mp4");
        f.press(Key::Left);
        f.press(Key::Backspace);
        assert_eq!(f.value(0), "cli2.mp4");
        assert_eq!(f.cursor, 3);
        f.press(Key::Right);
        f.press(Key::Right);
        f.press(Key::Space);
        assert_eq!(f.value(0), "cli2. mp4");
        f.focus(0, Some(99));
        assert_eq!(f.cursor, 9, "clamped to the end");
        f.fields[0].value = "åäö".into();
        f.focus(0, Some(1));
        f.press(Key::Backspace);
        assert_eq!(f.value(0), "äö", "characters, not bytes");
    }

    #[test]
    fn every_key_is_hittable_and_distinct() {
        let f = form();
        let mut fonts = Fonts::load().expect("fonts");
        for (key, (x, y, w, h)) in keys(&f, 1600.0) {
            assert_eq!(
                hit(&f, &mut fonts, 1600.0, x + w / 2.0, y + h / 2.0),
                Hit::Key(key),
                "{key:?}"
            );
            assert!(y + h <= 1000.0, "{key:?} below the panel");
        }
    }
}
