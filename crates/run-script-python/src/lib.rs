//! `run_script` support for Python project scripts: `{ file: pyproject.toml, name: <script> }`
//! runs `uv run <script>` or `poetry run <script>` in the project directory.
//!
//! Scripts come from `[project.scripts]` and `[tool.poetry.scripts]`. uv is
//! chosen when a `uv.lock` exists up to the worktree (uv workspaces keep one
//! at the root) or the project has a `[tool.uv]` table; poetry when a
//! `poetry.lock` sits next to the `pyproject.toml`. uv wins when both apply.
//!
//! Neither tool gets a `--`. uv passes everything after the command name to
//! it and forwards a `--` there literally (checked with uv 0.13); poetry
//! documents "Any extra arguments are forwarded to the command"
//! (<https://python-poetry.org/docs/cli/#run>).
//!
//! [`PythonScripts`] adapts [`plan`] to `devlanes_core::contrib::ScriptRunner`.

mod adapter;

pub use adapter::PythonScripts;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// File name handled by this runner.
pub const MANIFEST: &str = "pyproject.toml";

/// Tools that run a project's scripts inside its environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonRunner {
    Uv,
    Poetry,
}

impl PythonRunner {
    /// Returns the executable name.
    pub fn program(self) -> &'static str {
        match self {
            Self::Uv => "uv",
            Self::Poetry => "poetry",
        }
    }

    /// Renders `<tool> run <script> <args>`.
    pub fn run_argv(self, script: &str, extra_args: &[String]) -> Vec<String> {
        let mut argv = vec![
            self.program().to_owned(),
            "run".to_owned(),
            script.to_owned(),
        ];
        argv.extend(extra_args.iter().cloned());
        argv
    }
}

/// A `run_script` reference to a Python project script.
#[derive(Debug, Clone, Copy)]
pub struct ScriptRequest<'a> {
    /// Root of the lane's worktree; lockfile lookup never goes above it.
    pub worktree: &'a Path,
    /// Path to a `pyproject.toml`, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    pub name: &'a str,
    /// Arguments for the script, typically preset flags.
    pub extra_args: &'a [String],
}

/// What to spawn for a [`ScriptRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Directory containing the `pyproject.toml`.
    pub cwd: PathBuf,
    pub runner: PythonRunner,
    /// What selected the runner: `uv.lock`, `[tool.uv]` or `poetry.lock`,
    /// with the lockfile path relative to the worktree.
    pub evidence: String,
    /// Entry point such as `acme.server:main`.
    pub entry_point: Option<String>,
    /// Non-fatal findings, such as both tools being configured.
    pub notes: Vec<String>,
}

/// Errors from resolving a Python project script.
#[derive(Debug, thiserror::Error)]
pub enum PythonError {
    #[error("invalid script name `{name}`: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("`{}` is outside the lane worktree `{}`", .file.display(), .worktree.display())]
    OutsideWorktree { file: PathBuf, worktree: PathBuf },
    #[error("`{}` is not a pyproject.toml", .file.display())]
    NotPyproject { file: PathBuf },
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
        error: toml::de::Error,
    },
    #[error(
        "`{}` has no script `{name}` in [project.scripts] or [tool.poetry.scripts] ({})",
        .file.display(),
        known_hint(.available)
    )]
    MissingScript {
        file: PathBuf,
        name: String,
        available: Vec<String>,
    },
    #[error(
        "`{}` has no uv.lock, [tool.uv] table or poetry.lock, so devlanes cannot tell uv from poetry; run `uv lock` or `poetry lock` in its directory",
        .file.display()
    )]
    NoRunner { file: PathBuf },
}

/// Formats the script list for [`PythonError::MissingScript`].
fn known_hint(available: &[String]) -> String {
    if available.is_empty() {
        "it defines no scripts".to_owned()
    } else {
        format!("scripts: {}", available.join(", "))
    }
}

/// The parts of `pyproject.toml` this runner reads.
#[derive(Debug, Default, Deserialize)]
struct Pyproject {
    project: Option<Project>,
    #[serde(rename = "build-system")]
    build_system: Option<toml::Value>,
    tool: Option<Tool>,
}

