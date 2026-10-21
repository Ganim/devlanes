//! mise tasks: `[tasks]` in the project's mise config files and executable file tasks in
//! the task directories. Every one becomes an on-demand devlanes task: mise does not say
//! which tasks keep running.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use super::{Context, Source, env_entries, typed, unknown_fields};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit},
    words,
};

/// The mise source.
pub(crate) struct Mise;

/// Project config files in mise's order; later files override earlier ones.
const FILES: [&str; 6] = [
    ".config/mise/config.toml",
    ".config/mise.toml",
    "mise/config.toml",
    ".mise/config.toml",
    "mise.toml",
    ".mise.toml",
];

/// Personal overrides, usually not committed.
const LOCAL_FILES: [&str; 6] = [
    ".config/mise/config.local.toml",
    ".config/mise.local.toml",
    "mise/config.local.toml",
    ".mise/config.local.toml",
    "mise.local.toml",
    ".mise.local.toml",
];

/// Directories mise reads file tasks from.
const TASK_DIRS: [&str; 5] = [
    "mise-tasks",
    ".mise-tasks",
    "mise/tasks",
    ".mise/tasks",
    ".config/mise/tasks",
];

/// Script extensions mise leaves out of a file task's name; others stay in it.
const SCRIPT_EXTENSIONS: [&str; 12] = [
    "sh", "bash", "zsh", "fish", "py", "js", "mjs", "cjs", "ts", "rb", "pl", "ps1",
];

/// The parts of a config file this source reads.
#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    tasks: IndexMap<String, Value>,
    env: Option<Value>,
}

/// A task in its table form.
#[derive(Debug, Clone, Default, Deserialize)]
struct Task {
    run: Option<Run>,
    file: Option<String>,
    dir: Option<String>,
    #[serde(default)]
    env: IndexMap<String, Value>,
    depends: Option<Value>,
    description: Option<String>,
    confirm: Option<String>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `run`: one command or several, run in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Run {
    One(String),
    Many(Vec<RunItem>),
}

/// One `run` entry: a command or other tasks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RunItem {
    Text(String),
    Task { task: String },
    Tasks { tasks: Vec<String> },
}

/// Table keys mise uses for its own bookkeeping, with why they do not carry over.
const KNOWN: [(&str, &str); 9] = [
    ("alias", "devlanes runs a task by its name"),
    ("hide", "devlanes lists every task"),
    ("sources", "devlanes runs the task every time"),
    ("outputs", "devlanes runs the task every time"),
    ("shell", "devlanes runs run text with sh"),
    ("usage", "devlanes passes no task arguments"),
    ("run_windows", "devlanes runs on macOS and Linux"),
    ("quiet", "devlanes keeps the output"),
    ("tools", "devlanes reads tools from the mise config itself"),
];

