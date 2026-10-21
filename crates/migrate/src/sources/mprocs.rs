//! mprocs: `mprocs.yaml`, else `mprocs.json`, in the directory mprocs starts in.

use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use super::{Context, OneOrMany, Source, env_entries, existing, typed, unknown_fields};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit, escape, signal_name},
};

/// The mprocs source.
pub(crate) struct Mprocs;

/// mprocs reads the first of these from the current directory.
const FILES: [&str; 2] = ["mprocs.yaml", "mprocs.json"];

/// Top-level keys that only shape mprocs' own terminal UI or remote control.
const UI_KEYS: [&str; 12] = [
    "hide_keymap_window",
    "mouse_scroll_speed",
    "scrollback",
    "proc_list_width",
    "proc_list_title",
    "proc_log",
    "keymap_procs",
    "keymap_term",
    "keymap_copy",
    "server",
    "on_init",
    "on_all_finished",
];

/// A whole `mprocs.yaml`.
#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    procs: IndexMap<String, Value>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// One process in its mapping form.
#[derive(Debug, Deserialize)]
struct Proc {
    shell: Option<String>,
    cmd: Option<Vec<String>>,
    cwd: Option<String>,
    #[serde(default)]
    env: IndexMap<String, Value>,
    add_path: Option<OneOrMany>,
    autostart: Option<bool>,
    autorestart: Option<bool>,
    stop: Option<Stop>,
    #[serde(default)]
    deps: Vec<String>,
    log: Option<Value>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `stop:` forms.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Stop {
    Name(String),
    Keys {
        #[serde(rename = "send-keys")]
        send_keys: Vec<String>,
    },
    Cmd {
        cmd: String,
    },
}

impl Source for Mprocs {
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
        let mut draft = Draft::new(SourceKind::Mprocs);
        let Some(file) = files.first() else {
            return Ok(draft);
        };
        if file == FILES[0] && cx.dir.join(FILES[1]).is_file() {
            draft.note(
                file,
                "mprocs.json is ignored, as mprocs does when mprocs.yaml exists",
            );
        }
        let mut value = cx.yaml_value(file)?;
        let mut picked = Vec::new();
        resolve_select(&mut value, &cx.options.os, "", &mut picked);
        for path in picked {
            draft.note(
                file,
                format!("{path} uses $select; took the {} value", cx.options.os),
            );
        }
        let parsed: File = typed(file, value)?;
        for (key, _) in &parsed.other {
            let why = if UI_KEYS.contains(&key.as_str()) {
                "an mprocs UI setting; not migrated"
            } else {
                "not migrated"
            };
            draft.note(file, format!("{key} is {why}"));
        }
        let mut names = Names::default();
        for (raw, entry) in parsed.procs {
            let proc = match entry {
                Value::Null => continue,
                Value::String(shell) => Proc::shell(shell),
                Value::Array(_) => Proc::argv(typed(file, entry)?),
                other => typed(file, other)?,
            };
            let (name, renamed) = names.claim(&raw);
            let mut unit = convert_proc(cx, file, &raw, proc)?;
            if let Some(renamed) = renamed {
                unit.notes.insert(0, renamed);
            }
            draft.services.push((name, unit));
        }
        Ok(draft)
    }
}

impl Proc {
    /// The string shorthand: a shell command.
    fn shell(shell: String) -> Self {
        Self::empty(Some(shell), None)
    }

    /// The list shorthand: an argument vector.
    fn argv(cmd: Vec<String>) -> Self {
        Self::empty(None, Some(cmd))
    }

    /// A process with only its command set.
    fn empty(shell: Option<String>, cmd: Option<Vec<String>>) -> Self {
        Self {
            shell,
            cmd,
            cwd: None,
            env: IndexMap::new(),
            add_path: None,
            autostart: None,
            autorestart: None,
            stop: None,
            deps: Vec::new(),
            log: None,
            other: IndexMap::new(),
        }
    }
}