#[derive(Debug, Default, Deserialize)]
struct Project {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct Tool {
    uv: Option<UvTool>,
    poetry: Option<PoetryTool>,
}

#[derive(Debug, Default, Deserialize)]
struct UvTool {
    package: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PoetryTool {
    /// A string entry point, or a table with `reference` (poetry 2) or
    /// `callable` (poetry 1).
    #[serde(default)]
    scripts: BTreeMap<String, toml::Value>,
}

/// Returns whether `path` names a file this runner handles.
pub fn handles(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == MANIFEST)
}

/// Resolves a script reference into the command to spawn.
///
/// Reads the `pyproject.toml` and checks for lockfiles. Errors when the
/// script is not declared or neither uv nor poetry is configured.
pub fn plan(request: &ScriptRequest<'_>) -> Result<LaunchPlan, PythonError> {
    validate_name(request.name)?;
    let worktree = normalize(request.worktree);
    let file = normalize(&worktree.join(request.source));
    if !file.starts_with(&worktree) {
        return Err(PythonError::OutsideWorktree { file, worktree });
    }
    if !handles(&file) {
        return Err(PythonError::NotPyproject { file });
    }
    let text = std::fs::read_to_string(&file).map_err(|error| PythonError::Read {
        path: file.clone(),
        error,
    })?;
    let pyproject: Pyproject = toml::from_str(&text).map_err(|error| PythonError::Parse {
        path: file.clone(),
        error,
    })?;
    let cwd = file
        .parent()
        .map_or_else(|| worktree.clone(), Path::to_path_buf);
    let project_scripts = pyproject.project.as_ref().map(|p| &p.scripts);
    let tool = pyproject.tool.as_ref();
    let poetry_scripts = tool.and_then(|t| t.poetry.as_ref()).map(|p| &p.scripts);
    let entry_point = match (
        project_scripts.and_then(|s| s.get(request.name)),
        poetry_scripts.and_then(|s| s.get(request.name)),
    ) {
        (Some(entry), _) => Some(entry.clone()),
        (None, Some(entry)) => Some(poetry_entry(entry)),
        (None, None) => {
            let mut available: Vec<String> = project_scripts
                .into_iter()
                .flat_map(BTreeMap::keys)
                .chain(poetry_scripts.into_iter().flat_map(BTreeMap::keys))
                .cloned()
                .collect();
            available.sort();
            available.dedup();
            return Err(PythonError::MissingScript {
                file,
                name: request.name.to_owned(),
                available,
            });
        }
    };
    let uv_tool = tool.and_then(|t| t.uv.as_ref());
    let uv_lock = std::iter::once(cwd.as_path())
        .chain(
            cwd.ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(&worktree)),
        )
        .map(|dir| dir.join("uv.lock"))
        .find(|path| path.is_file());
    let poetry_lock = cwd.join("poetry.lock").is_file();
    let mut notes = Vec::new();
    let runner = match (uv_lock.is_some() || uv_tool.is_some(), poetry_lock) {
        (true, true) => {
            notes.push(
                "both uv and poetry are configured (poetry.lock is present); using uv".to_owned(),
            );
            PythonRunner::Uv
        }
        (true, false) => PythonRunner::Uv,
        (false, true) => PythonRunner::Poetry,
        (false, false) => return Err(PythonError::NoRunner { file }),
    };
    let shown = |path: &Path| {
        path.strip_prefix(&worktree)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let evidence = match (runner, &uv_lock) {
        (PythonRunner::Uv, Some(lock)) => shown(lock),
        (PythonRunner::Uv, None) => "[tool.uv]".to_owned(),
        (PythonRunner::Poetry, _) => shown(&cwd.join("poetry.lock")),
    };
    // uv only installs entry points for packaged projects, so the script
    // would not exist in the environment.
    let in_project_scripts = project_scripts.is_some_and(|s| s.contains_key(request.name));
    let packaged =
        pyproject.build_system.is_some() || uv_tool.and_then(|t| t.package) == Some(true);
    if runner == PythonRunner::Uv && in_project_scripts && !packaged {
        notes.push(format!(
            "uv installs [project.scripts] only for packaged projects; add a [build-system] table or `tool.uv.package = true` so `{}` exists",
            request.name
        ));
    }
    Ok(LaunchPlan {
        argv: runner.run_argv(request.name, request.extra_args),
        cwd,
        runner,
        evidence,
        entry_point,
        notes,
    })
}

impl LaunchPlan {
    /// One line on the chosen tool and entry point, e.g.
    /// `uv (uv.lock), entry point acme.server:main`.
    pub fn detail(&self) -> String {
        let mut detail = format!("{} ({})", self.runner.program(), self.evidence);
        if let Some(entry) = self.entry_point.as_deref().filter(|e| !e.is_empty()) {
            detail.push_str(&format!(", entry point {entry}"));
        }
        detail
    }
}

/// Reads a poetry script entry: a string, or a table naming the callable.
fn poetry_entry(entry: &toml::Value) -> String {
    match entry {
        toml::Value::String(reference) => reference.clone(),
        toml::Value::Table(table) => table
            .get("reference")
            .or_else(|| table.get("callable"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        // Poetry rejects other shapes itself; there is no entry point to show.
        _ => String::new(),
    }
}

/// Rejects names uv or poetry would read as one of their own options.
fn validate_name(name: &str) -> Result<(), PythonError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.starts_with('-') {
        "it starts with `-` and would be read as an option"
    } else {
        return Ok(());
    };
    Err(PythonError::InvalidName {
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