impl Source for Mise {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        let mut files: Vec<String> = FILES
            .into_iter()
            .filter(|name| has_tasks(dir, name))
            .map(str::to_owned)
            .collect();
        files.extend(
            TASK_DIRS
                .into_iter()
                .filter(|task_dir| !file_tasks(dir, task_dir).is_empty())
                .map(|task_dir| format!("{task_dir}/")),
        );
        (!files.is_empty()).then_some(files)
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::Mise);
        let mut tasks: IndexMap<String, (String, Task)> = IndexMap::new();
        for file in files {
            if let Some(task_dir) = file.strip_suffix('/') {
                for (name, path) in file_tasks(cx.dir, task_dir) {
                    let task = Task {
                        file: Some(path),
                        ..Task::default()
                    };
                    tasks.insert(name, (file.clone(), task));
                }
                continue;
            }
            let value: Value =
                toml::from_str(&cx.read(file)?).map_err(|e| MigrateError::Parse {
                    file: file.clone(),
                    message: e.to_string(),
                })?;
            let parsed: File = typed(file, value)?;
            if parsed.env.is_some() {
                draft.note(
                    file,
                    "[env] is not copied: devlanes adds the mise environment to every unit itself",
                );
            }
            for (name, value) in parsed.tasks {
                let task = match value {
                    Value::String(line) => Task {
                        run: Some(Run::One(line)),
                        ..Task::default()
                    },
                    Value::Array(_) => Task {
                        run: Some(typed(file, value)?),
                        ..Task::default()
                    },
                    table => typed(file, table)?,
                };
                tasks.insert(name, (file.clone(), task));
            }
        }
        for local in LOCAL_FILES {
            if cx.dir.join(local).is_file() {
                draft.note(local, "personal overrides are not migrated");
            }
        }
        let mut names = Names::default();
        let claimed: IndexMap<String, (String, Option<String>)> = tasks
            .keys()
            .map(|raw| (raw.clone(), names.claim(raw)))
            .collect();
        for (raw, (file, task)) in tasks {
            let (name, renamed) = claimed[&raw].clone();
            let depends = depends(task.depends.as_ref());
            let dep_names: Vec<String> = depends
                .iter()
                .map(|dep| {
                    claimed
                        .get(dep)
                        .map_or_else(|| dep.clone(), |(n, _)| n.clone())
                })
                .collect();
            let run = match (&task.run, &task.file) {
                (Some(run), _) => run_command(run),
                (None, Some(path)) => Some(Command::shell(&script_path(path))),
                (None, None) if !dep_names.is_empty() => Some(Command::Steps(dep_names.clone())),
                (None, None) => None,
            };
            let Some(run) = run else {
                draft.note(&file, format!("task {raw} runs nothing; not migrated"));
                continue;
            };
            let mut unit = Unit::new(&file, run);
            if let Some(renamed) = renamed {
                unit.note(renamed);
            }
            unit.description = task.description.clone();
            if !dep_names.is_empty() && !matches!(unit.run, Command::Steps(_)) {
                unit.note(format!(
                    "depends ({}) is not migrated: run those tasks first",
                    dep_names.join(", ")
                ));
            }
            if let Some(dir) = &task.dir {
                match dir.strip_prefix("{{config_root}}") {
                    Some(rest) => {
                        let rest = rest.trim_start_matches('/');
                        unit.cwd = cx.cwd(&mut unit, "", if rest.is_empty() { "." } else { rest });
                    }
                    None if dir.contains("{{") => {
                        unit.note(format!("dir {dir} uses a mise template; not migrated"));
                    }
                    None => unit.cwd = cx.cwd(&mut unit, "", dir),
                }
            }
            env_entries(&mut unit, &task.env);
            if let Some(confirm) = &task.confirm {
                unit.confirm = Some(crate::draft::escape(confirm));
            }
            if let Command::Shell(text) = unit.run.clone() {
                if text.contains("{{") {
                    unit.note("run uses mise templates ({{...}}); fill them in");
                }
                if text.starts_with("#!") {
                    unit.note("run is a shebang script, but devlanes runs run text with sh; move it to a file");
                }
            }
            let mut other = task.other.clone();
            for (key, why) in KNOWN {
                if other.shift_remove(key).is_some() {
                    unit.note(format!("{key} is not migrated: {why}"));
                }
            }
            unknown_fields(&mut unit.notes, &format!("tasks.{raw}"), &other);
            draft.tasks.push((name, unit));
        }
        Ok(draft)
    }
}

/// Whether a config file exists and defines at least one task.
fn has_tasks(dir: &Path, name: &str) -> bool {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .and_then(|text| toml::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("tasks").cloned())
        .is_some_and(|tasks| tasks.as_object().is_some_and(|map| !map.is_empty()))
}

/// Executable files under a task directory, named as mise names them (`db/seed` is
/// `db:seed`), with their path relative to the project.
fn file_tasks(dir: &Path, task_dir: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect(dir, &PathBuf::from(task_dir), &mut out);
    out.sort();
    out.into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(task_dir)
                .unwrap_or(&path)
                .trim_start_matches('/')
                .to_owned();
            let stem = relative
                .rsplit_once('.')
                .filter(|(_, ext)| SCRIPT_EXTENSIONS.contains(ext))
                .map_or(relative.as_str(), |(stem, _)| stem);
            let stem = stem.strip_suffix("/_default").unwrap_or(stem);
            (stem.replace('/', ":"), path)
        })
        .collect()
}

/// Walks a task directory for executable files.
fn collect(dir: &Path, relative: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir.join(relative)) else {
        return;
    };
    for entry in entries.flatten() {
        let child = relative.join(entry.file_name());
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            collect(dir, &child, out);
        } else if meta.is_file() && executable(&meta) {
            out.push(child.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// mise runs only executable files as tasks.
#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// Without permission bits every file counts.
#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    true
}

/// A script path runnable from the project root.
fn script_path(path: &str) -> String {
    if path.starts_with('/') || path.starts_with("./") {
        words::quote(path)
    } else {
        format!("./{}", words::quote(path))
    }
}

/// `depends` as task names: a string, a list of strings, or objects with `task`.
fn depends(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(one)) => one.split_whitespace().map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(name) => Some(name.clone()),
                Value::Object(map) => map.get("task").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `run` as shell text. mise runs with `sh -o errexit`, so several entries or lines stop at
/// the first failure; `set -e` keeps that. Task references run through `mise run`.
fn run_command(run: &Run) -> Option<Command> {
    let lines: Vec<String> = match run {
        Run::One(line) => vec![line.clone()],
        Run::Many(items) => items
            .iter()
            .map(|item| match item {
                RunItem::Text(text) => text.clone(),
                RunItem::Task { task } => format!("mise run {}", words::quote(task)),
                RunItem::Tasks { tasks } => format!("mise run {}", words::join(tasks)),
            })
            .collect(),
    };
    let text = lines.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    let script = if text.trim().contains('\n') && !text.trim_start().starts_with("#!") {
        format!("set -e\n{}", text.trim())
    } else {
        text.trim().to_owned()
    };
    Some(Command::shell(&script))
}
