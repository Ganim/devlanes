//! The options page of the Esc menu, after btop's: every user config setting
//! in category tabs, each row with its current value, and the selected
//! setting's description on the right.
//!
//! Enter edits a row: ← → change it, typing replaces it, Enter keeps it and
//! Esc puts it back. Changes stay on the page until `s` saves them all to
//! `config.yaml` through [`devlanes_config::set_user_value`], which keeps the
//! file's comments and order. Colors and glyphs preview while they change and
//! go back when the changes are discarded.

use std::{cell::RefCell, collections::HashMap, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::app::{Effect, Msg};
use crate::menu::{centered, floating_box};
use crate::text::{self, clip};
use crate::theme::{Glyph, Glyphs, Theme, Tone};
use crate::ui::{fit, spans_width, spread};

/// Tabs and the config sections they hold, in order. Sections not listed
/// here join the first tab.
const CATEGORIES: [(&str, &[&str]); 4] = [
    ("general", &["tui", "terminal", "updates"]),
    ("network", &["proxy", "hostnames", "ports", "tls"]),
    ("startup", &["startup"]),
    ("storage", &["logs", "metrics", "history", "state"]),
];

/// Reserved settings the config rejects today; offering them would only
/// produce errors.
const SKIP: [&str; 2] = ["hostnames.base_domain", "hostnames.dns_provider"];

/// How a setting's value is changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// ← → toggle.
    Bool,
    /// ← → cycle through the values.
    Choice(Vec<Choice>),
    /// ← → step by one, or type a number.
    Integer { min: i64, max: i64 },
    /// Typed; the config parses it (`30d`, `512MiB`, `75%`).
    Text,
    /// Typed, the values separated by commas.
    List(Vec<Choice>),
}

impl Kind {
    /// Whether editing the row means typing.
    fn typed(&self) -> bool {
        matches!(self, Kind::Integer { .. } | Kind::Text | Kind::List(_))
    }
}

/// One allowed value and its description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub about: String,
}

/// When a saved change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// The TUI redraws with it (colors, glyphs); it previews before saving.
    Now,
    /// The next command reads it.
    NextCommand,
    /// The daemon reads it when it starts.
    Daemon,
}

/// One setting, from the user config schema.
#[derive(Debug, Clone)]
pub struct Item {
    /// Dotted path in config.yaml: `proxy.https_port`.
    pub key: String,
    pub kind: Kind,
    /// Unset is a value of its own (`hostnames.hosts_file`, `startup.max`).
    pub optional: bool,
    pub about: String,
    pub examples: Vec<String>,
    /// The schema's default, as the config serializes it.
    pub default: Option<Value>,
    pub applies: Applies,
}

/// One tab.
#[derive(Debug, Clone)]
pub struct Category {
    pub name: &'static str,
    pub items: Vec<Item>,
}

/// File work for the options page, done by the runtime.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Load,
    /// Sets (`Some`) or removes (`None`) each key in order, then reloads.
    Save {
        changes: Vec<(String, Option<Value>)>,
    },
}

/// What the runtime read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Loaded {
        /// The file, as shown in the title.
        path: String,
        /// The file's text; an absent file is empty.
        source: Result<String, String>,
        /// Why a save failed.
        error: Option<String>,
    },
}

/// What the app does after a key or click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    /// Back to the Esc menu.
    Menu,
    /// Closed by a click outside.
    Close,
    /// Ctrl+C: the TUI's own quit.
    Quit,
    /// R: the daemon restart prompt.
    RestartDaemon,
}

/// The row being edited.
#[derive(Debug, Clone)]
pub struct Edit {
    /// The page's text before the edit, for Esc.
    before: String,
    /// What is typed, for typed settings.
    pub text: Option<String>,
    /// `d` put the default back; Enter keeps it.
    defaulted: bool,
}

/// The question asked when the page closes with unsaved changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prompt {
    /// 0 save changes, 1 discard, 2 keep editing.
    pub choice: usize,
    /// Where the page goes after saving or discarding.
    then: Outcome,
}

/// The answers of the prompt, in order.
const ANSWERS: [(&str, &str); 3] = [
    ("s", "save changes"),
    ("d", "discard"),
    ("esc", "keep editing"),
];

/// Rectangles of the last render, for the mouse.
#[derive(Debug, Default)]
struct Hits {
    outer: Option<Rect>,
    tabs: Vec<Rect>,
    rows: Vec<(Rect, usize)>,
    less: Option<Rect>,
    more: Option<Rect>,
    save: Option<Rect>,
    answers: Vec<Rect>,
}

/// The open options page.
#[derive(Debug)]
pub struct Settings {
    pub categories: Vec<Category>,
    pub tab: usize,
    /// The selected row of the tab.
    pub index: usize,
    /// config.yaml as last read; `None` until loaded.
    saved: Option<String>,
    saved_file: Value,
    /// config.yaml with the unsaved changes; what the page shows.
    pub source: Option<String>,
    file: Value,
    pub path: Option<String>,
    /// The file could not be read or is not YAML; nothing can be edited.
    pub load_error: Option<String>,
    /// Settings the config drops (and uses the default for), by key.
    problems: HashMap<String, String>,
    /// Keys changed since the last save, in the order they were first changed.
    staged: Vec<String>,
    pub edit: Option<Edit>,
    /// Why the last change or save was refused.
    pub error: Option<String>,
    /// Saved settings the daemon reads at start.
    pub restart: Vec<String>,
    /// A save in flight: how many settings it writes and which need a restart.
    saving: Option<(usize, Vec<String>)>,
    /// How many settings the last save wrote.
    pub saved_count: Option<usize>,
    pub prompt: Option<Prompt>,
    /// A setting that previews (colors, glyphs) changed since the last save.
    previewed: bool,
    hits: RefCell<Hits>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    /// Every setting of the user config schema, waiting for the file.
    pub fn new() -> Self {
        Self {
            categories: catalog(),
            tab: 0,
            index: 0,
            saved: None,
            saved_file: Value::Null,
            source: None,
            file: Value::Null,
            path: None,
            load_error: None,
            problems: HashMap::new(),
            staged: Vec::new(),
            edit: None,
            error: None,
            restart: Vec::new(),
            saving: None,
            saved_count: None,
            prompt: None,
            previewed: false,
            hits: RefCell::new(Hits::default()),
        }
    }

    /// The selected setting.
    pub fn selected(&self) -> Option<&Item> {
        self.categories.get(self.tab)?.items.get(self.index)
    }

