//! tmuxinator: the project-local `.tmuxinator.yml`. Each pane that runs a program becomes a
//! service; editors and plain shells are left out.

use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use super::{Context, OneOrMany, Source, existing, typed, unknown_top};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit, When},
};

/// The tmuxinator source.
pub(crate) struct Tmuxinator;

/// `tmuxinator new --local` writes `.tmuxinator.yml`; `.yaml` is accepted too.
const FILES: [&str; 2] = [".tmuxinator.yml", ".tmuxinator.yaml"];

/// Settings that only shape the tmux session.
const SESSION_KEYS: [&str; 10] = [
    "name",
    "project_name",
    "socket_name",
    "tmux_options",
    "tmux_command",
    "startup_window",
    "startup_pane",
    "attach",
    "enable_pane_titles",
    "pane_title_position",
];

/// Programs that need a person at the terminal; a pane running one is not a service.
const INTERACTIVE: [&str; 24] = [
    "vim",
    "nvim",
    "vi",
    "emacs",
    "nano",
    "hx",
    "helix",
    "micro",
    "htop",
    "top",
    "btop",
    "lazygit",
    "lazydocker",
    "tig",
    "less",
    "more",
    "man",
    "bash",
    "zsh",
    "fish",
    "sh",
    "ssh",
    "git",
    "clear",
];

/// A whole `.tmuxinator.yml`.
#[derive(Debug, Deserialize)]
struct File {
    root: Option<String>,
    pre_window: Option<OneOrMany>,
    pre_tab: Option<OneOrMany>,
    on_project_start: Option<OneOrMany>,
    on_project_first_start: Option<OneOrMany>,
    on_project_stop: Option<OneOrMany>,
    on_project_restart: Option<OneOrMany>,
    on_project_exit: Option<OneOrMany>,
    pre: Option<OneOrMany>,
    post: Option<OneOrMany>,
    #[serde(default)]
    windows: Vec<IndexMap<String, Value>>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// A window in its mapping form.
#[derive(Debug, Default, Deserialize)]
struct Window {
    root: Option<String>,
    #[serde(default)]
    panes: Vec<Value>,
    pre: Option<OneOrMany>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

impl Source for Tmuxinator {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        existing(dir, &FILES)
            .into_iter()
            .next()
            .map(|file| vec![file])
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::Tmuxinator);
        let Some(file) = files.first() else {
            return Ok(draft);
        };
        let parsed: File = cx.yaml(file)?;
        let mut other = parsed.other.clone();
        for key in SESSION_KEYS {
            other.shift_remove(key);
        }
        unknown_top(&mut draft, file, &other);
        let mut names = Names::default();
        let root = session_root(cx, &mut draft, file, parsed.root.as_deref());
        let before: Vec<String> = parsed
            .pre_window
            .as_ref()
            .or(parsed.pre_tab.as_ref())
            .map(OneOrMany::items)
            .unwrap_or_default();
        if !before.is_empty() {
            draft.note(file, "pre_window runs before every pane's commands");
        }
        for (hook, when, commands) in [
            (
                "on-project-start",
                Some(When::Start),
                &parsed.on_project_start,
            ),
            ("pre", Some(When::Start), &parsed.pre),
            (
                "on-project-first-start",
                Some(When::Create),
                &parsed.on_project_first_start,
            ),
            ("on-project-stop", Some(When::Stop), &parsed.on_project_stop),
            ("on-project-restart", None, &parsed.on_project_restart),
            ("on-project-exit", None, &parsed.on_project_exit),
            ("post", None, &parsed.post),
        ] {
            let Some(commands) = commands else { continue };
            if commands.items().iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            match when {
                Some(when) => {
                    let mut unit = Unit::new(file, script(&commands.items()));
                    unit.cwd = root.clone();
                    unit.when = Some(when);
                    let (name, _) = names.claim(hook);
                    draft.tasks.push((name, unit));
                }
                None => draft.note(
                    file,
                    format!(
                        "{} has no devlanes lifecycle hook; not migrated",
                        hook.replace('-', "_")
                    ),
                ),
            }
        }
        for (index, entry) in parsed.windows.iter().enumerate() {
            let Some((window_name, body)) = entry.iter().next() else {
                continue;
            };
            let window: Window = match body {
                Value::Object(_) => typed(file, body.clone())?,
                single => Window {
                    panes: vec![single.clone()],
                    ..Window::default()
                },
            };
            let window_root = match &window.root {
                Some(raw) => {
                    let mut holder = Unit::new(file, Command::Shell(String::new()));
                    let dir = cx.cwd(&mut holder, "", raw);
                    for note in holder.notes {
                        draft.note(file, format!("window {window_name}: {note}"));
                    }
                    dir
                }
                None => root.clone(),
            };
            let mut window_before = before.clone();
            if let Some(pre) = &window.pre {
                window_before.extend(pre.items());
            }
            let mut other = window.other.clone();
            for key in ["layout", "synchronize", "focused_pane"] {
                other.shift_remove(key);
            }
            for key in other.keys() {
                draft.note(file, format!("windows[{index}].{key} is not migrated"));
            }
            let panes: Vec<(Option<String>, Vec<String>)> =
                window.panes.iter().map(pane_commands).collect();
            let runnable: Vec<usize> = (0..panes.len())
                .filter(|i| runs_program(&panes[*i].1))
                .collect();
            for (pane_index, (pane_name, commands)) in panes.iter().enumerate() {
                if !runnable.contains(&pane_index) {
                    if let Some(last) = commands.last() {
                        draft.note(
                            file,
                            format!(
                                "window {window_name}: pane {last:?} needs a terminal; not migrated"
                            ),
                        );
                    }
                    continue;
                }
                let raw = match (runnable.len(), pane_name) {
                    (1, _) => window_name.clone(),
                    (_, Some(pane)) => format!("{window_name}-{pane}"),
                    (_, None) => format!("{window_name}-{}", pane_index + 1),
                };
                let mut all = window_before.clone();
                all.extend(commands.iter().cloned());
                let mut unit = Unit::new(file, script(&all));
                unit.cwd = window_root.clone();
                let (name, renamed) = names.claim(&raw);
                if let Some(renamed) = renamed {
                    unit.note(renamed);
                }
                draft.services.push((name, unit));
            }
        }
        Ok(draft)
    }
}

