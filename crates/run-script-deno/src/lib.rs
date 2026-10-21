//! `run_script` support for Deno tasks: `deno task <name> <args>` in the
//! config file's directory.
//!
//! Arguments go right after the task name without `--`. Deno appends them to
//! the task command, each single-quoted (`get_script_with_args` in
//! `cli/task_runner.rs` of denoland/deno), and the `task` subcommand keeps a
//! `--` as a literal argument (`keep_double_dash: true` for `TASK_SUBCOMMAND`
//! in `libs/cli_parser/src/defs.rs`). Deno runs tasks in the config file's
//! directory (<https://docs.deno.com/runtime/reference/cli/task/>).
//!
//! [`DenoTasks`] adapts [`plan`] to `devlanes_core::contrib::ScriptRunner`.

mod adapter;
mod jsonc;

pub use adapter::DenoTasks;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Config file names Deno discovers, in its lookup order.
const CONFIG_NAMES: &[&str] = &["deno.json", "deno.jsonc"];

/// A `run_script` reference to a Deno task.
#[derive(Debug, Clone, Copy)]
pub struct TaskRequest<'a> {
    /// Root of the lane's worktree; the config file must be inside it.
    pub worktree: &'a Path,
    /// Path to `deno.json` or `deno.jsonc`, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    pub name: &'a str,
    /// Arguments appended to the task command, typically preset flags.
    pub extra_args: &'a [String],
}

/// What to spawn for a [`TaskRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Directory containing the config file.
    pub cwd: PathBuf,
    /// The task's command; `None` for a task that only lists dependencies.
    pub body: Option<String>,
}

/// Errors from resolving a Deno task.
#[derive(Debug, thiserror::Error)]
pub enum DenoError {
    #[error("invalid task name `{name}`: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("`{}` is outside the lane worktree `{}`", .file.display(), .worktree.display())]
    OutsideWorktree { file: PathBuf, worktree: PathBuf },
    #[error("`{}` is not a deno.json or deno.jsonc", .file.display())]
    NotConfig { file: PathBuf },
    #[error("cannot read `{}`", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("cannot parse `{}`", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        error: serde_json::Error,
    },
    #[error("`{}` has no task `{name}` ({})", .file.display(), known_hint(.available))]
    MissingTask {
        file: PathBuf,
        name: String,
        available: Vec<String>,
    },
}

/// Formats the task list for [`DenoError::MissingTask`].
fn known_hint(available: &[String]) -> String {
    if available.is_empty() {
        "it defines no tasks".to_owned()
    } else {
        format!("tasks: {}", available.join(", "))
    }
}

/// The `tasks` map of a Deno config.
#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    tasks: BTreeMap<String, Task>,
}

/// A task is a command string or an object with an optional command.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Task {
    Command(String),
    Object {
        #[serde(default)]
        command: Option<String>,
    },
}

/// Returns whether `path` names a file this runner handles.
pub fn handles(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| CONFIG_NAMES.contains(&name))
}

/// Reads the tasks of a Deno config, accepting comments and trailing commas.
pub fn read_tasks(path: &Path) -> Result<BTreeMap<String, Option<String>>, DenoError> {
    let text = std::fs::read_to_string(path).map_err(|error| DenoError::Read {
        path: path.to_owned(),
        error,
    })?;
    let config: Config =
        serde_json::from_str(&jsonc::to_json(&text)).map_err(|error| DenoError::Parse {
            path: path.to_owned(),
            error,
        })?;
    Ok(config
        .tasks
        .into_iter()
        .map(|(name, task)| {
            let body = match task {
                Task::Command(command) => Some(command),
                Task::Object { command } => command,
            };
            (name, body)
        })
        .collect())
}

/// Resolves a task reference into the command to spawn.
///
/// Reads the config file. Errors when it escapes the worktree, cannot be
/// parsed or has no such task.
pub fn plan(request: &TaskRequest<'_>) -> Result<LaunchPlan, DenoError> {
    validate_name(request.name)?;
    let worktree = normalize(request.worktree);
    let file = normalize(&worktree.join(request.source));
    if !file.starts_with(&worktree) {
        return Err(DenoError::OutsideWorktree { file, worktree });
    }
    if !handles(&file) {
        return Err(DenoError::NotConfig { file });
    }
    let mut tasks = read_tasks(&file)?;
    let Some(body) = tasks.remove(request.name) else {
        return Err(DenoError::MissingTask {
            file,
            name: request.name.to_owned(),
            available: tasks.into_keys().collect(),
        });
    };
    let cwd = file
        .parent()
        .map_or_else(|| worktree.clone(), Path::to_path_buf);
    let mut argv = vec!["deno".to_owned(), "task".to_owned()];
    // Deno prefers deno.json, so a deno.jsonc next to one must be named.
    if file.file_name().is_some_and(|name| name == "deno.jsonc") && cwd.join("deno.json").is_file()
    {
        argv.push("--config".to_owned());
        argv.push("deno.jsonc".to_owned());
    }
    argv.push(request.name.to_owned());
    argv.extend(request.extra_args.iter().cloned());
    Ok(LaunchPlan { argv, cwd, body })
}

/// Rejects names deno would read as one of its own options.
fn validate_name(name: &str) -> Result<(), DenoError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.starts_with('-') {
        "it starts with `-` and would be read as an option"
    } else {
        return Ok(());
    };
    Err(DenoError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

/// Resolves `.` and `..` lexically so the worktree check needs no filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}