    /// Applies what the runtime read: the first load, or the file after a save.
    pub fn reply(&mut self, reply: Reply) {
        let Reply::Loaded {
            path,
            source,
            error,
        } = reply;
        self.path = Some(path);
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                self.load_error = Some(error);
                self.source = None;
                return;
            }
        };
        match (self.saving.take(), error) {
            (Some((count, restart)), None) => {
                for key in restart {
                    if !self.restart.contains(&key) {
                        self.restart.push(key);
                    }
                }
                self.saved_count = Some(count);
                self.staged.clear();
                self.previewed = false;
                self.set_saved(&source);
                self.set_working(source);
            }
            // The file on disk is what was saved; the page keeps the rest unsaved.
            (Some(_), Some(error)) => {
                self.error = Some(format!("Not saved: {error}"));
                self.set_saved(&source);
            }
            (None, error) => {
                if error.is_some() {
                    self.error = error;
                }
                self.set_saved(&source);
                self.set_working(source);
            }
        }
    }

    /// The text on disk.
    fn set_saved(&mut self, source: &str) {
        self.saved_file = devlanes_config::parse_user_value(source).unwrap_or(Value::Null);
        self.saved = Some(source.to_owned());
    }

    /// The text the page shows: its data and its problems.
    fn set_working(&mut self, source: String) {
        match devlanes_config::parse_user_value(&source) {
            Ok(file) => {
                let (_, issues) = devlanes_config::parse_user_lenient("config.yaml", &source);
                self.problems = issues
                    .into_iter()
                    .filter_map(|issue| {
                        let message = issue.message.trim_end_matches("; the default applies");
                        let (key, message) = message.split_once(": ")?;
                        Some((key.to_owned(), message.to_owned()))
                    })
                    .collect();
                self.file = file;
                self.load_error = None;
                self.source = Some(source);
            }
            Err(error) => {
                self.load_error = Some(format!(
                    "config.yaml does not parse, so options cannot change it. Fix it by hand: {}",
                    error.message
                ));
                self.source = None;
            }
        }
    }

    /// The value written in the page's text, if any.
    pub fn file_value(&self, key: &str) -> Option<Value> {
        pointer(&self.file, key)
    }

    /// The value in effect: the page's, else the default.
    fn current(&self, item: &Item) -> Value {
        self.file_value(&item.key)
            .or_else(|| item.default.clone())
            .unwrap_or(Value::Null)
    }

    /// A setting's value as its row shows it.
    pub fn shown(&self, item: &Item) -> String {
        match self.file_value(&item.key) {
            Some(value) => display(&value),
            None => humanize(&self.current(item)),
        }
    }

    /// Whether `key`'s value differs from the saved file.
    pub fn changed(&self, key: &str) -> bool {
        pointer(&self.file, key) != pointer(&self.saved_file, key)
    }

    /// Keys to write on save: the staged ones whose value still differs.
    fn dirty(&self) -> Vec<String> {
        self.staged
            .iter()
            .filter(|key| self.changed(key))
            .cloned()
            .collect()
    }

    /// How many changes are not saved yet.
    pub fn unsaved(&self) -> usize {
        self.dirty().len()
    }

    /// Keys in the list: ↑ ↓ select, ← → and 1-4 change tabs, Enter edits,
    /// d restores the default, s saves, R restarts the daemon after a save,
    /// Esc closes. While a row is edited see [`Settings::edit_key`].
    pub fn key(&mut self, key: KeyEvent, theme: &mut Theme, effects: &mut Vec<Effect>) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.prompt.is_some() {
            return self.prompt_key(key, theme, effects);
        }
        if ctrl && key.code == KeyCode::Char('c') {
            return self.leave(Outcome::Quit, theme);
        }
        // s is a letter while a value is typed; Ctrl+S saves from anywhere.
        let typing = self.edit.is_some() && self.selected().is_some_and(|item| item.kind.typed());
        if key.code == KeyCode::Char('s') && (ctrl || !typing) {
            self.save(theme, effects);
            return Outcome::Stay;
        }
        if self.edit.is_some() {
            self.edit_key(key, theme);
            return Outcome::Stay;
        }
        let count = self
            .categories
            .get(self.tab)
            .map_or(0, |tab| tab.items.len());
        let page = self.page_rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return self.leave(Outcome::Menu, theme),
            KeyCode::Char('R') if !self.restart.is_empty() => return Outcome::RestartDaemon,
            KeyCode::Up | KeyCode::Char('k') => self.select(self.index.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.index + 1),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(count.saturating_sub(1)),
            KeyCode::PageUp => self.select(self.index.saturating_sub(page)),
            KeyCode::PageDown => self.select(self.index + page),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => self.switch(self.tab + 1),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                self.switch(self.tab + self.categories.len() - 1);
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let tab = usize::from(digit as u8 - b'1');
                if tab < self.categories.len() {
                    self.switch(tab);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.begin_edit(),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                self.stage(None, theme);
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Keys while a row is edited. ← → change a value or step a number,
    /// typing replaces a typed value, d (Delete while typing) restores the
    /// default, Enter keeps the value, Esc puts back the one from before the
    /// edit, ↑ ↓ keep it and move.
    fn edit_key(&mut self, key: KeyEvent, theme: &mut Theme) {
        let Some(item) = self.selected().cloned() else {
            self.edit = None;
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let typed = item.kind.typed();
        match key.code {
            KeyCode::Esc => self.revert(theme),
            KeyCode::Enter => {
                self.finish(theme);
            }
            KeyCode::Up | KeyCode::Down => {
                if self.finish(theme) {
                    let index = if key.code == KeyCode::Up {
                        self.index.saturating_sub(1)
                    } else {
                        self.index + 1
                    };
                    self.select(index);
                }
            }
            KeyCode::Left | KeyCode::Right => {
                let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                self.step(&item, delta, theme);
            }
            KeyCode::Delete => self.restore(theme),
            KeyCode::Char('d') if !matches!(item.kind, Kind::Text | Kind::List(_)) => {
                self.restore(theme);
            }
            KeyCode::Char('h' | 'l' | ' ') if !typed => {
                let delta = if key.code == KeyCode::Char('h') {
                    -1
                } else {
                    1
                };
                self.step(&item, delta, theme);
            }
            KeyCode::Backspace if typed => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.text.get_or_insert_default().pop();
                    edit.defaulted = false;
                }
            }
            KeyCode::Char('u') if ctrl && typed => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.text = Some(String::new());
                    edit.defaulted = false;
                }
            }
            KeyCode::Char(c) if typed && !ctrl => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.text.get_or_insert_default().push(c);
                    edit.defaulted = false;
                }
                self.error = None;
            }
            _ => {}
        }
    }

    /// Clicks: a tab switches to it; a row selects it and a click on the
    /// selected row edits it; ◂ ▸ change the edited row; [save] saves. A
    /// click outside the box closes the page.
    pub fn mouse(
        &mut self,
        mouse: MouseEvent,
        theme: &mut Theme,
        effects: &mut Vec<Effect>,
    ) -> Outcome {
        let position = Position::new(mouse.column, mouse.row);
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let delta: isize = match mouse.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return Outcome::Stay,
            };
            if self.edit.is_none() && self.prompt.is_none() {
                self.select(self.index.saturating_add_signed(delta));
            }
            return Outcome::Stay;
        }
        // Taken out for the click, which changes the page; put back for the
        // next click before a render replaces them.
        let hits = std::mem::take(&mut *self.hits.borrow_mut());
        let outcome = self.click(&hits, position, theme, effects);
        *self.hits.borrow_mut() = hits;
        outcome
    }

    /// A left click at `position` over the last render's `hits`.
    fn click(
        &mut self,
        hits: &Hits,
        position: Position,
        theme: &mut Theme,
        effects: &mut Vec<Effect>,
    ) -> Outcome {
        let inside = |rect: &Option<Rect>| rect.is_some_and(|rect| rect.contains(position));
        if self.prompt.is_some() {
            if let Some(answer) = hits.answers.iter().position(|rect| rect.contains(position)) {
                return self.answer(answer, theme, effects);
            }
            return Outcome::Stay;
        }
        if !inside(&hits.outer) {
            return self.leave(Outcome::Close, theme);
        }
        if inside(&hits.save) {
            self.save(theme, effects);
            return Outcome::Stay;
        }
        if let Some(item) = self.selected().cloned()
            && self.edit.is_some()
            && (inside(&hits.less) || inside(&hits.more))
        {
            self.step(&item, if inside(&hits.less) { -1 } else { 1 }, theme);
            return Outcome::Stay;
        }
        if let Some(tab) = hits.tabs.iter().position(|rect| rect.contains(position)) {
            if self.finish(theme) {
                self.switch(tab);
            }
            return Outcome::Stay;
        }
        if let Some((_, index)) = hits.rows.iter().find(|(rect, _)| rect.contains(position)) {
            // A click on the selected row (or the second of a double click) edits it.
            if *index == self.index {
                if self.edit.is_none() {
                    self.begin_edit();
                }
            } else if self.finish(theme) {
                self.select(*index);
            }
        }
        Outcome::Stay
    }

    /// Rows per page, from the last render.
    fn page_rows(&self) -> usize {
        self.hits.borrow().rows.len().max(1)
    }

    fn select(&mut self, index: usize) {
        let count = self
            .categories
            .get(self.tab)
            .map_or(0, |tab| tab.items.len());
        self.index = index.min(count.saturating_sub(1));
        self.error = None;
    }

    fn switch(&mut self, tab: usize) {
        self.tab = tab % self.categories.len().max(1);
        self.index = 0;
        self.error = None;
    }

    /// Enter on a row: edit it, typed settings starting from their value.
    fn begin_edit(&mut self) {
        let Some(source) = self.source.clone() else {
            return;
        };
        let Some(item) = self.selected().cloned() else {
            return;
        };
        let text = item.kind.typed().then(|| self.initial_text(&item));
        self.edit = Some(Edit {
            before: source,
            text,
            defaulted: false,
        });
        self.error = None;
    }

    /// The text a typed setting starts from: its value, or empty when
    /// devlanes works the default out.
    fn initial_text(&self, item: &Item) -> String {
        match self.file_value(&item.key) {
            Some(value) => display(&value),
            None => item
                .default
                .as_ref()
                .filter(|value| !value.is_null() && value.as_array().is_none_or(|a| !a.is_empty()))
                .map(humanize)
                .unwrap_or_default(),
        }
    }

    /// Esc while editing: the row goes back to its value before the edit.
    fn revert(&mut self, theme: &mut Theme) {
        if let Some(edit) = self.edit.take() {
            let previews = self
                .selected()
                .is_some_and(|item| item.applies == Applies::Now);
            self.set_working(edit.before);
            if previews {
                retheme(self.source.as_deref().unwrap_or(""), theme);
            }
        }
        self.error = None;
    }

    /// Enter while editing: keeps the value, staging what was typed. False
    /// when the config refuses it; the row stays in edit mode then.
    fn finish(&mut self, theme: &mut Theme) -> bool {
        let Some(edit) = self.edit.clone() else {
            return true;
        };
        let Some(item) = self.selected().cloned() else {
            self.edit = None;
            return true;
        };
        if let Some(text) = edit.text.filter(|_| !edit.defaulted) {
            let text = text.trim();
            // Untouched text changes nothing; a default is not written out.
            if text != self.initial_text(&item) {
                let value = match parse_typed(&item.kind, text) {
                    Ok(value) => value,
                    Err(error) => {
                        self.error = Some(error);
                        return false;
                    }
                };
                if !self.stage(value, theme) {
                    return false;
                }
            }
        }
        self.edit = None;
        true
    }

    /// `d` while editing: the default, kept in edit mode until Enter or Esc.
    fn restore(&mut self, theme: &mut Theme) {
        if self.stage(None, theme)
            && let Some(item) = self.selected().cloned()
        {
            let shown = self.shown(&item);
            if let Some(edit) = self.edit.as_mut() {
                edit.defaulted = true;
                if edit.text.is_some() {
                    edit.text = Some(shown);
                }
            }
        }
    }

    /// ← (`-1`) and → (`1`) while editing: the previous or next value. Typed
    /// numbers step their text.
    fn step(&mut self, item: &Item, delta: i64, theme: &mut Theme) {
        if self.edit.is_none() {
            return;
        }
        let current = self.current(item);
        let next = match &item.kind {
            Kind::Bool => Some(Value::Bool(!current.as_bool().unwrap_or(false))),
            Kind::Choice(choices) => {
                let mut options: Vec<Option<&str>> = Vec::new();
                if item.optional {
                    options.push(None);
                }
                options.extend(choices.iter().map(|choice| Some(choice.value.as_str())));
                let at = options
                    .iter()
                    .position(|option| *option == current.as_str())
                    .unwrap_or(0);
                let len = i64::try_from(options.len()).unwrap_or(1).max(1);
                let index = (i64::try_from(at).unwrap_or(0) + delta).rem_euclid(len);
                options
                    .get(usize::try_from(index).unwrap_or(0))
                    .copied()
                    .flatten()
                    .map(|value| Value::String(value.to_owned()))
            }
            Kind::Integer { min, max } => {
                let typed = self
                    .edit
                    .as_ref()
                    .and_then(|edit| edit.text.as_deref())
                    .and_then(|text| text.trim().parse::<i64>().ok());
                let text = match typed.or(current.as_i64()) {
                    // Unset means a default devlanes works out; start from the example.
                    None => item
                        .examples
                        .first()
                        .and_then(|example| example.parse::<i64>().ok())
                        .unwrap_or(*min)
                        .to_string(),
                    Some(value) if item.optional && value + delta < *min => String::new(),
                    Some(value) => (value + delta).clamp(*min, *max).to_string(),
                };
                if let Some(edit) = self.edit.as_mut() {
                    edit.text = Some(text);
                    edit.defaulted = false;
                }
                self.error = None;
                return;
            }
            Kind::Text | Kind::List(_) => return,
        };
        self.stage(next, theme);
    }

    /// Changes the selected setting on the page (`None` restores its
    /// default) after the config checked it; nothing is written until a
    /// save. False when the config refused it.
    fn stage(&mut self, value: Option<Value>, theme: &mut Theme) -> bool {
        let Some(item) = self.selected().cloned() else {
            return false;
        };
        let Some(source) = self.source.clone() else {
            return false;
        };
        let (key, value) = self.target(&item.key, value);
        match devlanes_config::edit_user_source(&source, &key, value.as_ref()) {
            Ok(text) => {
                self.error = None;
                self.saved_count = None;
                if text != source {
                    self.set_working(text);
                    if !self.staged.contains(&key) {
                        self.staged.push(key);
                    }
                    if item.applies == Applies::Now {
                        self.previewed = true;
                        retheme(self.source.as_deref().unwrap_or(""), theme);
                    }
                }
                true
            }
            Err(error) => {
                let message = error.message;
                self.error = Some(
                    message
                        .strip_prefix(&format!("{}: ", item.key))
                        .unwrap_or(&message)
                        .to_owned(),
                );
                false
            }
        }
    }

    /// `s`: writes every unsaved change in one request.
    fn save(&mut self, theme: &mut Theme, effects: &mut Vec<Effect>) {
        if !self.finish(theme) || self.saving.is_some() {
            return;
        }
        let dirty = self.dirty();
        if dirty.is_empty() {
            return;
        }
        let changes: Vec<(String, Option<Value>)> = dirty
            .iter()
            .map(|key| (key.clone(), pointer(&self.file, key)))
            .collect();
        let restart: Vec<String> = self
            .categories
            .iter()
            .flat_map(|category| &category.items)
            .filter(|item| item.applies == Applies::Daemon && self.changed(&item.key))
            .map(|item| item.key.clone())
            .collect();
        self.saving = Some((changes.len(), restart));
        self.error = None;
        effects.push(Effect::Settings(Request::Save { changes }));
    }

    /// Esc, q, Ctrl+C or a click outside: asks first when changes are unsaved.
    fn leave(&mut self, then: Outcome, theme: &mut Theme) -> Outcome {
        if self.edit.is_some() {
            self.revert(theme);
        }
        // A save in flight already has the changes.
        if self.saving.is_none() && self.unsaved() > 0 {
            self.prompt = Some(Prompt { choice: 0, then });
            return Outcome::Stay;
        }
        then
    }

    /// Keys of the unsaved-changes question.
    fn prompt_key(
        &mut self,
        key: KeyEvent,
        theme: &mut Theme,
        effects: &mut Vec<Effect>,
    ) -> Outcome {
        let Some(prompt) = self.prompt.as_mut() else {
            return Outcome::Stay;
        };
        match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab | KeyCode::Char('h') => {
                prompt.choice = (prompt.choice + ANSWERS.len() - 1) % ANSWERS.len();
                Outcome::Stay
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab | KeyCode::Char('l') => {
                prompt.choice = (prompt.choice + 1) % ANSWERS.len();
                Outcome::Stay
            }
            KeyCode::Enter => {
                let choice = prompt.choice;
                self.answer(choice, theme, effects)
            }
            KeyCode::Char('s' | 'y') => self.answer(0, theme, effects),
            KeyCode::Char('d' | 'n') => self.answer(1, theme, effects),
            KeyCode::Esc | KeyCode::Char('k' | 'q') => self.answer(2, theme, effects),
            _ => Outcome::Stay,
        }
    }

    /// Acts on an answer of the unsaved-changes question.
    fn answer(&mut self, answer: usize, theme: &mut Theme, effects: &mut Vec<Effect>) -> Outcome {
        let Some(prompt) = self.prompt.take() else {
            return Outcome::Stay;
        };
        match answer {
            0 => {
                self.save(theme, effects);
                // The page closes now; the restart it needs shows as a notice.
                if let Some((_, restart)) = &self.saving {
                    for key in restart {
                        if !self.restart.contains(key) {
                            self.restart.push(key.clone());
                        }
                    }
                }
                prompt.then
            }
            1 => {
                if let Some(saved) = self.saved.clone() {
                    self.set_working(saved);
                }
                if std::mem::take(&mut self.previewed) {
                    retheme(self.source.as_deref().unwrap_or(""), theme);
                }
                self.staged.clear();
                prompt.then
            }
            _ => Outcome::Stay,
        }
    }

    /// The key and value to write. An older config's `terminal: ghostty` (a
    /// terminal app name, now ignored) has no fields to set, so it becomes a
    /// mapping with the one being changed.
    fn target(&self, key: &str, value: Option<Value>) -> (String, Option<Value>) {
        if let Some(field) = key.strip_prefix("terminal.")
            && let Some(Value::String(_)) = self.file.get("terminal")
        {
            let value = value.map(|value| {
                let mut mapping = serde_json::Map::new();
                mapping.insert(field.into(), value);
                Value::Object(mapping)
            });
            return ("terminal".into(), value);
        }
        (key.to_owned(), value)
    }
}