/// One process as a service.
fn convert_proc(cx: &Context<'_>, file: &str, raw: &str, proc: Proc) -> Result<Unit, MigrateError> {
    let run = match (&proc.shell, &proc.cmd) {
        (Some(shell), None) => Command::shell(shell),
        (None, Some(cmd)) if !cmd.is_empty() => Command::argv(cmd),
        _ => {
            return Err(MigrateError::Parse {
                file: file.to_owned(),
                message: format!("procs.{raw} needs exactly one of shell or cmd"),
            });
        }
    };
    let mut unit = Unit::new(file, run);
    if let Some(cwd) = &proc.cwd {
        let relative = cwd
            .strip_prefix("<CONFIG_DIR>")
            .map_or(cwd.as_str(), |rest| rest.trim_start_matches(['/', '\\']));
        let relative = if relative.is_empty() { "." } else { relative };
        unit.cwd = cx.cwd(&mut unit, "", relative);
    }
    env_entries(&mut unit, &proc.env);
    if let Some(add_path) = &proc.add_path {
        add_to_path(&mut unit, &add_path.items());
    }
    if proc.autostart == Some(false) {
        unit.autostart = Some(false);
    }
    if proc.autorestart == Some(true) {
        unit.note("autorestart is not migrated: devlanes does not restart processes that exit");
    }
    match &proc.stop {
        None => {}
        Some(Stop::Name(name)) => match name.as_str() {
            "SIGTERM" => {}
            "hard-kill" => unit.stop_signal = Some("SIGKILL".into()),
            other => match signal_name(other) {
                Some(signal) => unit.stop_signal = Some(signal.into()),
                None => unit.note(format!("stop {other} is not migrated")),
            },
        },
        Some(Stop::Keys { send_keys }) => stop_keys(&mut unit, send_keys),
        Some(Stop::Cmd { cmd }) => unit.note(format!(
            "stop command {cmd:?} is not migrated: devlanes stops a service with a signal"
        )),
    }
    if !proc.deps.is_empty() {
        unit.note(format!(
            "deps ({}) are not migrated: devlanes starts services together",
            proc.deps.join(", ")
        ));
    }
    if proc.log.is_some() {
        unit.note("log is not migrated: devlanes keeps every service's output");
    }
    unknown_fields(&mut unit.notes, &format!("procs.{raw}"), &proc.other);
    Ok(unit)
}

/// `send-keys: ["<C-c>"]` is what a terminal turns into SIGINT; other keys have no
/// devlanes equivalent.
pub(super) fn stop_keys(unit: &mut Unit, keys: &[String]) {
    if keys.len() == 1 && keys[0].eq_ignore_ascii_case("<C-c>") {
        unit.stop_signal = Some("SIGINT".into());
        unit.note("stop sends SIGINT instead of typing <C-c>");
    } else {
        unit.note(format!(
            "stop keys {} are not migrated: devlanes stops a service with a signal",
            keys.join(" ")
        ));
    }
}

/// `add_path` entries go in front of the outer `PATH`.
pub(super) fn add_to_path(unit: &mut Unit, entries: &[String]) {
    if entries.is_empty() {
        return;
    }
    let base = unit
        .env
        .iter()
        .find(|(key, _)| key == "PATH")
        .map_or_else(|| "${{ env.PATH }}".to_owned(), |(_, value)| value.clone());
    let front: Vec<String> = entries.iter().map(|entry| escape(entry)).collect();
    unit.set_env("PATH", format!("{}:{base}", front.join(":")));
    unit.note(
        "add_path became PATH; it replaces the PATH devlanes builds (including mise tools), so check it",
    );
}

/// Replaces each `{$select: os, <os>: value, $else: value}` with the value for `os`, recording
/// the paths it resolved.
fn resolve_select(value: &mut Value, os: &str, path: &str, picked: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.get("$select").and_then(Value::as_str) == Some("os") {
                let chosen = map.get(os).or_else(|| map.get("$else")).cloned();
                *value = chosen.unwrap_or(Value::Null);
                picked.push(if path.is_empty() {
                    "a value".into()
                } else {
                    path.to_owned()
                });
                resolve_select(value, os, path, picked);
                return;
            }
            for (key, child) in map.iter_mut() {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                resolve_select(child, os, &child_path, picked);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                resolve_select(child, os, &format!("{path}[{index}]"), picked);
            }
        }
        _ => {}
    }
}
