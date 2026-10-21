//! Package manager detection from lockfiles and the `packageManager` field.

use crate::{Manifest, NpmError, PackageManager, read_manifest};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Why a package manager was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionSource {
    /// The nearest lockfile; when several managers' lockfiles share a directory,
    /// that directory's `packageManager` field picked one.
    Lockfile(PathBuf),
    /// No lockfile up to the worktree; the nearest `package.json` with a
    /// `packageManager` field decided.
    PackageManagerField(PathBuf),
    /// Neither was found, so npm, which ships with node, is used.
    Default,
}

/// The chosen package manager and the evidence for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub manager: PackageManager,
    pub source: DetectionSource,
}

/// Manager families as lockfiles identify them; yarn's major version is
/// resolved separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

/// Lockfile names in reporting order. `bun.lockb` is the binary format used
/// before bun 1.2 switched to the text `bun.lock`.
const LOCKFILES: &[(&str, Family)] = &[
    ("pnpm-lock.yaml", Family::Pnpm),
    ("package-lock.json", Family::Npm),
    ("npm-shrinkwrap.json", Family::Npm),
    ("yarn.lock", Family::Yarn),
    ("bun.lock", Family::Bun),
    ("bun.lockb", Family::Bun),
];

/// A parsed `packageManager` value such as `pnpm@9.12.0+sha512...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Declared {
    family: Family,
    major: Option<u64>,
}

/// Finds the package manager for scripts in `package_dir`.
///
/// Walks from `package_dir` up to `boundary` (inclusive) and stops at the
/// first directory holding a lockfile, which is the workspace root for
/// pnpm/npm/yarn/bun workspaces. Directories above `boundary` are never
/// read, so a parent checkout cannot influence a lane.
pub fn detect_package_manager(package_dir: &Path, boundary: &Path) -> Result<Detection, NpmError> {
    let dirs: Vec<&Path> = std::iter::once(package_dir)
        .chain(
            package_dir
                .ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(boundary)),
        )
        .collect();
    for dir in &dirs {
        if let Some(detection) = from_lockfiles(dir)? {
            return Ok(detection);
        }
    }
    for dir in &dirs {
        let manifest = dir.join(crate::MANIFEST);
        if let Some(declared) = declared(read_manifest(&manifest)?.as_ref()) {
            return Ok(Detection {
                manager: resolve(declared.family, dir, None, Some(declared))?,
                source: DetectionSource::PackageManagerField(manifest),
            });
        }
    }
    Ok(Detection {
        manager: PackageManager::Npm,
        source: DetectionSource::Default,
    })
}

/// Picks a manager from the lockfiles in `dir`, if there are any.
fn from_lockfiles(dir: &Path) -> Result<Option<Detection>, NpmError> {
    let found: Vec<(&str, Family)> = LOCKFILES
        .iter()
        .copied()
        .filter(|(name, _)| dir.join(name).is_file())
        .collect();
    let Some(&(first_name, first_family)) = found.first() else {
        return Ok(None);
    };
    // Read lazily: most directories have one lockfile and need no manifest.
    let mut manifest: Option<Option<Manifest>> = None;
    let mut load = |dir: &Path| -> Result<Option<Declared>, NpmError> {
        if manifest.is_none() {
            manifest = Some(read_manifest(&dir.join(crate::MANIFEST))?);
        }
        Ok(declared(manifest.as_ref().and_then(Option::as_ref)))
    };
    let (name, family) = if found.iter().all(|(_, family)| *family == first_family) {
        (first_name, first_family)
    } else {
        let declared = load(dir)?;
        match declared.and_then(|d| found.iter().find(|(_, family)| *family == d.family)) {
            Some(&(name, family)) => (name, family),
            None => {
                return Err(NpmError::AmbiguousLockfiles {
                    dir: dir.to_owned(),
                    lockfiles: found.iter().map(|(name, _)| (*name).to_owned()).collect(),
                });
            }
        }
    };
    let lockfile = dir.join(name);
    let declared = if family == Family::Yarn {
        load(dir)?
    } else {
        None
    };
    Ok(Some(Detection {
        manager: resolve(family, dir, Some(&lockfile), declared)?,
        source: DetectionSource::Lockfile(lockfile),
    }))
}

/// Turns a family into a concrete manager, telling yarn 1 from yarn 2+.
///
/// Yarn evidence, strongest first: the lockfile header (`__metadata:` only
/// exists in 2+ lockfiles, `# yarn lockfile v1` only in 1.x), the declared
/// major version, then `.yarnrc.yml`, which only 2+ reads. Yarn 1 is the
/// fallback because a bare global `yarn` is usually 1.x.
fn resolve(
    family: Family,
    dir: &Path,
    lockfile: Option<&Path>,
    declared: Option<Declared>,
) -> Result<PackageManager, NpmError> {
    Ok(match family {
        Family::Npm => PackageManager::Npm,
        Family::Pnpm => PackageManager::Pnpm,
        Family::Bun => PackageManager::Bun,
        Family::Yarn => {
            if let Some(manager) = lockfile.map(yarn_from_lockfile).transpose()?.flatten() {
                manager
            } else if let Some(major) = declared
                .filter(|d| d.family == Family::Yarn)
                .and_then(|d| d.major)
            {
                if major >= 2 {
                    PackageManager::YarnBerry
                } else {
                    PackageManager::YarnClassic
                }
            } else if dir.join(".yarnrc.yml").is_file() {
                PackageManager::YarnBerry
            } else {
                PackageManager::YarnClassic
            }
        }
    })
}

/// Classifies a `yarn.lock` by its header; `None` when it has neither marker
/// (for example an empty file created by hand).
fn yarn_from_lockfile(path: &Path) -> Result<Option<PackageManager>, NpmError> {
    let read_error = |error| NpmError::Read {
        path: path.to_owned(),
        error,
    };
    // Both markers sit in the first lines; lockfiles can be many megabytes.
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(path)
        .map_err(read_error)?
        .take(4096)
        .read_to_end(&mut head)
        .map_err(read_error)?;
    let head = String::from_utf8_lossy(&head);
    Ok(if head.contains("__metadata:") {
        Some(PackageManager::YarnBerry)
    } else if head.contains("# yarn lockfile v1") {
        Some(PackageManager::YarnClassic)
    } else {
        None
    })
}

/// Parses a manifest's `packageManager` field; unknown managers are ignored.
fn declared(manifest: Option<&Manifest>) -> Option<Declared> {
    let value = manifest?.package_manager.as_deref()?;
    let (name, version) = value.split_once('@').unwrap_or((value, ""));
    let family = match name.trim() {
        "npm" => Family::Npm,
        "pnpm" => Family::Pnpm,
        "yarn" => Family::Yarn,
        "bun" => Family::Bun,
        _ => return None,
    };
    let major = version
        .split(['.', '+', '-'])
        .next()
        .and_then(|major| major.parse().ok());
    Some(Declared { family, major })
}