/// The value at a dotted `key` of `file`; null counts as unset.
fn pointer(file: &Value, key: &str) -> Option<Value> {
    let pointer: String = key
        .split('.')
        .map(|segment| format!("/{segment}"))
        .collect();
    file.pointer(&pointer)
        .filter(|value| !value.is_null())
        .cloned()
}

/// Typed text as the setting's type; empty restores the default.
fn parse_typed(kind: &Kind, text: &str) -> Result<Option<Value>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(match kind {
        Kind::Integer { .. } => text
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "Enter a whole number.".to_owned())?,
        Kind::List(_) => Value::Array(
            text.split([',', ' '])
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.to_owned()))
                .collect(),
        ),
        _ => Value::String(text.to_owned()),
    }))
}

/// The colors and glyphs `source` asks for, applied to `theme`.
fn retheme(source: &str, theme: &mut Theme) {
    let config = devlanes_config::parse_user_lenient("config.yaml", source).0;
    *theme = Theme::detect(
        Some(config.tui.theme.as_str()),
        Some(config.tui.glyphs.as_str()),
        |key| std::env::var(key).ok(),
        crate::theme::classic_console(),
    );
}

/// A value as written in YAML.
fn display(value: &Value) -> String {
    match value {
        Value::Null => "default".into(),
        Value::String(text) => text.clone(),
        Value::Array(items) if items.is_empty() => "none".into(),
        Value::Array(items) => items.iter().map(display).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// A default as the user would write it: `604800000ms` is `7d`,
/// `5368709120B` is `5GiB`.
fn humanize(value: &Value) -> String {
    let Value::String(text) = value else {
        return display(value);
    };
    let digits = |number: &&str| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    if let Some(number) = text.strip_suffix("ms").filter(digits)
        && let Ok(millis) = number.parse::<u64>()
    {
        for (unit, size) in [
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1000),
        ] {
            if millis > 0 && millis.is_multiple_of(size) {
                return format!("{}{unit}", millis / size);
            }
        }
    }
    if let Some(number) = text.strip_suffix('B').filter(digits)
        && let Ok(bytes) = number.parse::<u64>()
    {
        for (unit, size) in [("GiB", 1_u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
            if bytes > 0 && bytes.is_multiple_of(size) {
                return format!("{}{unit}", bytes / size);
            }
        }
    }
    text.clone()
}

/// Runs a request off the event loop and sends the file back.
pub fn perform(request: Request, home: Option<PathBuf>, sender: mpsc::Sender<Msg>) {
    tokio::task::spawn_blocking(move || {
        let Some(home) = home else {
            let reply = Reply::Loaded {
                path: "config.yaml".into(),
                source: Err(
                    "This connection has no devlanes home to read config.yaml from.".into(),
                ),
                error: None,
            };
            let _ = sender.blocking_send(Msg::Settings(reply));
            return;
        };
        let path = home.join("config.yaml");
        let mut error = None;
        if let Request::Save { changes } = request {
            for (key, value) in changes {
                let result = match value {
                    Some(value) => devlanes_config::set_user_value(&path, &key, &value),
                    None => devlanes_config::remove_user_value(&path, &key),
                };
                if let Err(failure) = result {
                    error = Some(failure.message);
                    break;
                }
            }
        }
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => Ok(source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(format!("Cannot read {}: {error}.", path.display())),
        };
        #[cfg(unix)]
        let shown = match std::env::var_os("HOME").map(PathBuf::from) {
            Some(user) if path.starts_with(&user) => {
                format!("~/{}", path.strip_prefix(&user).unwrap_or(&path).display())
            }
            _ => path.display().to_string(),
        };
        // `USERPROFILE`: Git Bash's `HOME` (`/c/Users/acme`) never matches.
        #[cfg(windows)]
        let shown = crate::ui::home_short(
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .as_deref(),
            &path.display().to_string(),
        );
        let reply = Reply::Loaded {
            path: shown,
            source,
            error,
        };
        let _ = sender.blocking_send(Msg::Settings(reply));
    });
}

// ---- catalog --------------------------------------------------------------

/// Every editable setting of the user config schema, grouped into tabs.
pub fn catalog() -> Vec<Category> {
    let schema = devlanes_config::user_schema();
    let mut items = Vec::new();
    walk(&schema, &schema, "", &mut items);
    let place = |key: &str| {
        let section = key.split('.').next().unwrap_or(key);
        CATEGORIES
            .iter()
            .enumerate()
            .find_map(|(tab, (_, sections))| {
                sections
                    .iter()
                    .position(|s| *s == section)
                    .map(|order| (tab, order))
            })
            .unwrap_or((0, usize::MAX))
    };
    let mut placed: Vec<((usize, usize), Item)> = items
        .into_iter()
        .map(|item| (place(&item.key), item))
        .collect();
    // Sections in the tab's order; settings in schema order within them.
    placed.sort_by_key(|(at, _)| *at);
    let mut categories: Vec<Category> = CATEGORIES
        .iter()
        .map(|(name, _)| Category {
            name,
            items: Vec::new(),
        })
        .collect();
    for ((tab, _), item) in placed {
        categories[tab].items.push(item);
    }
    categories.retain(|category| !category.items.is_empty());
    categories
}

/// Follows `$ref`s to the definition.
fn resolve<'a>(mut node: &'a Value, root: &'a Value) -> &'a Value {
    for _ in 0..8 {
        match node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        {
            Some(target) => node = target,
            None => break,
        }
    }
    node
}

/// The non-null alternatives of a node (through nested `anyOf`), and
/// whether null is one of them.
fn alternatives<'a>(node: &'a Value, root: &'a Value) -> (Vec<&'a Value>, bool) {
    let node = resolve(node, root);
    let types_null = node
        .get("type")
        .and_then(Value::as_array)
        .is_some_and(|types| types.iter().any(|t| t == "null"));
    let Some(branches) = node.get("anyOf").and_then(Value::as_array) else {
        return (vec![node], types_null);
    };
    let mut out = Vec::new();
    let mut nullable = types_null;
    for branch in branches {
        let branch = resolve(branch, root);
        if branch.get("type").and_then(Value::as_str) == Some("null") {
            nullable = true;
            continue;
        }
        let (inner, null) = alternatives(branch, root);
        nullable |= null;
        out.extend(inner);
    }
    (out, nullable)
}

/// Whether a schema node allows `kind` (`"type": "x"` or `["x", "null"]`).
fn has_type(node: &Value, kind: &str) -> bool {
    match node.get("type") {
        Some(Value::String(t)) => t == kind,
        Some(Value::Array(types)) => types.iter().any(|t| t == kind),
        _ => false,
    }
}

/// The values of a `oneOf` of constants, without the reserved ones.
fn choices(node: &Value) -> Option<Vec<Choice>> {
    let branches = node.get("oneOf")?.as_array()?;
    let all: Option<Vec<Choice>> = branches
        .iter()
        .map(|branch| {
            Some(Choice {
                value: branch.get("const")?.as_str()?.to_owned(),
                about: one_line(branch.get("description")),
            })
        })
        .collect();
    Some(
        all?.into_iter()
            .filter(|choice| !choice.about.contains("not supported yet"))
            .collect(),
    )
}

/// A description on one line; some are wrapped in the Rust source.
fn one_line(description: Option<&Value>) -> String {
    description
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collects the settings under `node`'s properties.
fn walk(node: &Value, root: &Value, prefix: &str, out: &mut Vec<Item>) {
    let node = resolve(node, root);
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if SKIP.contains(&key.as_str()) {
            continue;
        }
        let (branches, optional) = alternatives(property, root);
        if let Some(section) = branches.iter().find(|b| b.get("properties").is_some()) {
            walk(section, root, &key, out);
            continue;
        }
        let Some(first) = branches.first() else {
            continue;
        };
        let kind = if let Some(choices) = choices(first) {
            Kind::Choice(choices)
        } else if has_type(first, "boolean") {
            Kind::Bool
        } else if has_type(first, "integer") {
            let max = match first.get("format").and_then(Value::as_str) {
                Some("uint16") => 65_535,
                Some("uint32") => i64::from(u32::MAX),
                _ => i64::MAX,
            };
            Kind::Integer {
                min: first.get("minimum").and_then(Value::as_i64).unwrap_or(0),
                max: first.get("maximum").and_then(Value::as_i64).unwrap_or(max),
            }
        } else if has_type(first, "array") {
            let items = first.get("items").map(|items| resolve(items, root));
            match items.and_then(choices) {
                Some(choices) => Kind::List(choices),
                None => continue,
            }
        } else if has_type(first, "string") {
            Kind::Text
        } else {
            continue;
        };
        let examples = property
            .get("examples")
            .and_then(Value::as_array)
            .map(|examples| {
                examples
                    .iter()
                    .filter(|example| !example.is_object())
                    .map(display)
                    .collect()
            })
            .unwrap_or_default();
        let applies = match key.as_str() {
            "tui.theme" | "tui.glyphs" => Applies::Now,
            _ if key.starts_with("tui.") || key.starts_with("terminal.") => Applies::NextCommand,
            _ => Applies::Daemon,
        };
        out.push(Item {
            about: one_line(property.get("description")),
            default: property.get("default").cloned(),
            key,
            kind,
            optional,
            examples,
            applies,
        });
    }
}

// ---- rendering ------------------------------------------------------------

/// Draws the options page over `area`.
pub fn render(theme: &Theme, settings: &Settings, frame: &mut Frame, area: Rect) {
    let mut hits = Hits::default();
    // A margin around the box keeps the screen's own borders from touching it.
    let outer = centered(
        area,
        area.width.saturating_sub(4).min(112),
        area.height.saturating_sub(4).min(28),
    );
    hits.outer = Some(outer);
    frame.render_widget(Clear, outer);
    let unsaved = settings.unsaved();
    let mut title = vec![Span::raw(" options ")];
    if unsaved > 0 {
        let mark = if theme.glyphs == Glyphs::Ascii {
            "*"
        } else {
            "●"
        };
        let count = match unsaved {
            1 => "1 unsaved change".to_owned(),
            n => format!("{n} unsaved changes"),
        };
        title.push(Span::styled(
            format!("{mark} {count} "),
            theme.tone(Tone::Attention),
        ));
    }
    let mut block = floating_box(theme).title(Line::from(title));
    if let Some(path) = &settings.path
        && usize::from(outer.width) > text::width(path) + 40
    {
        block = block
            .title(Line::from(Span::styled(format!(" {path} "), theme.quiet())).right_aligned());
    }
    let save_label = "[save]";
    let save_style = if unsaved > 0 {
        theme.focus().add_modifier(Modifier::BOLD)
    } else {
        theme.quiet()
    };
    let save_width = u16::try_from(save_label.len()).unwrap_or(0);
    block = block
        .title_bottom(hint_line(
            theme,
            hints(settings),
            usize::from(outer.width).saturating_sub(usize::from(save_width) + 6),
        ))
        .title_bottom(
            Line::from(vec![Span::styled(save_label, save_style), Span::raw(" ")]).right_aligned(),
        );
    hits.save = Some(Rect::new(
        outer.right().saturating_sub(save_width + 2),
        outer.bottom().saturating_sub(1),
        save_width,
        1,
    ));
    frame.render_widget(block, outer);
    let inner = Rect::new(
        outer.x + 2,
        outer.y + 1,
        outer.width.saturating_sub(4),
        outer.height.saturating_sub(2),
    );
    if inner.height < 4 || inner.width < 10 {
        *settings.hits.borrow_mut() = hits;
        return;
    }
    render_tabs(theme, settings, frame, inner, &mut hits);
    let body = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 3);
    let status = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
    let notice = match (&settings.load_error, &settings.source) {
        (Some(error), _) => Some(Line::styled(error.clone(), theme.tone(Tone::Failure))),
        (None, None) => Some(Line::styled("loading…", theme.quiet())),
        (None, Some(_)) => None,
    };
    if let Some(notice) = notice {
        let lines: Vec<Line> = text::wrap(&notice.to_string(), usize::from(body.width))
            .into_iter()
            .map(|line| Line::styled(line, notice.style))
            .collect();
        frame.render_widget(Paragraph::new(lines), body);
        *settings.hits.borrow_mut() = hits;
        return;
    }
    let wide = body.width >= 70;
    let (list, about) = if wide {
        // As wide as the longest row of any tab, so it does not jump with the tab.
        let longest = settings
            .categories
            .iter()
            .flat_map(|category| &category.items)
            .map(|item| 2 + text::width(&item.key) + 3 + text::width(&settings.shown(item)) + 6)
            .max()
            .unwrap_or(0);
        let longest = u16::try_from(longest).unwrap_or(u16::MAX).max(30);
        let list_width = (body.width * 11 / 20).min(56).min(longest);
        let bar = if theme.glyphs == Glyphs::Ascii {
            "|"
        } else {
            "│"
        };
        frame.render_widget(
            Paragraph::new(
                (0..body.height)
                    .map(|_| Line::styled(bar, theme.rule()))
                    .collect::<Vec<_>>(),
            ),
            Rect::new(body.x + list_width, body.y, 1, body.height),
        );
        (
            Rect::new(body.x, body.y, list_width, body.height),
            Rect::new(
                body.x + list_width + 2,
                body.y,
                body.width.saturating_sub(list_width + 2),
                body.height,
            ),
        )
    } else {
        // Narrow: the description's first lines under the list.
        let about_height = if body.height > 5 { 2 } else { 0 };
        (
            Rect::new(body.x, body.y, body.width, body.height - about_height),
            Rect::new(
                body.x,
                body.bottom() - about_height,
                body.width,
                about_height,
            ),
        )
    };
    let pages = render_list(theme, settings, frame, list, &mut hits);
    if let Some(item) = settings.selected() {
        render_about(theme, settings, item, frame, about, wide);
    }
    render_status(theme, settings, frame, status, pages);
    if let Some(prompt) = &settings.prompt {
        render_prompt(theme, prompt, unsaved, frame, outer, &mut hits);
    }
    *settings.hits.borrow_mut() = hits;
}

