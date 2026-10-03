//! Moving around the browser panel with the D-pad: one item has the focus,
//! drawn as if pointed at, and A (or the trigger) clicks it. In the list,
//! up/down go row by row (scrolling as needed) and left/right through a row's
//! buttons; elsewhere (header, dialogs, the keyboard) focus moves to the
//! nearest item in that direction.

use super::browser::{self, Hit, Icon, Rect, View};
use super::canvas::Fonts;
use super::form;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    /// From the D-pad's step values (-1/+1, 0 for none): left/right first.
    pub fn from_steps(horizontal: i32, vertical: i32) -> Option<Dir> {
        match (horizontal.signum(), vertical.signum()) {
            (-1, _) => Some(Dir::Left),
            (1, _) => Some(Dir::Right),
            (_, 1) => Some(Dir::Up),
            (_, -1) => Some(Dir::Down),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Focus {
    pub hit: Hit,
    /// Where the item was (its point): the next move starts from here, and
    /// tells apart two items with the same hit.
    at: (f32, f32),
}

fn make((hit, rect): (Hit, Rect)) -> Focus {
    Focus {
        hit,
        at: point(hit, rect),
    }
}

/// The point that stands for an item: its centre, or a field's end (so a
/// click puts the caret after the text).
fn point(hit: Hit, (x, y, w, h): Rect) -> (f32, f32) {
    match hit {
        Hit::Form(form::Hit::Field(..)) => (x + w - 8.0, y + h / 2.0),
        _ => (x + w / 2.0, y + h / 2.0),
    }
}

fn row_of(hit: Hit) -> Option<usize> {
    match hit {
        Hit::Row(i) | Hit::Lock(i) | Hit::RowAction(i, _) => Some(i),
        _ => None,
    }
}

/// The list is what the D-pad moves in (no form, dialog or status message).
fn list_shown(view: &View) -> bool {
    view.form.is_none() && view.dialog.is_none() && view.status.is_none()
}

/// The first row at least partly in view.
fn first_visible(view: &View) -> usize {
    (view.scroll.ceil() as usize).min(view.rows.len().saturating_sub(1))
}

fn row_focus(view: &View, i: usize, column: usize) -> Option<Focus> {
    let across = browser::row_targets(view, i);
    across
        .get(column.min(across.len().saturating_sub(1)))
        .copied()
        .map(make)
}

/// Where focus starts on this view: the safe button of a dialog, the
/// keyboard's first letter, the row just come back out of, or the first row.
pub fn initial(view: &View, fonts: &mut Fonts) -> Option<Focus> {
    let fixed = browser::fixed_targets(view, fonts);
    if view.form.is_some() {
        let key = fixed
            .iter()
            .find(|(h, _)| matches!(h, Hit::Form(form::Hit::Key(form::Key::Char(_)))));
        return key.or(fixed.first()).copied().map(make);
    }
    if let Some(dialog) = &view.dialog {
        // Never a destructive button first: mashing A must not delete.
        let i = if dialog.danger {
            0
        } else {
            dialog.buttons.len().saturating_sub(1)
        };
        return fixed.get(i).copied().map(make);
    }
    if list_shown(view) && !view.rows.is_empty() {
        let i = view
            .rows
            .iter()
            .position(|r| r.outlined)
            .unwrap_or_else(|| first_visible(view));
        return row_focus(view, i, 0);
    }
    fixed.last().copied().map(make)
}

/// Whether `focus` still names something on this view.
pub fn valid(view: &View, fonts: &mut Fonts, focus: &Focus) -> bool {
    match row_of(focus.hit) {
        Some(i) => {
            list_shown(view)
                && browser::row_targets(view, i)
                    .iter()
                    .any(|(h, _)| *h == focus.hit)
        }
        None => browser::fixed_targets(view, fonts)
            .iter()
            .any(|(h, _)| *h == focus.hit),
    }
}

/// Right on a folder (or share) row opens it, as clicking would. Rows with
/// buttons (edit mode, servers) or checkboxes move to the buttons instead.
pub fn opens_on_right(view: &View, focus: &Focus) -> Option<Hit> {
    let Hit::Row(i) = focus.hit else {
        return None;
    };
    let row = view.rows.get(i).filter(|_| list_shown(view))?;
    let container = matches!(row.icon, Icon::Folder | Icon::Share | Icon::Server);
    let plain = row.actions.is_empty() && row.lock.is_none() && row.checked.is_none();
    (container && plain && !row.dimmed).then_some(focus.hit)
}

/// After the view changed under the focus (e.g. Shift relabels the keys, a
/// row went away): the item now nearest to where it was.
pub fn refind(view: &View, fonts: &mut Fonts, old: &Focus) -> Option<Focus> {
    if let Some(i) = row_of(old.hit).filter(|_| list_shown(view) && !view.rows.is_empty()) {
        return row_focus(view, i.min(view.rows.len() - 1), 0);
    }
    let distance = |f: &Focus| (f.at.0 - old.at.0).powi(2) + (f.at.1 - old.at.1).powi(2);
    browser::fixed_targets(view, fonts)
        .into_iter()
        .map(make)
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        .or_else(|| initial(view, fonts))
}

/// Focus after a D-pad press (unchanged at an edge).
pub fn step(view: &View, fonts: &mut Fonts, from: Focus, dir: Dir) -> Focus {
    let fixed = browser::fixed_targets(view, fonts);
    if let Some(i) = row_of(from.hit).filter(|_| list_shown(view)) {
        let across = browser::row_targets(view, i);
        let column = across.iter().position(|(h, _)| *h == from.hit).unwrap_or(0);
        let next = match dir {
            Dir::Left => column.checked_sub(1).and_then(|c| row_focus(view, i, c)),
            Dir::Right => across.get(column + 1).copied().map(make),
            Dir::Up if i == 0 => {
                // Out of the list into the header: the item above, or the last.
                let above = (from.at.0, browser::list_top());
                nearest(&fixed, above, Dir::Up).or(fixed.last().copied().map(make))
            }
            Dir::Up => row_focus(view, i - 1, column),
            Dir::Down => row_focus(view, i + 1, column),
        };
        return next.unwrap_or(from);
    }
    if list_shown(view) && dir == Dir::Down && !view.rows.is_empty() {
        // Header to list.
        if let Some(f) = row_focus(view, first_visible(view), 0) {
            return f;
        }
    }
    nearest(&fixed, from.at, dir).unwrap_or(from)
}

fn nearest(targets: &[(Hit, Rect)], from: (f32, f32), dir: Dir) -> Option<Focus> {
    let points: Vec<(f32, f32)> = targets.iter().map(|&(h, r)| point(h, r)).collect();
    nearest_point(&points, from, dir).map(|i| make(targets[i]))
}

/// Of `points`, the nearest to `from` in direction `dir`, keeping to the same
/// row (or column) where there is one.
pub fn nearest_point(points: &[(f32, f32)], from: (f32, f32), dir: Dir) -> Option<usize> {
    points
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let (dx, dy) = (p.0 - from.0, p.1 - from.1);
            let (along, across) = match dir {
                Dir::Left => (-dx, dy),
                Dir::Right => (dx, dy),
                Dir::Up => (-dy, dx),
                Dir::Down => (dy, dx),
            };
            (along > 1.0).then_some((along + 10.0 * across.abs(), i))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, i)| i)
}

