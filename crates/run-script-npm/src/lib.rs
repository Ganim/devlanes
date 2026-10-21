//! `run_script` support for `package.json` scripts.
//!
//! [`plan`] picks the package manager from the nearest lockfile and renders
//! `<pm> run <name>` in the manifest's directory. It reads the manifest,
//! lockfile headers and `package.json` files on the way up to the lane
//! worktree, so async callers should run it on a blocking thread.
//!
//! [`NpmScripts`] adapts it to `devlanes_core::contrib::ScriptRunner`, and
//! [`forward_args`] is the helper presets use to add arguments to a draft.

mod adapter;
mod detect;

pub use adapter::{NpmScripts, forward_args, needs_npm_separator};
pub use detect::{Detection, DetectionSource, detect_package_manager};

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// File name handled by this runner.
pub const MANIFEST: &str = "package.json";

/// Package managers that can run `package.json` scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageManager {
    Npm,
    Pnpm,
    /// Yarn 1.x.
    YarnClassic,
    /// Yarn 2 and later.
    YarnBerry,
    Bun,
}

impl PackageManager {
    /// Returns the executable name looked up on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::YarnClassic | Self::YarnBerry => "yarn",
            Self::Bun => "bun",
        }
    }

    /// Returns whether arguments for the script need a `--` after its name.
    ///
    /// Checked in October 2026 against the official docs and against npm 11.17,
    /// pnpm 12.10, yarn 1.22 and 4.18 and bun 1.3:
    ///
    /// - npm: "Use `--` to pass `-`-prefixed flags and options which would
    ///   otherwise be parsed by npm" (<https://docs.npmjs.com/cli/v11/commands/npm-run>).
    ///   Without it npm consumes `--port` itself.
    /// - pnpm: "Options listed after the script's name are passed to the executed
    ///   script" (<https://pnpm.io/cli/run>). A `--` is forwarded literally, and
    ///   CLIs such as Vite then treat the flags after it as positionals.
    /// - yarn 2+: `yarn run <scriptName> ...` forwards everything verbatim,
    ///   including `--` (<https://yarnpkg.com/cli/run>).
    /// - yarn 1: "pass additional arguments to your script by passing them after
    ///   the script name" (<https://classic.yarnpkg.com/en/docs/cli/run>). It drops
    ///   `--` with a warning that a future version will forward it as-is.
    /// - bun: `bun [bun flags] run <script> [script flags]`; flags after the
    ///   script name go to the script (<https://bun.com/docs/runtime>).
    pub fn needs_separator(self) -> bool {
        matches!(self, Self::Npm)
    }

    /// Renders `<pm> run <script> [--] <extra_args>`, adding `--` only when
    /// [`Self::needs_separator`] is true and there are arguments to forward.
    pub fn run_argv(self, script: &str, extra_args: &[String]) -> Vec<String> {
        let mut argv = vec![
            self.program().to_owned(),
            "run".to_owned(),
            script.to_owned(),
        ];
        if !extra_args.is_empty() && self.needs_separator() {
            argv.push("--".to_owned());
        }
        argv.extend(extra_args.iter().cloned());
        argv
    }
}

/// A `run_script` reference to a `package.json` script.
#[derive(Debug, Clone, Copy)]
pub struct ScriptRequest<'a> {
    /// Root of the lane's worktree. Lockfile lookup never goes above it.
    pub worktree: &'a Path,
    /// Path to the `package.json`, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    /// Script name in `scripts`.
    pub name: &'a str,
    /// Arguments forwarded to the script, typically preset flags.
    pub extra_args: &'a [String],
}

/// What to spawn for a [`ScriptRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Directory containing the `package.json`.
    pub cwd: PathBuf,
    pub manager: PackageManager,
    pub detection: DetectionSource,
    /// The script's command line, for presets that reject hardcoded flags.
    pub script_body: String,
}

/// A script read from a `package.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageScript {
    pub manifest: PathBuf,
    pub name: String,
    pub body: String,
}

/// Errors from reading manifests and resolving the package manager.
#[derive(Debug, thiserror::Error)]
pub enum NpmError {
    #[error("invalid script name `{name}`: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("`{}` is outside the lane worktree `{}`", .file.display(), .worktree.display())]
    OutsideWorktree { file: PathBuf, worktree: PathBuf },
    #[error("`{}` is not a package.json", .file.display())]
    NotManifest { file: PathBuf },
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
    #[error("`{}` has no script `{name}` ({})", .file.display(), available_hint(.available))]
    MissingScript {
        file: PathBuf,
        name: String,
        available: Vec<String>,
    },
    #[error(
        "`{}` has lockfiles of several package managers ({}); set `packageManager` in its package.json or delete the stale lockfile",
        .dir.display(),
        .lockfiles.join(", ")
    )]
    AmbiguousLockfiles {
        dir: PathBuf,
        lockfiles: Vec<String>,
    },
}