/// The key hints for the page's mode.
fn hints(settings: &Settings) -> &'static [(&'static str, &'static str)] {
    let kind = settings.selected().map(|item| &item.kind);
    match (&settings.edit, kind) {
        (Some(_), Some(Kind::Text | Kind::List(_))) => &[
            ("type", "a value"),
            ("enter", "done"),
            ("esc", "undo"),
            ("del", "default"),
        ],
        (Some(_), Some(Kind::Integer { .. })) => &[
            ("←→", "step"),
            ("enter", "done"),
            ("esc", "undo"),
            ("d", "default"),
        ],
        (Some(_), _) => &[
            ("←→", "change"),
            ("enter", "done"),
            ("esc", "undo"),
            ("d", "default"),
        ],
        (None, _) => &[
            ("↑↓", "select"),
            ("←→", "section"),
            ("enter", "edit"),
            ("d", "default"),
            ("s", "save"),
            ("esc", "close"),
        ],
    }
}

/// `1 general   2 network …` and the rule under them.
fn render_tabs(
    theme: &Theme,
    settings: &Settings,
    frame: &mut Frame,
    inner: Rect,
    hits: &mut Hits,
) {
    let mut x = inner.x;
    for (index, category) in settings.categories.iter().enumerate() {
        let label = format!("{} {}", index + 1, category.name);
        let width = u16::try_from(text::width(&label)).unwrap_or(0);
        if x + width > inner.right() {
            break;
        }
        let style = if index == settings.tab {
            theme
                .focus()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::new()
        };
        let rect = Rect::new(x, inner.y, width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{} ", index + 1), theme.quiet()),
                Span::styled(category.name, style),
            ])),
            rect,
        );
        hits.tabs.push(rect);
        x += width + 3;
    }
    let rule = if theme.glyphs == Glyphs::Ascii {
        "-"
    } else {
        "─"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            rule.repeat(usize::from(inner.width)),
            theme.rule(),
        )),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
}

