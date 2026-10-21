//! dekit: `dekit.yaml` at the project root, plus the fragments its `load` names.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Context, Source, autostart_closure, env_entries, existing, http_check, typed, unknown_fields,
};
use crate::{
    MigrateError, SourceKind,
    draft::{Check, Command, Draft, Names, Ready, Unit, When, duration_text, signal_name},
    words,
};

/// The dekit source.
pub(crate) struct Dekit;

/// dekit reads only this name; `load` adds fragments.
const FILES: [&str; 1] = ["dekit.yaml"];

/// Directories never searched for fragments.
const SKIPPED_DIRS: [&str; 3] = [".git", "node_modules", "target"];

/// A `dekit.yaml` or a fragment.
#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    tasks: IndexMap<String, Value>,
    defaults: Option<Task>,
    #[serde(default)]
    load: Vec<Load>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// A `load` entry.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Load {
    Glob(String),
    Mounted { file: String, at: Option<String> },
}

/// One task, or `defaults` (which takes the settings only).
#[derive(Debug, Clone, Default, Deserialize)]
struct Task {
    label: Option<String>,
    cmd: Option<Cmd>,
    script: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    ready: Option<ReadyCheck>,
    cwd: Option<String>,
    env: Option<IndexMap<String, Value>>,
    add_path: Option<Vec<String>>,
    autostart: Option<bool>,
    autorestart: Option<String>,
    stop: Option<Stop>,
    log: Option<Value>,
    scrollback_len: Option<Value>,
    mouse_scroll_speed: Option<Value>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `cmd`: an argv, or a line split like `sh` would without running a shell.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Cmd {
    Line(String),
    Argv(Vec<String>),
}

impl Cmd {
    /// As devlanes `run` text. dekit refuses shell syntax in a line, so `sh` reads it the same.
    fn command(&self) -> Command {
        match self {
            Self::Line(line) => Command::shell(line),
            Self::Argv(args) => Command::argv(args),
        }
    }
}

/// `ready:`.
#[derive(Debug, Clone, Deserialize)]
struct ReadyCheck {
    log: Option<String>,
    tcp: Option<Value>,
    http: Option<String>,
    cmd: Option<Cmd>,
    file: Option<String>,
    interval: Option<String>,
    timeout: Option<String>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `stop:` as a signal name or an object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Stop {
    Signal(String),
    Object(StopObject),
}

/// The object form of `stop:`.
#[derive(Debug, Clone, Default, Deserialize)]
struct StopObject {
    signal: Option<String>,
    group: Option<bool>,
    keys: Option<Vec<String>>,
    cmd: Option<Cmd>,
    timeout: Option<String>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

impl Stop {
    /// The object form, so `defaults` and a task can merge part by part.
    fn object(&self) -> StopObject {
        match self {
            Self::Signal(signal) => StopObject {
                signal: Some(signal.clone()),
                ..StopObject::default()
            },
            Self::Object(object) => object.clone(),
        }
    }
}

/// A task with the path dekit gives it and the file it is written in.
struct Found {
    path: String,
    file: String,
    /// Directory of the declaring file, relative to the project.
    base: String,
    task: Task,
}

impl Source for Dekit {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        let files = existing(dir, &FILES);
        (!files.is_empty()).then_some(files)
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::Dekit);
        let Some(root) = files.first() else {
            return Ok(draft);
        };
        let mut found = Vec::new();
        let mut defaults = Task::default();
        let mut stack = vec![(root.clone(), String::new())];
        let mut seen = BTreeSet::new();
        while let Some((file, mount)) = stack.pop() {
            if !seen.insert(file.clone()) {
                continue;
            }
            let parsed: File = cx.yaml(&file)?;
            let is_root = file == *root;
            if is_root {
                defaults = parsed.defaults.clone().unwrap_or_default();
                for key in parsed.other.keys() {
                    draft.note(&file, root_key_note(key));
                }
            } else {
                draft.note(&file, format!("read as a fragment of {root}"));
            }
            let base = parent(&file);
            for (name, value) in parsed.tasks {
                let task: Task = typed(&file, value)?;
                found.push(Found {
                    path: join(&mount, &name),
                    file: file.clone(),
                    base: base.clone(),
                    task: Task {
                        deps: task
                            .deps
                            .iter()
                            .map(|dep| match dep.strip_prefix('/') {
                                Some(top) => top.to_owned(),
                                None => join(&mount, dep),
                            })
                            .collect(),
                        ..task
                    },
                });
            }
            for entry in parsed.load.iter().rev() {
                let (pattern, at) = match entry {
                    Load::Glob(pattern) => (pattern.clone(), String::new()),
                    Load::Mounted { file, at } => (file.clone(), at.clone().unwrap_or_default()),
                };
                let matches = glob(cx.dir, &base, &pattern);
                if matches.is_empty() {
                    draft.note(&file, format!("load pattern {pattern} matches no file"));
                }
                for child in matches.into_iter().rev() {
                    stack.push((child, join(&mount, &at)));
                }
            }
        }
        convert_tasks(cx, &mut draft, found, &defaults);
        Ok(draft)
    }
}

/// Why a root key is not migrated.
fn root_key_note(key: &str) -> String {
    match key {
        "on_init" | "on_idle" => {
            format!("{key} is not migrated: devlanes runs lifecycle tasks with when:")
        }
        "tui" | "keymap" => format!("{key} is not migrated: it belongs in dekit's user config"),
        "kernel" | "log" => format!("{key} is a dekit runner setting; not migrated"),
        other => format!("{other} is not migrated"),
    }
}

/// Converts every task once all files are read, so dependencies across files resolve.
fn convert_tasks(cx: &Context<'_>, draft: &mut Draft, found: Vec<Found>, defaults: &Task) {
    let jobs: BTreeSet<String> = found
        .iter()
        .filter(|f| f.task.kind.as_deref() == Some("job"))
        .map(|f| f.path.clone())
        .collect();
    let deps: IndexMap<String, Vec<String>> = found
        .iter()
        .map(|f| (f.path.clone(), f.task.deps.clone()))
        .collect();
    let explicit: BTreeSet<String> = found
        .iter()
        .filter(|f| f.task.autostart.or(defaults.autostart).unwrap_or(false))
        .map(|f| f.path.clone())
        .collect();
    let mut started = explicit.clone();
    autostart_closure(&mut started, &deps);
    let mut names = Names::default();
    let claimed: IndexMap<String, (String, Option<String>)> = found
        .iter()
        .map(|f| (f.path.clone(), names.claim(&f.path)))
        .collect();
    for item in found {
        let Found {
            path,
            file,
            base,
            task,
        } = item;
        let (name, renamed) = claimed[&path].clone();
        let job = jobs.contains(&path);
        // A stop policy inherited from `defaults` means nothing for a job.
        let own_stop = task.stop.is_some();
        let task = overlay(defaults, task);
        let run = match (&task.cmd, &task.script) {
            (Some(cmd), _) => cmd.command(),
            (None, Some(script)) => {
                draft.note(
                    &file,
                    format!(
                        "task {path} runs {script} on dekit's JavaScript runtime; not migrated"
                    ),
                );
                continue;
            }
            (None, None) => {
                draft.note(&file, format!("task {path} has no cmd; not migrated"));
                continue;
            }
        };
        let mut unit = Unit::new(&file, run);
        if let Some(renamed) = renamed {
            unit.note(renamed);
        }
        unit.description = task.label.clone();
        if let Some(cwd) = &task.cwd {
            unit.cwd = cx.cwd(&mut unit, &base, cwd);
        }
        if let Some(env) = &task.env {
            env_entries(&mut unit, env);
        }
        if let Some(add_path) = &task.add_path {
            super::mprocs::add_to_path(&mut unit, add_path);
        }
        unit.tags = task
            .tags
            .iter()
            .filter(|tag| !matches!(tag.as_str(), "autostart" | "dynamic"))
            .cloned()
            .collect();
        if let Some(restart) = task.autorestart.as_deref().filter(|r| *r != "never") {
            unit.note(format!(
                "autorestart: {restart} is not migrated: devlanes does not restart processes that exit"
            ));
        }
        if let Some(stop) = &task.stop
            && (own_stop || !job)
        {
            convert_stop(&mut unit, &stop.object());
        }
        for (key, set) in [
            ("log", task.log.is_some()),
            ("scrollback_len", task.scrollback_len.is_some()),
            ("mouse_scroll_speed", task.mouse_scroll_speed.is_some()),
        ] {
            if set {
                unit.note(format!("{key} is a dekit terminal setting; not migrated"));
            }
        }
        unknown_fields(&mut unit.notes, &format!("tasks.{path}"), &task.other);
        let needed_by: Vec<&String> = deps
            .iter()
            .filter(|(user, list)| list.contains(&path) && started.contains(*user))
            .map(|(user, _)| user)
            .collect();
        let service_deps: Vec<String> = task
            .deps
            .iter()
            .filter(|dep| !jobs.contains(*dep))
            .map(|dep| {
                claimed
                    .get(dep)
                    .map_or_else(|| dep.clone(), |(n, _)| n.clone())
            })
            .collect();
        if job {
            if started.contains(&path) {
                unit.when = Some(When::Start);
            }
            if !service_deps.is_empty() {
                unit.after_services = unit.when.is_some();
                unit.note(format!(
                    "needs {}; {}",
                    service_deps.join(", "),
                    if unit.after_services {
                        "it runs after the lane's services are ready"
                    } else {
                        "start them before running it"
                    }
                ));
                let waiting: Vec<&str> = deps
                    .iter()
                    .filter(|(user, list)| list.contains(&path) && !jobs.contains(*user))
                    .map(|(user, _)| user.as_str())
                    .collect();
                if unit.after_services && !waiting.is_empty() {
                    unit.note(order_note(&waiting, &service_deps));
                }
            }
            if task.ready.is_some() {
                unit.note("ready is not migrated: a task has no readiness check");
            }
            draft.tasks.push((name, unit));
        } else {
            if !started.contains(&path) {
                unit.autostart = Some(false);
            } else if !explicit.contains(&path) && !needed_by.is_empty() {
                let users: Vec<&str> = needed_by.iter().map(|s| s.as_str()).collect();
                unit.note(format!(
                    "starts with the lane: {} depend on it",
                    users.join(", ")
                ));
            }
            if !service_deps.is_empty() {
                unit.note(format!(
                    "deps ({}) are not migrated: devlanes starts services together",
                    service_deps.join(", ")
                ));
            }
            if let Some(ready) = &task.ready {
                unit.ready = convert_ready(&mut unit, ready);
            }
            draft.services.push((name, unit));
        }
    }
}

/// Services waited for a one-shot task that itself needs services; devlanes can run such a
/// task only after all services.
pub(super) fn order_note(waiting: &[&str], needs: &[String]) -> String {
    format!(
        "{} waited for it, but start before it now; running {} as infra (Compose) lets it run before services",
        waiting.join(", "),
        needs.join(", ")
    )
}

/// `defaults` under a task: the task's own value wins; `stop` merges part by part.
fn overlay(defaults: &Task, task: Task) -> Task {
    let stop = match (&defaults.stop, &task.stop) {
        (Some(base), Some(over)) => {
            let (base, over) = (base.object(), over.object());
            let how_set = over.signal.is_some() || over.keys.is_some() || over.cmd.is_some();
            Some(Stop::Object(StopObject {
                signal: if how_set { over.signal } else { base.signal },
                group: if how_set { over.group } else { base.group },
                keys: if how_set { over.keys } else { base.keys },
                cmd: if how_set { over.cmd } else { base.cmd },
                timeout: over.timeout.or(base.timeout),
                other: over.other,
            }))
        }
        (base, over) => over.clone().or_else(|| base.clone()),
    };
    Task {
        cwd: task.cwd.or_else(|| defaults.cwd.clone()),
        env: task.env.or_else(|| defaults.env.clone()),
        add_path: task.add_path.or_else(|| defaults.add_path.clone()),
        autostart: task.autostart.or(defaults.autostart),
        autorestart: task.autorestart.or_else(|| defaults.autorestart.clone()),
        log: task.log.or_else(|| defaults.log.clone()),
        stop,
        ..task
    }
}

/// `stop:` in devlanes terms.
fn convert_stop(unit: &mut Unit, stop: &StopObject) {
    if let Some(signal) = &stop.signal {
        match signal_name(signal) {
            Some("SIGTERM") => {}
            Some(name) => unit.stop_signal = Some(name.into()),
            None => unit.note(format!("stop signal {signal} is not migrated")),
        }
    }
    if stop.group == Some(false) {
        unit.note("stop group: false is not migrated: devlanes stops the whole process tree");
    }
    if let Some(keys) = &stop.keys {
        super::mprocs::stop_keys(unit, keys);
    }
    if stop.cmd.is_some() {
        unit.note("stop cmd is not migrated: devlanes stops a service with a signal");
    }
    if let Some(timeout) = &stop.timeout {
        match duration_text(timeout) {
            Some(timeout) => unit.stop_timeout = Some(timeout),
            None => unit.note(format!("stop timeout {timeout} is not migrated")),
        }
    }
    unknown_fields(&mut unit.notes, "stop", &stop.other);
}

/// `ready:` in devlanes terms.
fn convert_ready(unit: &mut Unit, ready: &ReadyCheck) -> Option<Ready> {
    if ready.interval.is_some() {
        unit.note("ready interval is not migrated: devlanes picks its own retry interval");
    }
    unknown_fields(&mut unit.notes, "ready", &ready.other);
    let check = if let Some(text) = &ready.log {
        // dekit matches a substring; devlanes reads a regular expression.
        Check::Log(regex::escape(text))
    } else if let Some(tcp) = &ready.tcp {
        let port = match tcp {
            Value::Number(port) => port.as_u64().and_then(|p| u16::try_from(p).ok()),
            Value::String(text) => match text.rsplit_once(':') {
                Some(("localhost" | "127.0.0.1", port)) => port.parse().ok(),
                Some(_) => None,
                None => text.parse().ok(),
            },
            _ => None,
        };
        match port {
            Some(port) => Check::Probe { port, http: None },
            None => {
                unit.note(format!(
                    "ready tcp {tcp} is not migrated: it is not a local port"
                ));
                return None;
            }
        }
    } else if let Some(url) = &ready.http {
        http_check(unit, url, None)?
    } else if let Some(cmd) = &ready.cmd {
        Check::Cmd(cmd.command())
    } else if let Some(file) = &ready.file {
        Check::Cmd(Command::Shell(format!("test -e {}", words::quote(file))))
    } else {
        return None;
    };
    let timeout = ready.timeout.as_deref().and_then(|timeout| {
        let parsed = duration_text(timeout);
        if parsed.is_none() {
            unit.note(format!("ready timeout {timeout} is not migrated"));
        }
        parsed
    });
    Some(Ready { check, timeout })
}

/// `a/b` from a mount and a name; either may be empty.
fn join(mount: &str, name: &str) -> String {
    match (mount.is_empty(), name.is_empty()) {
        (true, _) => name.to_owned(),
        (_, true) => mount.to_owned(),
        _ => format!("{mount}/{name}"),
    }
}

/// The directory of a relative file, `""` at the top.
fn parent(file: &str) -> String {
    Path::new(file)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// Files matching a `load` pattern relative to `base`, sorted, as paths relative to `dir`.
fn glob(dir: &Path, base: &str, pattern: &str) -> Vec<String> {
    let full = if base.is_empty() {
        pattern.to_owned()
    } else {
        format!("{base}/{pattern}")
    };
    let full = full.trim_start_matches("./").to_owned();
    if !full.contains(['*', '?', '[']) {
        return if dir.join(&full).is_file() {
            vec![full]
        } else {
            Vec::new()
        };
    }
    let Ok(matcher) = globset::GlobBuilder::new(&full)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(dir, &PathBuf::new(), &mut |relative| {
        let text = relative.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&text) && text != "dekit.yaml" {
            out.push(text);
        }
    });
    out.sort();
    out
}

/// Visits every file under `dir`, skipping dependency and VCS directories.
fn walk(dir: &Path, relative: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir.join(relative)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let child = relative.join(&name);
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                if !SKIPPED_DIRS.iter().any(|skip| name == *skip) {
                    walk(dir, &child, visit);
                }
            }
            Ok(kind) if kind.is_file() => visit(&child),
            _ => {}
        }
    }
}
