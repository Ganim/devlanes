//! `run_script` support for Cargo binaries: `{ file: Cargo.toml, name: <bin> }` runs
//! `cargo run --bin <bin> -- <args>` in the manifest's directory.
//!
//! "All the arguments following the two dashes (`--`) are passed to the
//! binary to run" (<https://doc.rust-lang.org/cargo/commands/cargo-run.html>),
//! so the `--` is added whenever there are arguments. Running in the
//! manifest's directory makes Cargo pick that manifest, so no
//! `--manifest-path` is needed. A virtual workspace manifest finds the member
//! that owns the binary and adds `-p <package>`.
//!
//! [`CargoBins`] adapts [`plan`] to `devlanes_core::contrib::ScriptRunner`.

mod adapter;
mod targets;

pub use adapter::CargoBins;
pub use targets::{Package, package_bins};

use std::path::{Component, Path, PathBuf};

/// File name handled by this runner.
pub const MANIFEST: &str = "Cargo.toml";

/// A `run_script` reference to a Cargo binary.
#[derive(Debug, Clone, Copy)]
pub struct BinRequest<'a> {
    /// Root of the lane's worktree; the manifest must be inside it.
    pub worktree: &'a Path,
    /// Path to a `Cargo.toml`, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    /// Binary target name.
    pub name: &'a str,
    /// Arguments for the binary, passed after `--`.
    pub extra_args: &'a [String],
}

/// What to spawn for a [`BinRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Directory containing the manifest.
    pub cwd: PathBuf,
    /// Package that owns the binary.
    pub package: String,
}

/// Errors from resolving a Cargo binary.
#[derive(Debug, thiserror::Error)]
pub enum CargoError {
    #[error("invalid binary name `{name}`: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("`{}` is outside the lane worktree `{}`", .file.display(), .worktree.display())]
    OutsideWorktree { file: PathBuf, worktree: PathBuf },
    #[error("`{}` is not a Cargo.toml", .file.display())]
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
        error: toml::de::Error,
    },
    #[error("`{}` has neither [package] nor [workspace]", .file.display())]
    NoPackage { file: PathBuf },
    #[error("`{}` has no binary `{name}` ({})", .file.display(), known_hint(.available))]
    MissingBin {
        file: PathBuf,
        name: String,
        available: Vec<String>,
    },
    #[error(
        "binary `{name}` exists in several workspace members ({}); point run_script at the member's Cargo.toml",
        .packages.join(", ")
    )]
    AmbiguousBin { name: String, packages: Vec<String> },
}

/// Formats the binary list for [`CargoError::MissingBin`].
fn known_hint(available: &[String]) -> String {
    if available.is_empty() {
        "it builds no binaries".to_owned()
    } else {
        format!("binaries: {}", available.join(", "))
    }
}

/// Returns whether `path` names a file this runner handles.
pub fn handles(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == MANIFEST)
}

/// Resolves a binary reference into the command to spawn.
///
/// Reads the manifest, workspace member manifests and `src/` listings.
/// Errors when the binary does not exist or several members define it.
pub fn plan(request: &BinRequest<'_>) -> Result<LaunchPlan, CargoError> {
    validate_name(request.name)?;
    let worktree = normalize(request.worktree);
    let file = normalize(&worktree.join(request.source));
    if !file.starts_with(&worktree) {
        return Err(CargoError::OutsideWorktree { file, worktree });
    }
    if !handles(&file) {
        return Err(CargoError::NotManifest { file });
    }
    let cwd = file
        .parent()
        .map_or_else(|| worktree.clone(), Path::to_path_buf);
    let manifest = targets::read(&file)?;
    let mut available = Vec::new();
    if let Some(package) = targets::package(&cwd, &manifest) {
        if package.bins.iter().any(|bin| bin == request.name) {
            return Ok(render(cwd, package.name, None, request));
        }
        available.extend(package.bins);
    } else if manifest.workspace.is_none() {
        return Err(CargoError::NoPackage { file });
    }
    if let Some(workspace) = &manifest.workspace {
        // A root package selects only itself, and a virtual manifest selects
        // every member, so a member's binary needs `-p`.
        let mut owners = Vec::new();
        for member in targets::members(&cwd, workspace)? {
            if member.bins.iter().any(|bin| bin == request.name) {
                owners.push(member.name.clone());
            }
            available.extend(member.bins);
        }
        match owners.as_slice() {
            [owner] => {
                let owner = owner.clone();
                return Ok(render(cwd, owner.clone(), Some(owner), request));
            }
            [] => {}
            _ => {
                return Err(CargoError::AmbiguousBin {
                    name: request.name.to_owned(),
                    packages: owners,
                });
            }
        }
    }
    available.sort();
    available.dedup();
    Err(CargoError::MissingBin {
        file,
        name: request.name.to_owned(),
        available,
    })
}

/// Renders `cargo run [-p <package>] --bin <bin> [-- <args>]`.
fn render(
    cwd: PathBuf,
    package: String,
    select: Option<String>,
    request: &BinRequest<'_>,
) -> LaunchPlan {
    let mut argv = vec!["cargo".to_owned(), "run".to_owned()];
    if let Some(select) = select {
        argv.push("-p".to_owned());
        argv.push(select);
    }
    argv.push("--bin".to_owned());
    argv.push(request.name.to_owned());
    if !request.extra_args.is_empty() {
        argv.push("--".to_owned());
        argv.extend(request.extra_args.iter().cloned());
    }
    LaunchPlan { argv, cwd, package }
}

/// Rejects names cargo would read as one of its own options.
fn validate_name(name: &str) -> Result<(), CargoError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.starts_with('-') {
        "it starts with `-` and would be read as an option"
    } else {
        return Ok(());
    };
    Err(CargoError::InvalidName {
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
