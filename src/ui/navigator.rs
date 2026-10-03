//! Browser state: servers → shares → folders → videos. Talks to the
//! [`Library`] workers and turns their results into a [`View`].
//!
//! Each saved server can always be edited or removed from the server list.
//! Changing files is opt-in per server: its lock (on the server list) allows,
//! inside it, Rename on every entry and a "Select" tool for deleting several
//! entries at once.
//!
//! The server list also leads to the Settings screen ([`super::settings`]).

use super::browser::{Action, Dialog, Hit, Icon, Row, Tool, ToolIcon, View, format_size};
use super::form::{self, Field, Form, Key};
use super::settings::{self, Setting};
use crate::config::{self, Server};
use crate::library::{Library, Opened, Path, Request, Response, file_key};
use crate::playability::{Assessment, Verdict};
use crate::smb::SmbUrl;
use crate::vr::Layout;
use std::collections::HashSet;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mkv", "mov", "webm", "avi", "ts", "m2ts"];

pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Names from the top down to a place: server URL, share, folders.
fn trail(location: &Location) -> Vec<String> {
    match location {
        Location::Servers => Vec::new(),
        Location::Settings => vec!["Settings".into()],
        Location::Shares { server } => vec![server.url.clone()],
        Location::Folder {
            server,
            share,
            path,
        } => [server.url.clone(), share.clone()]
            .into_iter()
            .chain(path.iter().cloned())
            .collect(),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Location {
    Servers,
    Settings,
    Shares {
        server: Server,
    },
    Folder {
        server: Server,
        share: String,
        path: Path,
    },
}

enum Item {
    Server(Server),
    AddServer,
    /// Opens the Settings screen.
    Settings,
    Setting(Setting),
    Share(String),
    Dir(String),
    Video {
        name: String,
        size: u64,
        assessment: Option<Assessment>,
        /// How it will be shown (once probed).
        layout: Option<Layout>,
        broken: Option<String>,
    },
    /// A file that isn't a video: shown dimmed, can be renamed or deleted.
    File {
        name: String,
        size: u64,
    },
}

impl Item {
    fn name(&self) -> &str {
        match self {
            Item::Server(s) => &s.name,
            Item::AddServer | Item::Setting(_) => "",
            Item::Settings => "Settings",
            Item::Share(n)
            | Item::Dir(n)
            | Item::Video { name: n, .. }
            | Item::File { name: n, .. } => n,
        }
    }

    /// What [`trail`] calls this item.
    fn trail_name(&self) -> &str {
        match self {
            Item::Server(s) => &s.url,
            other => other.name(),
        }
    }

    fn is_entry(&self) -> bool {
        matches!(self, Item::Dir(_) | Item::Video { .. } | Item::File { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ToolAction {
    ToggleEdit,
    StartSelect,
    CancelSelect,
    DeleteSelected,
}

/// What the open form or dialog is for.
enum Purpose {
    AddServer,
    /// Editing the server with this URL.
    EditServer(String),
    Rename(usize),
    RemoveServer(usize),
    /// Deleting these entries.
    Delete(Vec<usize>),
    Info,
}

pub struct Navigator {
    library: Library,
    location: Location,
    items: Vec<Item>,
    view: View,
    purpose: Option<Purpose>,
    /// URLs of servers whose lock is open (this session only).
    unlocked: HashSet<String>,
    /// Selecting entries to delete: their indexes.
    selecting: Option<HashSet<usize>>,
    /// Edit mode (inside an unlocked server): rename/delete on each row.
    edit_mode: bool,
    /// What each header tool does.
    tool_actions: Vec<ToolAction>,
    /// The item being played (for previous/next).
    playing: Option<usize>,
    /// Navigation request whose answer we wait for.
    pending: Option<u64>,
    /// Rename/delete/add requests whose answers we wait for.
    pending_changes: HashSet<u64>,
    change_errors: Vec<String>,
    /// Scroll position of each place left, by its [`trail`].
    scrolls: std::collections::HashMap<Vec<String>, f32>,
    /// After going up: the entry we came out of (outlined) and the scroll
    /// to return to once the list has loaded.
    came_from: Option<String>,
    return_scroll: Option<f32>,
    next_id: u64,
    generation: u64,
    dirty: bool,
    /// As last read or saved, for the Settings screen.
    prefs: config::Preferences,
    /// A setting changed since [`Navigator::take_preferences`].
    prefs_changed: bool,
}

impl Navigator {
    pub fn new(library: Library) -> Self {
        let mut nav = Self {
            library,
            location: Location::Servers,
            items: Vec::new(),
            view: View::default(),
            purpose: None,
            unlocked: HashSet::new(),
            selecting: None,
            edit_mode: false,
            tool_actions: Vec::new(),
            playing: None,
            pending: None,
            pending_changes: HashSet::new(),
            change_errors: Vec::new(),
            scrolls: Default::default(),
            came_from: None,
            return_scroll: None,
            next_id: 1,
            generation: 0,
            dirty: true,
            prefs: config::Preferences::default(),
            prefs_changed: false,
        };
        nav.show_servers();
        nav
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    /// True (once) when the view changed and must be redrawn.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    pub fn scroll(&self) -> f32 {
        self.view.scroll
    }

    pub fn set_scroll(&mut self, scroll: f32) {
        if self.view.form.is_some() || self.view.dialog.is_some() {
            return;
        }
        let before = self.view.scroll;
        self.view.scroll = scroll;
        self.view.clamp_scroll();
        if self.view.scroll != before {
            self.dirty = true;
        }
    }

    pub fn scroll_by(&mut self, rows: f32) {
        self.set_scroll(self.view.scroll + rows);
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Changes are allowed here (the server's lock is open, inside a share).
    fn editing(&self) -> bool {
        matches!(&self.location, Location::Folder { server, .. } if self.unlocked.contains(&server.url))
    }

    fn crumbs(&self) -> Vec<String> {
        let mut crumbs = vec!["Just Video".to_string()];
        match &self.location {
            Location::Servers => {}
            Location::Settings => crumbs.push("Settings".into()),
            Location::Shares { server } => crumbs.push(server.name.clone()),
            Location::Folder {
                server,
                share,
                path,
            } => {
                crumbs.push(server.name.clone());
                crumbs.push(share.clone());
                crumbs.extend(path.iter().cloned());
            }
        }
        crumbs
    }

    fn reset_view(&mut self) {
        self.view = View {
            crumbs: self.crumbs(),
            ..Default::default()
        };
        self.purpose = None;
        self.selecting = None;
        self.dirty = true;
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.view.status = Some(status.into());
        self.view.rows.clear();
        self.dirty = true;
    }

    /// Before moving to `to`: remembers this list's scroll and, when going
    /// up, which entry we are leaving and where that list was scrolled.
    fn leave(&mut self, to: &Location) {
        if *to == self.location {
            return;
        }
        let (from, to) = (trail(&self.location), trail(to));
        if !self.items.is_empty() {
            self.scrolls.insert(from.clone(), self.view.scroll);
        }
        if to.len() < from.len() && from.starts_with(&to) {
            self.came_from = Some(from[to.len()].clone());
            self.return_scroll = self.scrolls.get(&to).copied();
        } else {
            self.came_from = None;
            self.return_scroll = None;
        }
    }

    /// Once a list has loaded: back to its old scroll, with the entry we
    /// came out of in view.
    fn restore_scroll(&mut self) {
        if let Some(scroll) = self.return_scroll.take() {
            self.view.scroll = scroll;
        }
        let Some(name) = &self.came_from else { return };
        if let Some(i) = self.items.iter().position(|it| it.trail_name() == name) {
            let (i, visible) = (i as f32, super::browser::visible_rows());
            if i < self.view.scroll || i + 1.0 > self.view.scroll + visible {
                self.view.scroll = i - (visible / 2.0).floor();
            }
        }
    }

    fn show_servers(&mut self) {
        self.leave(&Location::Servers);
        self.location = Location::Servers;
        self.pending = None;
        self.reset_view();
        let servers = config::servers().unwrap_or_else(|e| {
            eprintln!("Can't read saved servers: {e:#}");
            Vec::new()
        });
        self.items = servers.into_iter().map(Item::Server).collect();
        self.items.push(Item::AddServer);
        self.items.push(Item::Settings);
        self.restore_scroll();
        self.rebuild_rows();
    }

    fn show_settings(&mut self) {
        self.leave(&Location::Settings);
        self.location = Location::Settings;
        self.pending = None;
        self.reset_view();
        self.prefs = config::preferences();
        self.items = settings::ALL.into_iter().map(Item::Setting).collect();
        self.rebuild_rows();
    }

    /// Steps a setting to its next value and saves it.
    fn change_setting(&mut self, setting: Setting) {
        match config::update_preferences(|p| setting.cycle(p)) {
            Ok(prefs) => self.prefs = prefs,
            Err(e) => {
                // Still applies until Just Video quits.
                setting.cycle(&mut self.prefs);
                self.dialog("Couldn't save the setting", vec![format!("{e:#}")]);
            }
        }
        self.prefs_changed = true;
        self.rebuild_rows();
    }

    /// The preferences, once after the Settings screen changed them.
    pub fn take_preferences(&mut self) -> Option<config::Preferences> {
        std::mem::take(&mut self.prefs_changed).then_some(self.prefs)
    }

    fn navigate(&mut self, location: Location) {
        self.leave(&location);
        self.location = location.clone();
        self.items.clear();
        self.generation += 1;
        self.library.set_probe_generation(self.generation);
        self.reset_view();
        let id = self.id();
        self.pending = Some(id);
        match location {
            Location::Servers => self.show_servers(),
            Location::Settings => self.show_settings(),
            Location::Shares { server } => {
                self.set_status("Connecting…");
                self.library.send(Request::Shares { id, server });
            }
            Location::Folder {
                server,
                share,
                path,
            } => {
                self.set_status("Loading…");
                self.library.send(Request::List {
                    id,
                    server,
                    share,
                    path,
                });
            }
        }
    }

    fn refresh(&mut self) {
        let here = self.location.clone();
        let scroll = self.view.scroll;
        self.navigate(here);
        self.view.scroll = scroll;
    }

    fn rebuild_rows(&mut self) {
        let editing = self.editing();
        let selecting = self.selecting.as_ref();
        self.view.rows = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let mut row = match item {
                    Item::Server(s) => Row {
                        detail: s.url.clone(),
                        lock: Some(self.unlocked.contains(&s.url)),
                        // Always offered: nothing on the server changes, and
                        // Remove asks first.
                        actions: vec![Action::Edit, Action::Remove],
                        ..Row::new(Icon::Server, &s.name)
                    },
                    Item::AddServer => Row {
                        detail: "A Windows PC, NAS or Samba server on your network".into(),
                        ..Row::new(Icon::Add, "Add server")
                    },
                    Item::Settings => Row {
                        detail: "Jump lengths, volume, continuing videos".into(),
                        ..Row::new(Icon::Settings, "Settings")
                    },
                    Item::Setting(setting) => setting.row(&self.prefs),
                    Item::Share(name) => Row::new(Icon::Share, name),
                    Item::Dir(name) => Row::new(Icon::Folder, name),
                    Item::Video {
                        name,
                        size,
                        assessment,
                        layout,
                        broken,
                    } => Row {
                        icon: match (assessment, broken) {
                            (_, Some(_)) => Icon::Broken,
                            (a, None) => {
                                Icon::video(a.as_ref().map(|a| a.verdict), layout.as_ref())
                            }
                        },
                        detail: match (assessment, broken) {
                            (_, Some(_)) => "Can't read this file".into(),
                            (Some(a), None) => {
                                match layout.as_ref().and_then(Layout::short_label) {
                                    Some(format) => format!("{format}  ·  {}", a.title),
                                    None => a.title.clone(),
                                }
                            }
                            (None, None) => "Checking…".into(),
                        },
                        right: format_size(*size),
                        ..Row::new(Icon::Video(None), name)
                    },
                    Item::File { name, size } => Row {
                        right: format_size(*size),
                        dimmed: true,
                        ..Row::new(Icon::File, name)
                    },
                };
                row.outlined = self.came_from.as_deref() == Some(item.trail_name());
                if item.is_entry() {
                    match selecting {
                        Some(selected) => row.checked = Some(selected.contains(&i)),
                        None if editing && self.edit_mode => {
                            row.actions = vec![Action::Rename, Action::Delete]
                        }
                        None => {}
                    }
                }
                row
            })
            .collect();
        let has_entries = self.items.iter().any(Item::is_entry);
        let tools: Vec<(Tool, ToolAction)> = match &self.selecting {
            Some(selected) => {
                let mut tools = vec![(Tool::text("Cancel", false), ToolAction::CancelSelect)];
                if !selected.is_empty() {
                    let delete = Tool::text(format!("Delete {}", selected.len()), true);
                    tools.push((delete, ToolAction::DeleteSelected));
                }
                tools
            }
            None if editing && has_entries && self.edit_mode => vec![
                (Tool::icon(ToolIcon::Select, false), ToolAction::StartSelect),
                (Tool::icon(ToolIcon::Edit, true), ToolAction::ToggleEdit),
            ],
            None if editing && has_entries => {
                vec![(Tool::icon(ToolIcon::Edit, false), ToolAction::ToggleEdit)]
            }
            None => Vec::new(),
        };
        (self.view.tools, self.tool_actions) = tools.into_iter().unzip();
        self.view.clamp_scroll();
        self.dirty = true;
    }

    fn dialog(&mut self, title: impl Into<String>, body: Vec<String>) {
        self.view.dialog = Some(Dialog {
            title: title.into(),
            body,
            buttons: vec!["OK".into()],
            danger: false,
        });
        self.purpose = Some(Purpose::Info);
        self.dirty = true;
    }

    /// Handles worker results; returns a video ready to play.
    pub fn poll(&mut self) -> Option<Box<Opened>> {
        while let Some(response) = self.library.try_recv() {
            match response {
                Response::Shares { id, result } if Some(id) == self.pending => {
                    self.pending = None;
                    match result {
                        Ok(shares) if shares.is_empty() => {
                            self.set_status("This server has no shared folders you can open.")
                        }
                        Ok(shares) => {
                            self.view.status = None;
                            self.items = shares.into_iter().map(Item::Share).collect();
                            self.restore_scroll();
                            self.rebuild_rows();
                        }
                        Err(e) => self.set_status(format!("{e}  —  Press B to go back.")),
                    }
                }
                Response::List { id, result } if Some(id) == self.pending => {
                    self.pending = None;
                    let Location::Folder {
                        server,
                        share,
                        path,
                    } = self.location.clone()
                    else {
                        continue;
                    };
                    match result {
                        Ok(entries) => {
                            self.items = entries
                                .into_iter()
                                .filter(|e| !e.name.starts_with('.'))
                                .map(|e| {
                                    if e.is_dir {
                                        Item::Dir(e.name)
                                    } else if !is_video(&e.name) {
                                        Item::File {
                                            name: e.name,
                                            size: e.size,
                                        }
                                    } else {
                                        Item::Video {
                                            name: e.name,
                                            size: e.size,
                                            assessment: None,
                                            layout: None,
                                            broken: None,
                                        }
                                    }
                                })
                                .collect();
                            for item in &self.items {
                                if let Item::Video { name, .. } = item {
                                    let mut file = path.clone();
                                    file.push(name.clone());
                                    self.library.send(Request::Probe {
                                        generation: self.generation,
                                        server: server.clone(),
                                        share: share.clone(),
                                        path: file,
                                    });
                                }
                            }
                            self.view.status = if self.items.is_empty() {
                                Some("This folder is empty.".into())
                            } else {
                                None
                            };
                            self.restore_scroll();
                            self.rebuild_rows();
                        }
                        Err(e) => self.set_status(format!(
                            "Can't open this folder: {e}  —  Press B to go back."
                        )),
                    }
                }
                Response::Probe {
                    generation,
                    name,
                    result,
                } if generation == self.generation => {
                    for item in &mut self.items {
                        if let Item::Video {
                            name: n,
                            assessment,
                            layout,
                            broken,
                            ..
                        } = item
                            && *n == name
                        {
                            match &result {
                                Ok(p) => {
                                    *assessment = Some(p.assessment.clone());
                                    *layout = Some(p.layout);
                                }
                                Err(e) => *broken = Some(e.clone()),
                            }
                        }
                    }
                    self.rebuild_rows();
                }
                Response::Opened { id, result } if Some(id) == self.pending => {
                    self.pending = None;
                    self.view.notice = None;
                    self.dirty = true;
                    match result {
                        Ok(opened) if opened.assessment.verdict == Verdict::Unplayable => {
                            let a = opened.assessment.clone();
                            crate::xr::app::drop_in_background(opened);
                            self.dialog(
                                a.title,
                                [a.detail, a.hint].into_iter().flatten().collect(),
                            );
                        }
                        Ok(opened) => return Some(opened),
                        Err(e) => self.dialog("Can't open this video", vec![e]),
                    }
                }
                Response::Changed { id, result } if self.pending_changes.remove(&id) => {
                    if let Err(e) = result {
                        self.change_errors.push(e);
                    }
                    if self.pending_changes.is_empty() {
                        self.changes_done();
                    }
                }
                Response::ServerAdded { id, result } if self.pending_changes.remove(&id) => {
                    match result {
                        Ok(saved) => {
                            // An edit that changed the address replaces the old entry.
                            if let Some(Purpose::EditServer(old)) = &self.purpose
                                && *old != saved.url
                            {
                                if let Err(e) = config::move_server(old, &saved.url) {
                                    eprintln!("Can't move the old server's settings: {e:#}");
                                }
                                if self.unlocked.remove(old) {
                                    self.unlocked.insert(saved.url.clone());
                                }
                            }
                            self.show_servers();
                        }
                        Err(e) => {
                            if let Some(f) = &mut self.view.form {
                                f.busy = None;
                                f.error = Some(e);
                                self.dirty = true;
                            }
                        }
                    }
                }
                // A video that finished opening after we moved on: its reader and
                // decoder may take a while to close, so never on the frame loop.
                Response::Opened {
                    result: Ok(opened), ..
                } => crate::xr::app::drop_in_background(opened),
                _ => {} // stale response for a place we already left
            }
        }
        None
    }

    fn changes_done(&mut self) {
        self.view.notice = None;
        let errors = std::mem::take(&mut self.change_errors);
        if let (Some(f), Some(e)) = (&mut self.view.form, errors.first()) {
            // A rename that failed: say why, keep the form open.
            f.busy = None;
            f.error = Some(e.clone());
            self.dirty = true;
            return;
        }
        self.refresh();
        if !errors.is_empty() {
            let mut unique = errors.clone();
            unique.dedup();
            let title = if errors.len() == 1 {
                "That didn't work".to_string()
            } else {
                format!("{} items couldn't be changed", errors.len())
            };
            self.dialog(title, unique);
        }
    }

    /// Reacts to a click (trigger or A) on the panel.
    pub fn click(&mut self, hit: Hit) {
        match hit {
            Hit::Form(h) => self.click_form(h),
            Hit::DialogButton(i) => self.click_dialog(i),
            Hit::Crumb(i) => self.go_to_crumb(i),
            Hit::Lock(i) => {
                if let Some(Item::Server(s)) = self.items.get(i)
                    && !self.unlocked.remove(&s.url)
                {
                    self.unlocked.insert(s.url.clone());
                }
                self.rebuild_rows();
            }
            Hit::Tool(k) => self.click_tool(k),
            Hit::RowAction(i, Action::Rename) => self.start_rename(i),
            Hit::RowAction(i, Action::Delete) => self.confirm_delete(vec![i]),
            Hit::RowAction(i, Action::Edit) => self.start_edit_server(i),
            Hit::RowAction(i, Action::Remove) => self.confirm_remove_server(i),
            Hit::Row(i) => self.select(i),
            Hit::ScrollBar | Hit::Nothing => {}
        }
    }

    fn click_tool(&mut self, k: usize) {
        match self.tool_actions.get(k) {
            Some(ToolAction::ToggleEdit) => self.edit_mode = !self.edit_mode,
            Some(ToolAction::StartSelect) => self.selecting = Some(HashSet::new()),
            Some(ToolAction::CancelSelect) => self.selecting = None,
            Some(ToolAction::DeleteSelected) => {
                let mut indices: Vec<usize> = self.selecting.iter().flatten().copied().collect();
                indices.sort_unstable();
                self.confirm_delete(indices);
            }
            None => {}
        }
        self.rebuild_rows();
    }

    /// Asks before deleting these entries.
    fn confirm_delete(&mut self, indices: Vec<usize>) {
        let names: Vec<String> = indices
            .iter()
            .filter_map(|&i| self.items.get(i))
            .map(|it| it.name().to_string())
            .collect();
        let n = names.len();
        if n == 0 {
            return;
        }
        let mut body = vec![format!(
            "{} permanently deleted from the server. This can't be undone here. Only empty folders can be deleted.",
            if n == 1 { "It will be" } else { "They will be" }
        )];
        if n > 1 {
            let mut list = names
                .iter()
                .take(6)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if n > 6 {
                list += &format!(" and {} more", n - 6);
            }
            body.push(list);
        }
        let title = if n == 1 {
            format!("Delete {}?", names[0])
        } else {
            format!("Delete {n} items?")
        };
        self.view.dialog = Some(Dialog {
            title,
            body,
            buttons: vec!["Cancel".into(), "Delete".into()],
            danger: true,
        });
        self.purpose = Some(Purpose::Delete(indices));
        self.dirty = true;
    }

    /// A long press on a row: edit mode with that entry selected for deleting.
    pub fn long_press(&mut self, index: usize) {
        if !self.items.get(index).is_some_and(Item::is_entry) {
            return;
        }
        if !self.editing() {
            self.view.notice =
                Some("To change files, unlock this server on the server list first.".into());
            self.dirty = true;
            return;
        }
        self.edit_mode = true;
        self.selecting
            .get_or_insert_with(HashSet::new)
            .insert(index);
        self.rebuild_rows();
    }

    /// The playable video `delta` places from the playing one, if any.
    fn adjacent(&self, delta: isize) -> Option<usize> {
        let mut i = self.playing? as isize;
        loop {
            i += delta;
            let item = self.items.get(usize::try_from(i).ok()?)?;
            if let Item::Video {
                assessment,
                broken: None,
                ..
            } = item
                && assessment
                    .as_ref()
                    .is_none_or(|a| a.verdict != Verdict::Unplayable)
            {
                return Some(i as usize);
            }
        }
    }

    /// Back from playing: outlines the video that played, and keeps it in view.
    pub fn playback_ended(&mut self) {
        self.came_from = self
            .playing
            .and_then(|i| self.items.get(i))
            .map(|item| item.trail_name().to_string());
        self.reload_played_layout();
        self.restore_scroll();
        self.rebuild_rows();
    }

    /// The video that played may have had its format changed: mark it as saved.
    fn reload_played_layout(&mut self) {
        if let (
            Some(i),
            Location::Folder {
                server,
                share,
                path,
            },
        ) = (self.playing, &self.location)
            && let Some(Item::Video {
                name,
                layout: Some(layout),
                ..
            }) = self.items.get_mut(i)
        {
            let file: Path = path.iter().chain([&*name]).cloned().collect();
            if let Ok(Some(saved)) = config::layout_override(&file_key(server, share, &file)) {
                saved.apply(layout);
            }
        }
    }

    pub fn has_adjacent(&self, delta: isize) -> bool {
        self.adjacent(delta).is_some()
    }

    /// Opens the previous (-1) or next (+1) video in the folder; the result
    /// arrives from [`Navigator::poll`] like any other.
    pub fn open_adjacent(&mut self, delta: isize) -> bool {
        let Some(index) = self.adjacent(delta) else {
            return false;
        };
        self.selecting = None;
        self.view.dialog = None;
        self.view.form = None;
        self.reload_played_layout();
        self.select(index);
        true
    }

    fn go_to_crumb(&mut self, i: usize) {
        match (i, self.location.clone()) {
            (0, _) => self.show_servers(),
            (1, Location::Folder { server, .. }) => self.navigate(Location::Shares { server }),
            (
                n,
                Location::Folder {
                    server,
                    share,
                    path,
                },
            ) if n >= 2 => self.navigate(Location::Folder {
                server,
                share,
                path: path[..n - 2].to_vec(),
            }),
            _ => {}
        }
    }

    fn select(&mut self, index: usize) {
        if let Some(selected) = &mut self.selecting {
            if self.items.get(index).is_some_and(Item::is_entry) && !selected.remove(&index) {
                selected.insert(index);
            }
            self.rebuild_rows();
            return;
        }
        if self.pending.is_some() && !matches!(self.location, Location::Folder { .. }) {
            return;
        }
        let Some(item) = self.items.get(index) else {
            return;
        };
        match (item, self.location.clone()) {
            (Item::Server(s), _) => self.navigate(Location::Shares { server: s.clone() }),
            (Item::AddServer, _) => self.start_server_form(None),
            (Item::Settings, _) => self.show_settings(),
            (Item::Setting(setting), _) => self.change_setting(*setting),
            (Item::Share(name), Location::Shares { server }) => self.navigate(Location::Folder {
                server,
                share: name.clone(),
                path: Vec::new(),
            }),
            (
                Item::Dir(name),
                Location::Folder {
                    server,
                    share,
                    mut path,
                },
            ) => {
                path.push(name.clone());
                self.navigate(Location::Folder {
                    server,
                    share,
                    path,
                });
            }
            (
                Item::Video {
                    name,
                    assessment,
                    broken,
                    ..
                },
                Location::Folder {
                    server,
                    share,
                    mut path,
                },
            ) => {
                if let Some(e) = broken {
                    let body = vec!["It may be damaged or not a video.".into(), e.clone()];
                    self.dialog("Can't read this file", body);
                } else if let Some(a) = assessment
                    .as_ref()
                    .filter(|a| a.verdict == Verdict::Unplayable)
                {
                    let body = [a.detail.clone(), a.hint.clone()]
                        .into_iter()
                        .flatten()
                        .collect();
                    self.dialog(a.title.clone(), body);
                } else {
                    path.push(name.clone());
                    self.playing = Some(index);
                    self.view.notice = Some(format!("Opening {name}…"));
                    self.dirty = true;
                    let id = self.id();
                    self.pending = Some(id);
                    self.library.send(Request::Open {
                        id,
                        server,
                        share,
                        path,
                    });
                }
            }
            _ => {}
        }
    }

    /// The add-server form, or with `existing` the edit form prefilled from it.
    fn start_server_form(&mut self, existing: Option<Server>) {
        let field = |label: &str, value: String, placeholder: &str, secret: bool| Field {
            label: label.into(),
            value,
            secret,
            placeholder: placeholder.into(),
        };
        let url = existing.as_ref().and_then(|s| s.url.parse::<SmbUrl>().ok());
        let address = url.as_ref().map_or(String::new(), |u| match u.port {
            Some(port) => format!("{}:{port}", u.host),
            None => u.host.clone(),
        });
        let user = url.as_ref().map_or(String::new(), |u| match &u.domain {
            Some(domain) => format!("{domain};{}", u.user),
            None => u.user.clone(),
        });
        let password_hint = if existing.is_some() {
            "leave empty to keep the saved one"
        } else {
            ""
        };
        let (title, submit) = match &existing {
            Some(s) => (format!("Edit {}", s.name), "Save"),
            None => ("Add server".to_string(), "Connect"),
        };
        self.view.form = Some(Form::new(
            title,
            vec![
                field("Address", address, "IP or name, e.g. 192.168.1.10", false),
                field("User", user, "user name (or DOMAIN;user)", false),
                field("Password", String::new(), password_hint, true),
                field(
                    "Name",
                    existing.as_ref().map_or(String::new(), |s| s.name.clone()),
                    "optional, shown in the list",
                    false,
                ),
            ],
            submit,
        ));
        self.purpose = Some(match existing {
            Some(s) => Purpose::EditServer(s.url),
            None => Purpose::AddServer,
        });
        self.dirty = true;
    }

    fn start_edit_server(&mut self, index: usize) {
        if let Some(Item::Server(s)) = self.items.get(index) {
            let s = s.clone();
            self.start_server_form(Some(s));
        }
    }

    fn start_rename(&mut self, index: usize) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        let current = item.name().to_string();
        self.view.form = Some(Form::new(
            format!("Rename {current}"),
            vec![Field {
                label: "New name".into(),
                value: current,
                secret: false,
                placeholder: String::new(),
            }],
            "Rename",
        ));
        self.purpose = Some(Purpose::Rename(index));
        self.dirty = true;
    }

    fn confirm_remove_server(&mut self, index: usize) {
        let Some(Item::Server(s)) = self.items.get(index) else {
            return;
        };
        self.view.dialog = Some(Dialog {
            title: format!("Remove {}?", s.name),
            body: vec!["Removes this server and its saved password from Just Video. Nothing on the server changes.".into()],
            buttons: vec!["Cancel".into(), "Remove".into()],
            danger: true,
        });
        self.purpose = Some(Purpose::RemoveServer(index));
        self.dirty = true;
    }

    fn click_dialog(&mut self, button: usize) {
        let confirmed = self
            .view
            .dialog
            .as_ref()
            .is_some_and(|d| button + 1 == d.buttons.len());
        self.view.dialog = None;
        self.dirty = true;
        let purpose = self.purpose.take();
        if !confirmed {
            return;
        }
        match purpose {
            Some(Purpose::RemoveServer(index)) => {
                if let Some(Item::Server(s)) = self.items.get(index) {
                    let url = s.url.clone();
                    self.unlocked.remove(&url);
                    match config::remove_server(&url) {
                        Ok(_) => self.show_servers(),
                        Err(e) => self.dialog("Couldn't remove the server", vec![format!("{e:#}")]),
                    }
                }
            }
            Some(Purpose::Delete(indices)) => {
                let Location::Folder {
                    server,
                    share,
                    path,
                } = self.location.clone()
                else {
                    return;
                };
                self.selecting = None;
                let selected = indices;
                for index in selected {
                    let Some(item) = self.items.get(index) else {
                        continue;
                    };
                    let mut file = path.clone();
                    file.push(item.name().to_string());
                    let id = self.id();
                    self.pending_changes.insert(id);
                    self.library.send(Request::Delete {
                        id,
                        server: server.clone(),
                        share: share.clone(),
                        path: file,
                    });
                }
                if !self.pending_changes.is_empty() {
                    self.view.notice =
                        Some(format!("Deleting {} items…", self.pending_changes.len()));
                }
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    fn click_form(&mut self, hit: form::Hit) {
        let Some(f) = &mut self.view.form else { return };
        self.dirty = true;
        match hit {
            form::Hit::Field(i, cursor) => f.focus(i, Some(cursor)),
            form::Hit::Key(key) => match f.press(key) {
                Some(Key::Cancel) => {
                    self.view.form = None;
                    self.purpose = None;
                }
                Some(Key::Submit) => self.submit_form(),
                _ => {}
            },
            form::Hit::Nothing => {}
        }
    }

    fn submit_form(&mut self) {
        let Some(f) = self.view.form.as_mut() else {
            return;
        };
        match &self.purpose {
            Some(Purpose::AddServer | Purpose::EditServer(_)) => {
                let editing = match &self.purpose {
                    Some(Purpose::EditServer(url)) => Some(url.clone()),
                    _ => None,
                };
                let (address, user, typed_password, name) = (
                    f.value(0).trim().to_string(),
                    f.value(1).trim().to_string(),
                    f.value(2).to_string(),
                    f.value(3).trim().to_string(),
                );
                if address.is_empty() {
                    f.error = Some("Enter the server's address.".into());
                    f.focus(0, None);
                    return;
                }
                let host = address
                    .trim_start_matches("smb://")
                    .trim_end_matches('/')
                    .to_string();
                let user = if user.is_empty() {
                    "guest".to_string()
                } else {
                    user
                };
                let url = match format!("smb://{user}@{host}").parse::<SmbUrl>() {
                    Ok(url) if url.share.is_empty() => url,
                    Ok(_) => {
                        f.error = Some("Enter just the server, without a share or folder.".into());
                        return;
                    }
                    Err(e) => {
                        f.error = Some(format!("{e:#}"));
                        return;
                    }
                };
                // Editing: an empty password keeps the saved one.
                let password = match (&editing, typed_password.is_empty()) {
                    (Some(old), true) => config::password(old).ok().flatten().unwrap_or_default(),
                    _ => typed_password,
                };
                let server = Server {
                    name: if name.is_empty() {
                        url.host.clone()
                    } else {
                        name
                    },
                    url: url.server_url(),
                };
                f.busy = Some(format!("Connecting to {}…", url.host));
                let id = self.id();
                self.pending_changes.insert(id);
                self.library.send(Request::AddServer {
                    id,
                    server,
                    password,
                });
            }
            Some(Purpose::Rename(index)) => {
                let index = *index;
                let new_name = f.value(0).trim().to_string();
                let Some(item) = self.items.get(index) else {
                    return;
                };
                if new_name.is_empty() || new_name == item.name() {
                    self.view.form = None;
                    self.purpose = None;
                    return;
                }
                if let Location::Folder {
                    server,
                    share,
                    mut path,
                } = self.location.clone()
                {
                    path.push(item.name().to_string());
                    f.busy = Some("Renaming…".into());
                    let id = self.id();
                    self.pending_changes.insert(id);
                    self.library.send(Request::Rename {
                        id,
                        server,
                        share,
                        path,
                        new_name,
                    });
                }
            }
            _ => {}
        }
    }

    pub fn dialog_open(&self) -> bool {
        self.view.dialog.is_some()
    }

    /// Goes up one level (or closes a dialog, form or selection). False at the top.
    pub fn back(&mut self) -> bool {
        self.dirty = true;
        if self.view.form.is_some() && self.pending_changes.is_empty() {
            self.view.form = None;
            self.purpose = None;
            return true;
        }
        if self.view.dialog.is_some() {
            self.view.dialog = None;
            self.purpose = None;
            return true;
        }
        if self.selecting.take().is_some() {
            self.rebuild_rows();
            return true;
        }
        if std::mem::take(&mut self.edit_mode) && self.editing() {
            self.rebuild_rows();
            return true;
        }
        match self.location.clone() {
            Location::Servers => return false,
            Location::Settings | Location::Shares { .. } => self.show_servers(),
            Location::Folder {
                server,
                share,
                mut path,
            } => {
                if path.pop().is_some() {
                    self.navigate(Location::Folder {
                        server,
                        share,
                        path,
                    });
                } else {
                    self.navigate(Location::Shares { server });
                }
            }
        }
        true
    }

    #[cfg(test)]
    fn show_entries_for_test(&mut self, server: Server, names: &[&str]) {
        self.location = Location::Folder {
            server,
            share: "s".into(),
            path: Vec::new(),
        };
        self.reset_view();
        self.items = names.iter().map(|n| Item::Dir(n.to_string())).collect();
        self.rebuild_rows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_extensions() {
        assert!(is_video("clip_180_LR.MP4"));
        assert!(is_video("movie.mkv"));
        assert!(!is_video("notes.txt"));
        assert!(!is_video("mkv"));
    }

    #[test]
    fn server_list_offers_adding_one() {
        let nav = Navigator::new(Library::start(None));
        let rows = &nav.view().rows;
        assert_eq!(rows[rows.len() - 2].icon, Icon::Add);
        assert_eq!(rows[rows.len() - 1].icon, Icon::Settings);
        assert_eq!(nav.view().crumbs, vec!["Just Video".to_string()]);
    }

    #[test]
    fn settings_rows_cycle_and_save() {
        let dir = config::temp_config("nav-settings");
        let mut nav = Navigator::new(Library::start(None));
        let gear = nav.view().rows.len() - 1;
        nav.click(Hit::Row(gear));
        assert_eq!(nav.view().crumbs, ["Just Video", "Settings"]);
        assert_eq!(nav.view().rows.len(), settings::ALL.len());
        assert_eq!(nav.view().rows[0].right, "5 s");
        assert_eq!(nav.take_preferences(), None);

        nav.click(Hit::Row(0));
        assert_eq!(nav.view().rows[0].right, "10 s");
        assert_eq!(nav.take_preferences().map(|p| p.short_jump), Some(10));
        assert_eq!(nav.take_preferences(), None, "reported once");
        assert_eq!(config::preferences().short_jump, 10, "saved");

        assert!(nav.back());
        assert_eq!(nav.view().crumbs, ["Just Video"]);
        assert!(nav.view().rows[gear].outlined, "the way back in");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn server_lock_enables_rename_and_multi_delete() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
        };
        nav.show_entries_for_test(server.clone(), &["a", "b", "c"]);
        assert!(
            nav.view().rows.iter().all(|r| r.actions.is_empty()),
            "locked: read only"
        );
        assert!(nav.view().tools.is_empty());

        nav.unlocked.insert(server.url.clone());
        nav.rebuild_rows();
        assert!(
            nav.view().rows[0].actions.is_empty(),
            "rename/delete wait for edit mode"
        );
        assert_eq!(nav.view().tools.len(), 1, "edit");

        nav.click(Hit::Tool(0));
        assert_eq!(
            nav.view().rows[0].actions,
            vec![Action::Rename, Action::Delete]
        );
        assert_eq!(nav.view().tools.len(), 2, "select + edit");

        nav.click(Hit::Tool(0));
        assert!(nav.view().rows.iter().all(|r| r.checked == Some(false)));
        nav.click(Hit::Row(0));
        nav.click(Hit::Row(2));
        assert_eq!(nav.view().tools[1].label, "Delete 2");
        nav.click(Hit::Row(2));
        assert_eq!(nav.view().tools[1].label, "Delete 1");
        nav.click(Hit::Tool(1));
        assert!(nav.dialog_open());
        assert!(nav.back(), "closes the dialog");
        assert!(nav.back(), "leaves selection");
        assert!(nav.view().rows.iter().all(|r| r.checked.is_none()));
        assert!(nav.back(), "leaves edit mode");
        assert!(nav.view().rows[0].actions.is_empty());

        nav.long_press(1);
        assert_eq!(nav.view().rows[1].checked, Some(true), "long press selects");
    }

    #[test]
    fn servers_can_be_edited_without_unlocking() {
        let mut nav = Navigator::new(Library::start(None));
        nav.items = vec![
            Item::Server(Server {
                name: "NAS".into(),
                url: "smb://WORK;bob@nas:4455".into(),
            }),
            Item::AddServer,
        ];
        nav.rebuild_rows();
        let row = &nav.view().rows[0];
        assert_eq!(row.lock, Some(false));
        assert_eq!(row.actions, vec![Action::Edit, Action::Remove]);

        nav.click(Hit::RowAction(0, Action::Remove));
        assert!(nav.dialog_open(), "removing asks first");
        assert!(nav.back());

        nav.click(Hit::RowAction(0, Action::Edit));
        let form = nav.view().form.as_ref().expect("edit form");
        assert_eq!(form.title, "Edit NAS");
        let values: Vec<&str> = (0..4).map(|i| form.value(i)).collect();
        assert_eq!(values, ["nas:4455", "WORK;bob", "", "NAS"]);
        assert!(
            matches!(&nav.purpose, Some(Purpose::EditServer(url)) if url == "smb://WORK;bob@nas:4455")
        );
    }

    #[test]
    fn videos_are_marked_by_format() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
        };
        nav.show_entries_for_test(server, &[]);
        let a = crate::playability::assess(crate::playability::Platform::SteamFrame, None);
        nav.items = ["film.mkv", "film_sbs.mkv", "trip_180_LR.mp4", "new_360.mp4"]
            .into_iter()
            .map(|name| Item::Video {
                name: name.into(),
                size: 1,
                assessment: Some(a.clone()),
                layout: Some(crate::vr::detect(name, None)),
                broken: None,
            })
            .collect();
        if let Some(Item::Video {
            assessment, layout, ..
        }) = nav.items.last_mut()
        {
            (*assessment, *layout) = (None, None); // not probed yet
        }
        nav.rebuild_rows();
        let rows = &nav.view().rows;
        let v = Some(a.verdict);
        assert_eq!(rows[0].icon, Icon::Video(v));
        assert_eq!(rows[0].detail, a.title);
        assert_eq!(rows[1].icon, Icon::Video3d(v));
        assert_eq!(rows[1].detail, format!("3D  ·  {}", a.title));
        assert_eq!(rows[2].icon, Icon::VideoVr(v));
        assert_eq!(rows[2].detail, format!("VR180 3D  ·  {}", a.title));
        assert_eq!(rows[3].icon, Icon::Video(None));
    }

    #[test]
    fn going_up_returns_to_the_folder_left() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
        };
        let names: Vec<String> = (0..40).map(|i| format!("f{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        nav.show_entries_for_test(server.clone(), &names);
        nav.set_scroll(20.0);
        nav.select(25);
        assert!(matches!(&nav.location, Location::Folder { path, .. } if path == &["f25"]));
        nav.back();
        assert_eq!(nav.came_from.as_deref(), Some("f25"));
        // The listing arrives.
        nav.items = names.iter().map(|n| Item::Dir(n.to_string())).collect();
        nav.restore_scroll();
        nav.rebuild_rows();
        assert_eq!(nav.scroll(), 20.0);
        assert!(nav.view().rows[25].outlined);
        assert_eq!(nav.view().rows.iter().filter(|r| r.outlined).count(), 1);
    }

    #[test]
    fn previous_and_next_skip_what_cannot_play() {
        let mut nav = Navigator::new(Library::start(None));
        let server = Server {
            name: "NAS".into(),
            url: "smb://u@nas".into(),
        };
        nav.show_entries_for_test(server, &["folder"]);
        let video = |name: &str, broken: bool| Item::Video {
            name: name.into(),
            size: 1,
            assessment: None,
            layout: None,
            broken: broken.then(|| "bad".to_string()),
        };
        let file = Item::File {
            name: "notes.txt".into(),
            size: 1,
        };
        nav.items
            .extend([video("a", false), video("b", true), file, video("c", false)]);
        nav.playing = Some(1);
        assert!(!nav.has_adjacent(-1), "a folder is not a video");
        assert_eq!(
            nav.adjacent(1),
            Some(4),
            "skips the broken one and the file"
        );
        nav.playing = Some(4);
        assert!(!nav.has_adjacent(1));
        nav.playback_ended();
        assert!(nav.view().rows[4].outlined, "the video just played");
        assert_eq!(nav.view().rows.iter().filter(|r| r.outlined).count(), 1);
    }
}