/// Of `points`, the one closest to `to`.
pub fn closest_point(points: &[(f32, f32)], to: (f32, f32)) -> Option<usize> {
    let distance = |p: &(f32, f32)| (p.0 - to.0).powi(2) + (p.1 - to.1).powi(2);
    (0..points.len()).min_by(|&a, &b| distance(&points[a]).total_cmp(&distance(&points[b])))
}

/// Where to draw the focus (as a pointer position) on the view as it is now.
pub fn point_of(view: &View, fonts: &mut Fonts, focus: &Focus) -> Option<(f32, f32)> {
    let targets = match row_of(focus.hit) {
        Some(i) => browser::row_targets(view, i),
        None => browser::fixed_targets(view, fonts),
    };
    let distance = |p: (f32, f32)| (p.0 - focus.at.0).powi(2) + (p.1 - focus.at.1).powi(2);
    targets
        .into_iter()
        .filter(|(h, _)| *h == focus.hit)
        .map(|(h, r)| point(h, r))
        .min_by(|a, b| distance(*a).total_cmp(&distance(*b)))
}

/// The scroll that brings a focused row fully into view (None: already is).
pub fn scroll_to_show(view: &View, focus: &Focus) -> Option<f32> {
    let i = row_of(focus.hit)? as f32;
    let visible = browser::visible_rows().floor().max(1.0);
    if i < view.scroll {
        Some(i)
    } else if i + 1.0 > view.scroll + visible {
        Some((i + 1.0 - visible).min(view.max_scroll()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::browser::{Action, Dialog, Row, Tool};

    fn view(rows: usize) -> View {
        let mut v = View {
            crumbs: vec!["Servers".into(), "nas".into(), "Films".into()],
            tools: vec![Tool::text("Select", false)],
            ..View::default()
        };
        for i in 0..rows {
            let mut row = Row::new(Icon::Folder, format!("Folder {i}"));
            if i == 1 {
                row.actions = vec![Action::Rename, Action::Delete];
            }
            v.rows.push(row);
        }
        v
    }

    /// Clicking where the focus is drawn hits the focused item.
    fn round_trips(v: &View, fonts: &mut Fonts, f: &Focus) {
        let (x, y) = point_of(v, fonts, f).expect("drawn");
        assert_eq!(browser::hit(v, fonts, x, y), f.hit, "at {x},{y}");
    }

    #[test]
    fn list_moves_by_row_and_across_buttons() {
        let mut fonts = Fonts::load().expect("fonts");
        let v = view(30);
        let f = initial(&v, &mut fonts).unwrap();
        assert_eq!(f.hit, Hit::Row(0));
        let f = step(&v, &mut fonts, f, Dir::Down);
        assert_eq!(f.hit, Hit::Row(1));
        let f = step(&v, &mut fonts, f, Dir::Right);
        assert_eq!(f.hit, Hit::RowAction(1, Action::Rename));
        round_trips(&v, &mut fonts, &f);
        let f = step(&v, &mut fonts, f, Dir::Right);
        assert_eq!(f.hit, Hit::RowAction(1, Action::Delete));
        round_trips(&v, &mut fonts, &f);
        assert_eq!(step(&v, &mut fonts, f, Dir::Right), f, "edge");
        // Down to a row without buttons: the row itself.
        let f = step(&v, &mut fonts, f, Dir::Down);
        assert_eq!(f.hit, Hit::Row(2));
        round_trips(&v, &mut fonts, &f);
        // Up from the first row: the header; down again: back to the list.
        let first = initial(&v, &mut fonts).unwrap();
        let top = step(&v, &mut fonts, first, Dir::Up);
        assert!(matches!(top.hit, Hit::Crumb(_) | Hit::Tool(_)), "{top:?}");
        round_trips(&v, &mut fonts, &top);
        let left = step(&v, &mut fonts, top, Dir::Left);
        assert!(matches!(left.hit, Hit::Crumb(_)), "{left:?}");
        round_trips(&v, &mut fonts, &left);
        assert_eq!(step(&v, &mut fonts, top, Dir::Down).hit, Hit::Row(0));
    }

    #[test]
    fn right_opens_folders_but_reaches_buttons_in_edit_mode() {
        let mut fonts = Fonts::load().expect("fonts");
        let mut v = view(5);
        let first = initial(&v, &mut fonts).unwrap();
        assert_eq!(opens_on_right(&v, &first), Some(Hit::Row(0)));
        // Row 1 has Rename and Delete: right goes to them.
        let second = step(&v, &mut fonts, first, Dir::Down);
        assert_eq!(opens_on_right(&v, &second), None);
        v.rows[0].icon = Icon::Video(None);
        assert_eq!(opens_on_right(&v, &first), None, "videos open with A");
    }

    #[test]
    fn rows_out_of_view_scroll_in() {
        let mut fonts = Fonts::load().expect("fonts");
        let mut v = view(30);
        let mut f = initial(&v, &mut fonts).unwrap();
        for _ in 0..20 {
            f = step(&v, &mut fonts, f, Dir::Down);
            if let Some(scroll) = scroll_to_show(&v, &f) {
                v.scroll = scroll;
            }
            round_trips(&v, &mut fonts, &f);
        }
        assert_eq!(f.hit, Hit::Row(20));
        assert!(v.scroll > 0.0);
        let f = step(&v, &mut fonts, f, Dir::Up);
        assert_eq!(scroll_to_show(&v, &f), None, "row 19 is in view");
    }

    #[test]
    fn coming_back_focuses_the_folder_left() {
        let mut fonts = Fonts::load().expect("fonts");
        let mut v = view(5);
        v.rows[3].outlined = true;
        assert_eq!(initial(&v, &mut fonts).unwrap().hit, Hit::Row(3));
    }

    #[test]
    fn dialogs_start_on_the_safe_button() {
        let mut fonts = Fonts::load().expect("fonts");
        let mut v = view(5);
        v.dialog = Some(Dialog {
            title: "Delete?".into(),
            body: vec![],
            buttons: vec!["Cancel".into(), "Delete".into()],
            danger: true,
        });
        let f = initial(&v, &mut fonts).unwrap();
        assert_eq!(f.hit, Hit::DialogButton(0));
        assert!(!valid(
            &v,
            &mut fonts,
            &make((Hit::Row(0), (0.0, 0.0, 1.0, 1.0)))
        ));
        let f = step(&v, &mut fonts, f, Dir::Right);
        assert_eq!(f.hit, Hit::DialogButton(1));
        round_trips(&v, &mut fonts, &f);
        v.dialog.as_mut().unwrap().danger = false;
        assert_eq!(initial(&v, &mut fonts).unwrap().hit, Hit::DialogButton(1));
    }

    #[test]
    fn keyboard_moves_between_neighbouring_keys() {
        use form::{Field, Form, Key};
        let mut fonts = Fonts::load().expect("fonts");
        let mut v = view(5);
        v.form = Some(Form::new(
            "Rename",
            vec![Field {
                label: "Name".into(),
                value: "abc".into(),
                secret: false,
                placeholder: String::new(),
            }],
            "Save",
        ));
        let key = |f: &Focus| match f.hit {
            Hit::Form(form::Hit::Key(k)) => Some(k),
            _ => None,
        };
        let f = initial(&v, &mut fonts).unwrap();
        assert_eq!(key(&f), Some(Key::Char('1')));
        let f = step(&v, &mut fonts, f, Dir::Right);
        assert_eq!(key(&f), Some(Key::Char('2')));
        round_trips(&v, &mut fonts, &f);
        let f = step(&v, &mut fonts, f, Dir::Down);
        assert_eq!(key(&f), Some(Key::Char('w')));
        // Up from the top row: the field, with the caret at its end.
        let top_row = step(&v, &mut fonts, f, Dir::Up);
        let up = step(&v, &mut fonts, top_row, Dir::Up);
        assert_eq!(up.hit, Hit::Form(form::Hit::Field(0, 3)));
        round_trips(&v, &mut fonts, &up);
        // Down to the bottom row: Cancel, space and Save are all reachable.
        let mut f = f;
        for _ in 0..4 {
            f = step(&v, &mut fonts, f, Dir::Down);
        }
        assert_eq!(key(&f), Some(Key::Cancel), "below the left of the keyboard");
        let space = step(&v, &mut fonts, f, Dir::Right);
        assert_eq!(key(&space), Some(Key::Space));
        let save = step(&v, &mut fonts, space, Dir::Right);
        assert_eq!(key(&save), Some(Key::Submit));
        round_trips(&v, &mut fonts, &save);
        // Shift relabels the letters: the focus stays on the same key.
        let first = initial(&v, &mut fonts).unwrap();
        let w = step(&v, &mut fonts, first, Dir::Down);
        let w = step(&v, &mut fonts, w, Dir::Right);
        assert_eq!(key(&w), Some(Key::Char('w')));
        v.form.as_mut().unwrap().layer = form::Layer::Upper;
        assert!(!valid(&v, &mut fonts, &w));
        let big_w = refind(&v, &mut fonts, &w).unwrap();
        assert_eq!(key(&big_w), Some(Key::Char('W')));
    }
}