/// The session root as a project directory, with a note when it is elsewhere.
fn session_root(
    cx: &Context<'_>,
    draft: &mut Draft,
    file: &str,
    raw: Option<&str>,
) -> Option<String> {
    let raw = raw?;
    let mut holder = Unit::new(file, Command::Shell(String::new()));
    let dir = cx.cwd(&mut holder, "", raw);
    for note in holder.notes {
        draft.note(file, format!("root: {note}"));
    }
    dir
}

/// A pane's optional title and its commands.
pub(super) fn pane_commands(pane: &Value) -> (Option<String>, Vec<String>) {
    match pane {
        Value::String(command) => (None, vec![command.clone()]),
        Value::Array(items) => (None, items.iter().filter_map(text).collect()),
        Value::Object(map) => match map.iter().next() {
            Some((title, value)) => (Some(title.clone()), pane_commands(value).1),
            None => (None, Vec::new()),
        },
        _ => (None, Vec::new()),
    }
}

/// A scalar as text.
fn text(value: &Value) -> Option<String> {
    crate::draft::scalar_text(value)
}

/// Whether the last command starts a program that can run without a person: not an editor,
/// a shell, a pager, or just `cd`.
pub(super) fn runs_program(commands: &[String]) -> bool {
    let Some(last) = commands.iter().rev().find(|c| !c.trim().is_empty()) else {
        return false;
    };
    let first = last
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default();
    !INTERACTIVE.contains(&first) && first != "cd"
}

/// Commands typed one after another into a pane, as one shell script. `source` is a bash
/// and zsh builtin; `.` is its `sh` spelling.
pub(super) fn script(commands: &[String]) -> Command {
    let lines: Vec<String> = commands
        .iter()
        .map(|c| c.trim_end())
        .filter(|c| !c.is_empty())
        .map(|c| match c.strip_prefix("source ") {
            Some(rest) => format!(". {rest}"),
            None => c.to_owned(),
        })
        .collect();
    Command::shell(&lines.join("\n"))
}