/// `key label  key label`, cut to `width`.
fn hint_line(theme: &Theme, pairs: &[(&str, &str)], width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, (key, label)) in pairs.iter().enumerate() {
        let next = text::width(key) + text::width(label) + 4;
        if spans_width(&spans) + next > width {
            break;
        }
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled((*key).to_owned(), theme.key()));
        spans.push(Span::styled(format!(" {label}"), theme.quiet()));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The rows of the current page; returns the page and the page count.
fn render_list(
    theme: &Theme,
    settings: &Settings,
    frame: &mut Frame,
    area: Rect,
    hits: &mut Hits,
) -> (usize, usize) {
    let Some(category) = settings.categories.get(settings.tab) else {
        return (0, 1);
    };
    let rows = usize::from(area.height).max(1);
    let pages = category.items.len().div_ceil(rows).max(1);
    let page = settings.index / rows;
    let width = usize::from(area.width);
    // Names give way to values only when both do not fit; the 6 is ◂ ▸ and
    // the changed mark.
    let longest_value = category
        .items
        .iter()
        .map(|item| text::width(&settings.shown(item)) + 6)
        .max()
        .unwrap_or(0)
        .min(width / 2);
    let name_width = category
        .items
        .iter()
        .map(|item| text::width(&item.key))
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(longest_value + 3));
    let value_width = width.saturating_sub(name_width + 3);
    let ascii = theme.glyphs == Glyphs::Ascii;
    let (less, more) = if ascii { ("<", ">") } else { ("◂", "▸") };
    for (offset, (index, item)) in category
        .items
        .iter()
        .enumerate()
        .skip(page * rows)
        .take(rows)
        .enumerate()
    {
        let y = area.y + u16::try_from(offset).unwrap_or(0);
        let rect = Rect::new(area.x, y, area.width, 1);
        hits.rows.push((rect, index));
        let selected = index == settings.index;
        let editing = selected.then_some(settings.edit.as_ref()).flatten();
        let problem = settings.problems.contains_key(&item.key);
        let value_style = if problem {
            theme.tone(Tone::Attention)
        } else if settings.file_value(&item.key).is_some() {
            Style::new()
        } else {
            // Defaults are dimmed; values written in the file are not.
            theme.quiet()
        };
        let mut value: Vec<Span<'static>> = Vec::new();
        if settings.changed(&item.key) {
            let mark = if ascii { "* " } else { "● " };
            value.push(Span::styled(mark, theme.tone(Tone::Attention)));
        }
        let mut chip = Vec::new();
        match editing {
            Some(edit) if edit.text.is_some() && !edit.defaulted => {
                let typed = edit.text.clone().unwrap_or_default();
                let room = value_width.saturating_sub(4);
                // The end of the text stays in view while typing.
                let chars: Vec<char> = typed.chars().collect();
                let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
                chip.push(Span::raw(" "));
                chip.push(Span::raw(shown));
                chip.push(Span::raw(if ascii { "_" } else { "▏" }));
                chip.push(Span::raw(" "));
            }
            Some(_) => {
                let room = value_width.saturating_sub(6);
                chip.push(Span::raw(format!("{less} ")));
                chip.push(Span::raw(clip(&settings.shown(item), room)));
                chip.push(Span::raw(format!(" {more}")));
            }
            None => {
                let mut shown = settings.shown(item);
                if problem {
                    shown = format!("{} {shown}", theme.glyph(Glyph::Attention));
                }
                value.push(Span::styled(clip(&shown, value_width), value_style));
            }
        }
        let value_cells = spans_width(&value) + spans_width(&chip);
        let marker = if selected {
            theme.glyph(Glyph::Marker)
        } else {
            " "
        };
        let name = clip(&item.key, name_width);
        let name_cells = text::width(&name);
        let gap = width.saturating_sub(2 + name_cells + value_cells);
        let mut spans = vec![
            Span::styled(marker, theme.focus()),
            Span::raw(" "),
            Span::raw(name),
            Span::raw(" ".repeat(gap)),
        ];
        spans.extend(value);
        let mut line = fit(spans, width);
        if selected {
            line = line.patch_style(theme.selection());
        }
        if editing.is_some() {
            // The edited value is a chip in the focus color, cut out of the
            // selection so the row reads as being edited.
            let style = editing_style(theme);
            let chip_cells = spans_width(&chip);
            line.spans.extend(
                chip.into_iter()
                    .map(|span| Span::styled(span.content, style)),
            );
            let end = area.x + u16::try_from(2 + name_cells + gap + value_cells).unwrap_or(0);
            let start = end.saturating_sub(u16::try_from(chip_cells).unwrap_or(0));
            hits.less = Some(Rect::new(start, y, 2, 1));
            hits.more = Some(Rect::new(end.saturating_sub(2), y, 2, 1));
        }
        frame.render_widget(Paragraph::new(line), rect);
    }
    (page, pages)
}