/// Formats the script list for [`NpmError::MissingScript`].
fn available_hint(available: &[String]) -> String {
    if available.is_empty() {
        "it defines no scripts".to_owned()
    } else {
        format!("available: {}", available.join(", "))
    }
}

/// The subset of `package.json` devlanes reads.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Manifest {
    #[serde(default)]
    pub(crate) scripts: BTreeMap<String, String>,
    #[serde(default, rename = "packageManager")]
    pub(crate) package_manager: Option<String>,
}

/// Parses a `package.json`, returning `None` when the file does not exist.
pub(crate) fn read_manifest(path: &Path) -> Result<Option<Manifest>, NpmError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(NpmError::Read {
                path: path.to_owned(),
                error,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| NpmError::Parse {
            path: path.to_owned(),
            error,
        })
}

/// Returns whether `path` names a file this runner handles.
pub fn handles(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == MANIFEST)
}

/// Reads script `name` from the manifest at `manifest`.
///
/// Fails when the file is missing, is not valid JSON or has no such script.
pub fn read_script(manifest: &Path, name: &str) -> Result<PackageScript, NpmError> {
    let parsed = read_manifest(manifest)?.ok_or_else(|| NpmError::Read {
        path: manifest.to_owned(),
        error: std::io::ErrorKind::NotFound.into(),
    })?;
    match parsed.scripts.get(name) {
        Some(body) => Ok(PackageScript {
            manifest: manifest.to_owned(),
            name: name.to_owned(),
            body: body.clone(),
        }),
        None => Err(NpmError::MissingScript {
            file: manifest.to_owned(),
            name: name.to_owned(),
            available: parsed.scripts.into_keys().collect(),
        }),
    }
}

/// Resolves a script reference into the command to spawn.
///
/// Errors when the source escapes the worktree, the script is missing or
/// lockfiles of different managers sit in the same directory without a
/// `packageManager` field to pick one.
pub fn plan(request: &ScriptRequest<'_>) -> Result<LaunchPlan, NpmError> {
    validate_name(request.name)?;
    let worktree = normalize(request.worktree);
    let file = normalize(&worktree.join(request.source));
    if !file.starts_with(&worktree) {
        return Err(NpmError::OutsideWorktree { file, worktree });
    }
    if !handles(&file) {
        return Err(NpmError::NotManifest { file });
    }
    let script = read_script(&file, request.name)?;
    let cwd = file
        .parent()
        .map_or_else(|| worktree.clone(), Path::to_path_buf);
    let detection = detect_package_manager(&cwd, &worktree)?;
    Ok(LaunchPlan {
        argv: detection.manager.run_argv(request.name, request.extra_args),
        cwd,
        manager: detection.manager,
        detection: detection.source,
        script_body: script.body,
    })
}

/// Rejects names the package manager would parse as one of its own options.
fn validate_name(name: &str) -> Result<(), NpmError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.starts_with('-') {
        "it starts with `-` and would be read as an option"
    } else {
        return Ok(());
    };
    Err(NpmError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

/// Resolves `.` and `..` lexically. The worktree check must not follow
/// symlinks or require the file to exist before a precise error is possible.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Only npm gets a separator, and only when something is forwarded.
    #[test]
    fn separator_rules() {
        let args = vec!["--port".to_owned(), "1".to_owned()];
        assert_eq!(
            PackageManager::Npm.run_argv("dev", &args),
            ["npm", "run", "dev", "--", "--port", "1"]
        );
        assert_eq!(
            PackageManager::Npm.run_argv("dev", &[]),
            ["npm", "run", "dev"]
        );
        for manager in [
            PackageManager::Pnpm,
            PackageManager::YarnClassic,
            PackageManager::YarnBerry,
            PackageManager::Bun,
        ] {
            assert_eq!(manager.run_argv("dev", &args)[3..], ["--port", "1"]);
        }
    }

    /// Parent components cannot climb out of the worktree unnoticed.
    #[test]
    fn normalize_resolves_parent_components() {
        assert_eq!(
            normalize(Path::new("/w/a/../b/./package.json")),
            Path::new("/w/b/package.json")
        );
        assert!(!normalize(Path::new("/w/../x/package.json")).starts_with("/w"));
    }
}
