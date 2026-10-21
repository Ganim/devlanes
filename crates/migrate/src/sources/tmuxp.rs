//! tmuxp: the project-local `.tmuxp.yaml` (or `.yml`, `.json`). Each pane that runs a
//! program becomes a service; editors and plain shells are left out.

use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Context, Source, existing,
    tmuxinator::{runs_program, script},
    typed, unknown_top,
};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit, When, escape, scalar_text},
};

/// The tmuxp source.
pub(crate) struct Tmuxp;

/// Workspace files `tmuxp load .` finds in a project root.
const FILES: [&str; 3] = [".tmuxp.yaml", ".tmuxp.yml", ".tmuxp.json"];

/// Settings that only shape the tmux session.
const SESSION_KEYS: [&str; 5] = [
    "session_name",
    "global_options",
    "options",
    "suppress_history",
    "plugins",
];

/// A whole workspace file.
#[derive(Debug, Deserialize)]
struct File {
    start_directory: Option<String>,
    before_script: Option<String>,
    shell_command_before: Option<Commands>,
    #[serde(default)]
    environment: IndexMap<String, Value>,
    #[serde(default)]
    windows: Vec<Window>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// One window.
#[derive(Debug, Deserialize)]
struct Window {
    window_name: Option<String>,
    start_directory: Option<String>,
    shell_command_before: Option<Commands>,
    #[serde(default)]
    environment: IndexMap<String, Value>,
    #[serde(default)]
    panes: Vec<Value>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// One pane in its mapping form.
#[derive(Debug, Default, Deserialize)]
struct Pane {
    shell_command: Option<Commands>,
    start_directory: Option<String>,
    #[serde(default)]
    environment: IndexMap<String, Value>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `shell_command`: a string, or a list of strings and `{cmd: ...}` objects.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Commands {
    One(String),
    Many(Vec<Item>),
}

/// One item of a command list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Item {
    Text(String),
    Object { cmd: String },
}

impl Commands {
    /// The command lines in order.
    fn lines(&self) -> Vec<String> {
        match self {
            Self::One(line) => vec![line.clone()],
            Self::Many(items) => items
                .iter()
                .map(|item| match item {
                    Item::Text(text) | Item::Object { cmd: text } => text.clone(),
                })
                .collect(),
        }
    }
}

impl Source for Tmuxp {
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
        let mut draft = Draft::new(SourceKind::Tmuxp);
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
        let root = directory(cx, &mut draft, file, "", parsed.start_directory.as_deref());
        for (key, value) in &parsed.environment {
            match scalar_text(value) {
                Some(text) => draft.global_env.push((key.clone(), escape(&text))),
                None => draft.note(file, format!("environment {key} is not a string; skipped")),
            }
        }
        if let Some(before) = &parsed.before_script {
            let mut unit = Unit::new(file, Command::shell(before));
            unit.cwd = root.clone();
            unit.when = Some(When::Start);
            let (name, _) = names.claim("before-script");
            draft.tasks.push((name, unit));
        }
        let session_before = parsed
            .shell_command_before
            .as_ref()
            .map(Commands::lines)
            .unwrap_or_default();
        for (index, window) in parsed.windows.iter().enumerate() {
            let window_name = window
                .window_name
                .clone()
                .unwrap_or_else(|| format!("window-{}", index + 1));
            let base = root.clone().unwrap_or_default();
            let window_dir = match &window.start_directory {
                Some(raw) => directory(cx, &mut draft, file, &base, Some(raw)),
                None => root.clone(),
            };
            let mut before = session_before.clone();
            if let Some(more) = &window.shell_command_before {
                before.extend(more.lines());
            }
            let mut other = window.other.clone();
            for key in [
                "layout",
                "options",
                "options_after",
                "focus",
                "window_shell",
                "window_index",
            ] {
                other.shift_remove(key);
            }
            for key in other.keys() {
                draft.note(file, format!("windows[{index}].{key} is not migrated"));
            }
            let panes: Vec<Pane> = window
                .panes
                .iter()
                .map(|pane| match pane {
                    Value::String(line) => Ok(Pane {
                        shell_command: Some(Commands::One(line.clone())),
                        ..Pane::default()
                    }),
                    Value::Object(_) => typed(file, pane.clone()),
                    _ => Ok(Pane::default()),
                })
                .collect::<Result<_, _>>()?;
            let commands: Vec<Vec<String>> = panes
                .iter()
                .map(|pane| {
                    pane.shell_command
                        .as_ref()
                        .map(Commands::lines)
                        .unwrap_or_default()
                })
                .collect();
            let runnable: Vec<usize> = (0..panes.len())
                .filter(|i| runs_program(&commands[*i]))
                .collect();
            for (pane_index, pane) in panes.iter().enumerate() {
                if !runnable.contains(&pane_index) {
                    if let Some(last) = commands[pane_index].last() {
                        draft.note(
                            file,
                            format!(
                                "window {window_name}: pane {last:?} needs a terminal; not migrated"
                            ),
                        );
                    }
                    continue;
                }
                let raw = if runnable.len() == 1 {
                    window_name.clone()
                } else {
                    format!("{window_name}-{}", pane_index + 1)
                };
                let mut all = before.clone();
                all.extend(commands[pane_index].iter().cloned());
                let mut unit = Unit::new(file, script(&all));
                unit.cwd = match &pane.start_directory {
                    Some(raw) => {
                        let base = window_dir.clone().unwrap_or_default();
                        cx.cwd(&mut unit, &base, raw)
                    }
                    None => window_dir.clone(),
                };
                for (key, value) in window.environment.iter().chain(&pane.environment) {
                    if let Some(text) = scalar_text(value) {
                        unit.set_env(key, escape(&text));
                    }
                }
                let mut pane_other = pane.other.clone();
                for key in [
                    "focus",
                    "sleep_before",
                    "sleep_after",
                    "enter",
                    "suppress_history",
                ] {
                    pane_other.shift_remove(key);
                }
                super::unknown_fields(&mut unit.notes, "pane", &pane_other);
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

/// A start directory relative to the project, with a note when it is elsewhere.
fn directory(
    cx: &Context<'_>,
    draft: &mut Draft,
    file: &str,
    base: &str,
    raw: Option<&str>,
) -> Option<String> {
    let raw = raw?;
    let mut holder = Unit::new(file, Command::Shell(String::new()));
    let dir = cx.cwd(&mut holder, base, raw);
    for note in holder.notes {
        draft.note(file, format!("start_directory: {note}"));
    }
    dir
}