/// The value being edited: reverse video in the focus color, so it stands
/// apart from the selected row in every color tier.
fn editing_style(theme: &Theme) -> Style {
    let mut style = theme.focus().add_modifier(Modifier::BOLD);
    if theme.reverse_selection() {
        // The row is reversed already; a plain cell stands out of it.
        style = style.remove_modifier(Modifier::REVERSED);
        style.bg = Some(ratatui::style::Color::Reset);
    } else {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// The description of the selected setting: what it does, its values, the
/// default and when a change applies.
fn render_about(
    theme: &Theme,
    settings: &Settings,
    item: &Item,
    frame: &mut Frame,
    area: Rect,
    wide: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = usize::from(area.width);
    let wrapped = |text: &str, style: Style| -> Vec<Line<'static>> {
        text::wrap(text, width)
            .into_iter()
            .map(|line| Line::styled(line, style))
            .collect()
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(error) = &settings.error {
        lines.extend(wrapped(
            &format!("{} {error}", theme.glyph(Glyph::Failed)),
            theme.tone(Tone::Failure),
        ));
    }
    if !wide {
        if lines.is_empty() {
            lines.extend(wrapped(&item.about, theme.quiet()));
        }
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        item.key.clone(),
        Style::new().add_modifier(Modifier::BOLD),
    ));
    lines.extend(wrapped(&item.about, Style::new()));
    let current = settings.current(item);
    let options = match &item.kind {
        Kind::Choice(choices) | Kind::List(choices) => Some(choices),
        _ => None,
    };
    if let Some(choices) = options {
        lines.push(Line::from(""));
        let value_width = choices
            .iter()
            .map(|choice| text::width(&choice.value))
            .max()
            .unwrap_or(0);
        for choice in choices {
            let chosen = match &current {
                Value::String(value) => *value == choice.value,
                Value::Array(values) => values.iter().any(|value| *value == choice.value),
                _ => false,
            };
            let (mark, style) = if chosen {
                (
                    theme.glyph(Glyph::Marker),
                    theme.focus().add_modifier(Modifier::BOLD),
                )
            } else {
                (" ", Style::new())
            };
            // The description wraps under itself, right of the values.
            let indent = 1 + value_width + 2;
            let about = text::wrap(&choice.about, width.saturating_sub(indent).max(8));
            for (index, part) in about.into_iter().enumerate() {
                let mut spans = if index == 0 {
                    vec![
                        Span::styled(mark, theme.focus()),
                        Span::styled(format!("{:<value_width$}  ", choice.value), style),
                    ]
                } else {
                    vec![Span::raw(" ".repeat(indent))]
                };
                spans.push(Span::styled(part, theme.quiet()));
                lines.push(fit(spans, width));
            }
        }
    }
    lines.push(Line::from(""));
    let default = match &item.default {
        None | Some(Value::Null) => "worked out by devlanes".to_owned(),
        Some(value) => humanize(value),
    };
    lines.push(Line::from(vec![
        Span::styled("default  ", theme.quiet()),
        Span::raw(default),
    ]));
    if settings.changed(&item.key) {
        let saved = pointer(&settings.saved_file, &item.key)
            .map_or_else(|| "default".to_owned(), |value| display(&value));
        lines.push(Line::from(vec![
            Span::styled("saved    ", theme.quiet()),
            Span::raw(saved),
        ]));
    }
    if options.is_none() && !item.examples.is_empty() {
        lines.push(fit(
            vec![
                Span::styled("example  ", theme.quiet()),
                Span::raw(item.examples.join(", ")),
            ],
            width,
        ));
    }
    lines.push(Line::from(""));
    let applies = match item.applies {
        Applies::Now => "Previews at once; s saves it.",
        Applies::NextCommand => "Applies to the next devlanes command once saved.",
        Applies::Daemon => "Applies after saving and a daemon restart.",
    };
    lines.extend(wrapped(applies, theme.quiet()));
    if let Some(problem) = settings.problems.get(&item.key) {
        lines.push(Line::from(""));
        lines.extend(wrapped(
            &format!(
                "{} devlanes ignores the value in config.yaml ({problem}) and uses the default.",
                theme.glyph(Glyph::Attention)
            ),
            theme.tone(Tone::Attention),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The bottom row: unsaved changes, what the last save needs, and the page.
fn render_status(
    theme: &Theme,
    settings: &Settings,
    frame: &mut Frame,
    area: Rect,
    (page, pages): (usize, usize),
) {
    let mut left = Vec::new();
    let unsaved = settings.unsaved();
    if settings.saving.is_some() {
        left.push(Span::styled("saving…", theme.quiet()));
    } else if unsaved > 0 {
        left.push(Span::styled(
            "not saved yet   ",
            theme.tone(Tone::Attention),
        ));
        left.push(Span::styled("s", theme.key()));
        left.push(Span::styled(" saves to config.yaml", theme.quiet()));
    } else if let Some(first) = settings.restart.first() {
        let what = match settings.restart.len() {
            1 => format!("saved; {first} applies after a daemon restart"),
            n => format!(
                "saved; {first} and {} more apply after a daemon restart",
                n - 1
            ),
        };
        left.push(Span::styled(
            format!("{} {what}   ", theme.glyph(Glyph::Attention)),
            theme.tone(Tone::Attention),
        ));
        left.push(Span::styled("R", theme.key()));
        left.push(Span::styled(" restart now", theme.quiet()));
    } else if let Some(count) = settings.saved_count {
        let what = match count {
            1 => "saved 1 change".to_owned(),
            n => format!("saved {n} changes"),
        };
        left.push(Span::styled(
            format!("{} {what}", theme.glyph(Glyph::Done)),
            theme.tone(Tone::Ok),
        ));
    }
    let mut right = Vec::new();
    if pages > 1 {
        let (less, more) = if theme.glyphs == Glyphs::Ascii {
            ("<", ">")
        } else {
            ("◂", "▸")
        };
        right.push(Span::styled(
            format!("{less} page {}/{pages} {more}", page + 1),
            theme.quiet(),
        ));
    }
    frame.render_widget(
        Paragraph::new(spread(left, right, usize::from(area.width))),
        area,
    );
}

/// The unsaved-changes question over the page.
fn render_prompt(
    theme: &Theme,
    prompt: &Prompt,
    unsaved: usize,
    frame: &mut Frame,
    outer: Rect,
    hits: &mut Hits,
) {
    let sentence = match unsaved {
        1 => "1 setting changed and is not saved.".to_owned(),
        n => format!("{n} settings changed and are not saved."),
    };
    let buttons_width: usize = ANSWERS
        .iter()
        .map(|(key, label)| text::width(key) + text::width(label) + 3)
        .sum::<usize>()
        + 2 * (ANSWERS.len() - 1);
    let width = u16::try_from(buttons_width.max(text::width(&sentence)) + 6).unwrap_or(u16::MAX);
    let rect = centered(outer, width, 7);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        floating_box(theme).title(Span::raw(" unsaved changes ")),
        rect,
    );
    let inner = Rect::new(
        rect.x + 2,
        rect.y + 2,
        rect.width.saturating_sub(4),
        rect.height.saturating_sub(4),
    );
    frame.render_widget(
        Paragraph::new(Line::from(clip(&sentence, usize::from(inner.width)))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let mut x = inner.x;
    let y = inner.y + 2;
    for (index, (key, label)) in ANSWERS.iter().enumerate() {
        let cells = u16::try_from(text::width(key) + text::width(label) + 3).unwrap_or(0);
        let chosen = index == prompt.choice;
        let spans = vec![
            Span::raw(" "),
            Span::styled((*key).to_owned(), theme.key()),
            Span::raw(format!(" {label} ")),
        ];
        let mut line = Line::from(spans);
        if chosen {
            line = line.patch_style(theme.selection());
        }
        let button = Rect::new(x, y, cells, 1).intersection(inner);
        frame.render_widget(Paragraph::new(line), button);
        hits.answers.push(button);
        x += cells + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn press(settings: &mut Settings, code: KeyCode, theme: &mut Theme) -> Vec<Effect> {
        let mut effects = Vec::new();
        settings.key(KeyEvent::new(code, KeyModifiers::NONE), theme, &mut effects);
        effects
    }

    fn outcome(settings: &mut Settings, code: KeyCode, theme: &mut Theme) -> Outcome {
        settings.key(
            KeyEvent::new(code, KeyModifiers::NONE),
            theme,
            &mut Vec::new(),
        )
    }

    fn type_text(settings: &mut Settings, text: &str, theme: &mut Theme) {
        for c in text.chars() {
            press(settings, KeyCode::Char(c), theme);
        }
    }

    /// The page with `source` loaded and `key` selected.
    fn open(source: &str, key: &str) -> Settings {
        let mut settings = Settings::new();
        settings.reply(Reply::Loaded {
            path: "~/.devlanes/config.yaml".into(),
            source: Ok(source.into()),
            error: None,
        });
        let (tab, index) = settings
            .categories
            .iter()
            .enumerate()
            .find_map(|(tab, category)| {
                let index = category.items.iter().position(|item| item.key == key)?;
                Some((tab, index))
            })
            .expect("setting");
        settings.tab = tab;
        settings.index = index;
        settings
    }

    /// The changes of the save request in `effects`.
    fn saved(effects: &[Effect]) -> Vec<(String, Option<Value>)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Settings(Request::Save { changes }) => Some(changes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// What the runtime answers once a save wrote `source`.
    fn written(settings: &mut Settings, source: &str) {
        settings.reply(Reply::Loaded {
            path: "~/.devlanes/config.yaml".into(),
            source: Ok(source.into()),
            error: None,
        });
    }

    #[test]
    fn the_catalog_covers_the_user_config() {
        let categories = catalog();
        let names: Vec<_> = categories.iter().map(|c| c.name).collect();
        assert_eq!(names, ["general", "network", "startup", "storage"]);
        let keys: Vec<&str> = categories
            .iter()
            .flat_map(|c| c.items.iter().map(|item| item.key.as_str()))
            .collect();
        for key in [
            "tui.theme",
            "tui.glyphs",
            "tui.open_on_start",
            "terminal.progress",
            "updates.check",
            "proxy.https_port",
            "hostnames.resolver",
            "hostnames.tld",
            "ports.range",
            "tls.trust",
            "startup.mode",
            "startup.max",
            "logs.retention.days",
            "logs.retention.max_size",
            "metrics.retention",
            "history.retention",
            "state.max_size",
        ] {
            assert!(keys.contains(&key), "{key} missing from {keys:?}");
        }
        assert!(!keys.contains(&"hostnames.dns_provider"));
        assert!(categories[0].items[0].key.starts_with("tui."));
        let resolver = categories[1]
            .items
            .iter()
            .find(|item| item.key == "hostnames.resolver")
            .expect("resolver");
        let Kind::Choice(choices) = &resolver.kind else {
            panic!("{:?}", resolver.kind);
        };
        // The reserved public resolver is not offered.
        assert!(choices.iter().all(|choice| choice.value != "public"));
    }

    #[test]
    fn arrows_move_rows_and_tabs_and_enter_edits_in_place() {
        let mut theme = Theme::default();
        let mut settings = open("", "tui.open_on_start");
        let first = settings.index;
        press(&mut settings, KeyCode::Down, &mut theme);
        assert_eq!(settings.index, first + 1);
        press(&mut settings, KeyCode::Char('k'), &mut theme);
        assert_eq!(settings.index, first);
        // ← → change tabs, not values.
        press(&mut settings, KeyCode::Right, &mut theme);
        assert_eq!(settings.categories[settings.tab].name, "network");
        press(&mut settings, KeyCode::Left, &mut theme);
        press(&mut settings, KeyCode::Left, &mut theme);
        assert_eq!(settings.categories[settings.tab].name, "storage");
        press(&mut settings, KeyCode::Char('3'), &mut theme);
        assert_eq!(settings.categories[settings.tab].name, "startup");
        press(&mut settings, KeyCode::BackTab, &mut theme);
        press(&mut settings, KeyCode::Tab, &mut theme);
        assert_eq!(settings.categories[settings.tab].name, "startup");
        assert_eq!(settings.unsaved(), 0);

        // Enter edits the row without moving; Enter again leaves edit mode.
        let mut settings = open("", "updates.check");
        let row = settings.index;
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert!(settings.edit.is_some());
        assert_eq!(settings.index, row);
        press(&mut settings, KeyCode::Right, &mut theme);
        assert_eq!(settings.file_value("updates.check"), Some(json!(false)));
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert!(settings.edit.is_none());
        assert_eq!(settings.index, row);
        assert_eq!(settings.unsaved(), 1);
    }

    #[test]
    fn esc_in_edit_mode_reverts_only_that_row() {
        let mut theme = Theme::default();
        let mut settings = open("updates:\n  check: true\n", "hostnames.hosts_file");
        // A change kept with Enter on another row stays.
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(
            settings.file_value("hostnames.hosts_file"),
            Some(json!("auto"))
        );
        press(&mut settings, KeyCode::Down, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        type_text(&mut settings, "test", &mut theme);
        press(&mut settings, KeyCode::Esc, &mut theme);
        assert!(settings.edit.is_none());
        assert_eq!(settings.file_value("hostnames.tld"), None);
        // Esc reverts a cycled value too.
        press(&mut settings, KeyCode::Up, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        assert_eq!(
            settings.file_value("hostnames.hosts_file"),
            Some(json!("never"))
        );
        press(&mut settings, KeyCode::Esc, &mut theme);
        assert_eq!(
            settings.file_value("hostnames.hosts_file"),
            Some(json!("auto"))
        );
        assert_eq!(settings.unsaved(), 1);
    }

    #[test]
    fn numbers_step_and_type_and_are_checked() {
        let mut theme = Theme::default();
        let mut settings = open("proxy:\n  https_port: 8443\n", "proxy.https_port");
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(
            settings.edit.as_ref().and_then(|e| e.text.as_deref()),
            Some("8443")
        );
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.file_value("proxy.https_port"), Some(json!(8444)));
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Backspace, &mut theme);
        press(&mut settings, KeyCode::Char('x'), &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.error.as_deref(), Some("Enter a whole number."));
        assert!(
            settings.edit.is_some(),
            "a refused value stays in edit mode"
        );
        // The config's own checks: http and https must differ.
        press(&mut settings, KeyCode::Backspace, &mut theme);
        press(&mut settings, KeyCode::Backspace, &mut theme);
        press(&mut settings, KeyCode::Backspace, &mut theme);
        press(&mut settings, KeyCode::Backspace, &mut theme);
        type_text(&mut settings, "80", &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(
            settings.error.as_deref(),
            Some("must differ from proxy.http_port")
        );
        assert!(settings.edit.is_some());
        // d puts the default back; Enter keeps it.
        press(&mut settings, KeyCode::Char('d'), &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.file_value("proxy.https_port"), None);
        assert!(settings.edit.is_none());
        // An optional number steps below its minimum to unset.
        let mut settings = open("startup:\n  max: 1\n", "startup.max");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Left, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.file_value("startup.max"), None);
    }

    #[test]
    fn durations_sizes_and_lists_are_typed_and_checked() {
        let mut theme = Theme::default();
        let mut settings = open("", "history.retention");
        let item = settings.selected().expect("item").clone();
        assert_eq!(settings.shown(&item), "7d");
        press(&mut settings, KeyCode::Enter, &mut theme);
        // Enter on an untouched default does not write it out.
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.unsaved(), 0);
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Backspace, &mut theme);
        press(&mut settings, KeyCode::Backspace, &mut theme);
        // d is a character while typing: durations end in it.
        type_text(&mut settings, "soon", &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert!(
            settings
                .error
                .as_deref()
                .is_some_and(|error| error.contains("invalid duration")),
            "{:?}",
            settings.error
        );
        settings.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut theme,
            &mut Vec::new(),
        );
        type_text(&mut settings, "14d", &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.file_value("history.retention"), Some(json!("14d")));
        // d on the row restores the default.
        press(&mut settings, KeyCode::Char('d'), &mut theme);
        assert_eq!(settings.file_value("history.retention"), None);

        let settings = open("", "state.max_size");
        let item = settings.selected().expect("item").clone();
        assert_eq!(settings.shown(&item), "512MiB");

        let mut settings = open("", "tls.trust");
        press(&mut settings, KeyCode::Enter, &mut theme);
        settings.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut theme,
            &mut Vec::new(),
        );
        type_text(&mut settings, "system, firefox", &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(
            settings.file_value("tls.trust"),
            Some(json!(["system", "firefox"]))
        );
        press(&mut settings, KeyCode::Enter, &mut theme);
        settings.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut theme,
            &mut Vec::new(),
        );
        type_text(&mut settings, "safari", &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert!(settings.error.is_some());
    }

    #[test]
    fn changes_wait_for_s_and_save_in_one_request() {
        let mut theme = Theme::default();
        let mut settings = open("updates:\n  check: true # daily\n", "updates.check");
        press(&mut settings, KeyCode::Enter, &mut theme);
        let effects = press(&mut settings, KeyCode::Right, &mut theme);
        assert!(
            saved(&effects).is_empty(),
            "nothing is written while editing"
        );
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        settings.index = settings.categories[settings.tab]
            .items
            .iter()
            .position(|item| item.key == "proxy.lan")
            .expect("proxy.lan");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.unsaved(), 2);
        assert!(settings.restart.is_empty());
        let effects = press(&mut settings, KeyCode::Char('s'), &mut theme);
        assert_eq!(
            saved(&effects),
            [
                ("updates.check".to_owned(), Some(json!(false))),
                ("proxy.lan".to_owned(), Some(json!(true))),
            ]
        );
        written(
            &mut settings,
            "updates:\n  check: false # daily\nproxy:\n  lan: true\n",
        );
        assert_eq!(settings.unsaved(), 0);
        assert_eq!(settings.saved_count, Some(2));
        assert_eq!(settings.restart, ["updates.check", "proxy.lan"]);
        assert_eq!(
            outcome(&mut settings, KeyCode::Char('R'), &mut theme),
            Outcome::RestartDaemon
        );
        // Ctrl+S saves too; a value changed back is not a change.
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(settings.unsaved(), 0);
        let mut effects = Vec::new();
        settings.key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut theme,
            &mut effects,
        );
        assert!(saved(&effects).is_empty());
    }

    #[test]
    fn closing_with_unsaved_changes_asks_first() {
        let mut theme = Theme::default();
        let mut settings = open("", "updates.check");
        assert_eq!(
            outcome(&mut settings, KeyCode::Esc, &mut theme),
            Outcome::Menu
        );
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        // Keep editing.
        assert_eq!(
            outcome(&mut settings, KeyCode::Esc, &mut theme),
            Outcome::Stay
        );
        assert!(settings.prompt.is_some());
        assert_eq!(
            outcome(&mut settings, KeyCode::Esc, &mut theme),
            Outcome::Stay
        );
        assert!(settings.prompt.is_none());
        assert_eq!(settings.unsaved(), 1);
        // Save changes: one request, then the page goes where it was going.
        assert_eq!(
            outcome(&mut settings, KeyCode::Char('q'), &mut theme),
            Outcome::Stay
        );
        let mut effects = Vec::new();
        let gone = settings.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut theme,
            &mut effects,
        );
        assert_eq!(gone, Outcome::Menu);
        assert_eq!(
            saved(&effects),
            [("updates.check".to_owned(), Some(json!(false)))]
        );
        assert_eq!(settings.restart, ["updates.check"]);
        // Discard: nothing is written.
        let mut settings = open("", "updates.check");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        let mut effects = Vec::new();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            settings.key(ctrl_c, &mut theme, &mut effects),
            Outcome::Stay
        );
        let gone = settings.key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut theme,
            &mut effects,
        );
        assert_eq!(gone, Outcome::Quit);
        assert!(effects.is_empty());
        assert_eq!(settings.unsaved(), 0);
    }

    #[test]
    fn previews_revert_on_esc_and_on_discard() {
        // What the saved file (all defaults) gives in this environment.
        let saved = Theme::detect(
            Some("auto"),
            Some("auto"),
            |key| std::env::var(key).ok(),
            crate::theme::classic_console(),
        );
        let mut theme = Theme::default();
        let mut settings = open("", "tui.glyphs");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Left, &mut theme);
        assert_eq!(theme.glyphs, Glyphs::Ascii, "the preview is live");
        press(&mut settings, KeyCode::Esc, &mut theme);
        assert_eq!(theme, saved, "Esc undoes the preview");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Left, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert_eq!(theme.glyphs, Glyphs::Ascii);
        press(&mut settings, KeyCode::Esc, &mut theme);
        assert_eq!(
            outcome(&mut settings, KeyCode::Char('d'), &mut theme),
            Outcome::Menu
        );
        assert_eq!(theme, saved, "discard undoes the preview");
    }

    #[test]
    fn an_old_terminal_app_name_becomes_a_mapping() {
        let mut theme = Theme::default();
        let mut settings = open("terminal: ghostty # old\n", "terminal.progress");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        let effects = press(&mut settings, KeyCode::Char('s'), &mut theme);
        assert_eq!(
            saved(&effects),
            [("terminal".to_owned(), Some(json!({"progress": "always"})))]
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_not_edited() {
        let mut theme = Theme::default();
        let mut settings = Settings::new();
        settings.reply(Reply::Loaded {
            path: "config.yaml".into(),
            source: Ok("tui: [".into()),
            error: None,
        });
        assert!(settings.load_error.is_some());
        press(&mut settings, KeyCode::Enter, &mut theme);
        assert!(settings.edit.is_none());
    }

    #[test]
    fn clicks_switch_tabs_select_rows_and_edit_the_selected_one() {
        let mut theme = Theme::default();
        let mut settings = open("", "tui.open_on_start");
        let draw = |settings: &Settings, theme: &Theme| {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40))
                .expect("terminal");
            terminal
                .draw(|frame| render(theme, settings, frame, frame.area()))
                .expect("draw");
        };
        let click = |rect: Rect| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        draw(&settings, &theme);
        let tab = settings.hits.borrow().tabs[1];
        settings.mouse(click(tab), &mut theme, &mut Vec::new());
        assert_eq!(settings.tab, 1);
        draw(&settings, &theme);
        let (row, index) = settings.hits.borrow().rows[2];
        settings.mouse(click(row), &mut theme, &mut Vec::new());
        assert_eq!(settings.index, index);
        assert!(settings.edit.is_none());
        settings.mouse(click(row), &mut theme, &mut Vec::new());
        assert!(
            settings.edit.is_some(),
            "a click on the selected row edits it"
        );
        // [save] in the border.
        press(&mut settings, KeyCode::Esc, &mut theme);
        settings.index = settings.categories[1]
            .items
            .iter()
            .position(|item| item.key == "proxy.lan")
            .expect("proxy.lan");
        press(&mut settings, KeyCode::Enter, &mut theme);
        press(&mut settings, KeyCode::Right, &mut theme);
        press(&mut settings, KeyCode::Enter, &mut theme);
        draw(&settings, &theme);
        let save = settings.hits.borrow().save.expect("save");
        let mut effects = Vec::new();
        settings.mouse(click(save), &mut theme, &mut effects);
        assert_eq!(
            saved(&effects),
            [("proxy.lan".to_owned(), Some(json!(true)))]
        );
    }

    #[test]
    fn humanized_defaults() {
        assert_eq!(humanize(&json!("604800000ms")), "7d");
        assert_eq!(humanize(&json!("20000ms")), "20s");
        assert_eq!(humanize(&json!("1500ms")), "1500ms");
        assert_eq!(humanize(&json!("5368709120B")), "5GiB");
        assert_eq!(humanize(&json!("75%")), "75%");
        assert_eq!(humanize(&json!(null)), "default");
    }
}
