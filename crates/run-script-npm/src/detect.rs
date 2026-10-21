//! Package manager selection: the declared field first, lockfiles second.
//!
//! `packageManager` (corepack's field, e.g. `"pnpm@10.4.1"`) is
//! authoritative. npm's `devEngines.packageManager` is accepted too: an
//! object or an array of objects with `name` and optional `version` and
//! `onFail` (<https://docs.npmjs.com/cli/v11/configuring-npm/package-json#devengines>).
//! Lockfiles only decide when no field exists, and the result carries a note
//! asking for the field.

use crate::manager::{Family, PackageManager};
use crate::yarn::{self, YarnLinker};
use crate::{MANIFEST, Manifest, NpmError, read_manifest};
use std::path::{Path, PathBuf};

/// Why a package manager was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionSource {
    /// `packageManager` in this `package.json`.
    PackageManagerField(PathBuf),
    /// `devEngines.packageManager` in this `package.json`.
    DevEngines(PathBuf),
    /// No declared field; the nearest lockfile decided.
    Lockfile(PathBuf),
    /// Neither was found, so npm, which ships with node, is used.
    Default,
}

/// The chosen package manager, the evidence and anything worth telling the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub manager: PackageManager,
    pub source: DetectionSource,
    /// Version from the declared field, such as `10.4.1` or a devEngines range.
    /// Kept for diagnostics; corepack or the manager enforces it.
    pub version: Option<String>,
    /// Install mode for yarn 2+, read from `.yarnrc.yml`.
    pub yarn_linker: Option<YarnLinker>,
    /// Non-fatal findings: a missing field, a lockfile that contradicts it,
    /// a missing `.pnp.cjs`.
    pub notes: Vec<String>,
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

/// Which field declared the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    PackageManager,
    DevEngines,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::PackageManager => "packageManager",
            Self::DevEngines => "devEngines.packageManager",
        }
    }
}

/// A manager named by a field.
#[derive(Debug, Clone)]
struct Declared {
    family: Family,
    version: Option<String>,
    field: Field,
    manifest: PathBuf,
}

/// Lockfiles found in one directory.
struct Lockfiles<'a> {
    dir: &'a Path,
    found: Vec<(&'static str, Family)>,
}

/// Finds the package manager for scripts in `package_dir`.
///
/// Fields are searched from `package_dir` up to the workspace root: the
/// first directory with `pnpm-workspace.yaml`, a `package.json` with
/// `workspaces`, or a lockfile, else `boundary`. Lockfiles are searched up to
/// `boundary` (inclusive). Directories above `boundary` are never read, so a
/// parent checkout cannot influence a lane.
pub fn detect_package_manager(package_dir: &Path, boundary: &Path) -> Result<Detection, NpmError> {
    let dirs: Vec<&Path> = std::iter::once(package_dir)
        .chain(
            package_dir
                .ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(boundary)),
        )
        .collect();
    let shown = |path: &Path| relative(path, boundary);
    let mut notes = Vec::new();
    let mut root = package_dir;
    let mut declared = None;
    for dir in &dirs {
        let manifest_path = dir.join(MANIFEST);
        let manifest = read_manifest(&manifest_path)?;
        if declared.is_none()
            && let Some(manifest) = &manifest
        {
            declared = declared_in(manifest, &manifest_path, &shown(&manifest_path), &mut notes);
        }
        root = dir;
        let is_root = dir.join("pnpm-workspace.yaml").is_file()
            || manifest.as_ref().is_some_and(|m| m.workspaces.is_some())
            || !lockfiles_in(dir).is_empty();
        if is_root {
            break;
        }
    }
    let lockfiles = dirs.iter().find_map(|dir| {
        let found = lockfiles_in(dir);
        (!found.is_empty()).then_some(Lockfiles { dir, found })
    });
    let mut detection = match declared {
        Some(declared) => from_declared(declared, lockfiles.as_ref(), root, &shown, notes)?,
        None => from_lockfiles(lockfiles.as_ref(), root, &shown, notes)?,
    };
    if detection.manager == PackageManager::YarnBerry {
        let yarn_root = match (&detection.source, &lockfiles) {
            (_, Some(lock)) => lock.dir,
            (
                DetectionSource::PackageManagerField(path) | DetectionSource::DevEngines(path),
                None,
            ) => path.parent().unwrap_or(root),
            _ => root,
        };
        detection.yarn_linker = Some(yarn::linker(yarn_root));
        detection
            .notes
            .extend(yarn::pnp_note(yarn_root, &shown(yarn_root)));
    }
    Ok(detection)
}

/// Uses the declared manager and notes a lockfile of another manager.
fn from_declared(
    declared: Declared,
    lockfiles: Option<&Lockfiles<'_>>,
    root: &Path,
    shown: &dyn Fn(&Path) -> String,
    mut notes: Vec<String>,
) -> Result<Detection, NpmError> {
    let yarn_lock = lockfiles
        .filter(|lock| lock.found.iter().any(|(_, f)| *f == Family::Yarn))
        .map(|lock| lock.dir.join("yarn.lock"));
    let manager = match declared.family {
        Family::Npm => PackageManager::Npm,
        Family::Pnpm => PackageManager::Pnpm,
        Family::Bun => PackageManager::Bun,
        Family::Yarn => {
            let yarn_root = declared.manifest.parent().unwrap_or(root);
            yarn::flavor(
                major(declared.version.as_deref()),
                yarn_lock.as_deref(),
                yarn_root,
            )?
        }
    };
    if let Some(lock) = lockfiles
        && !lock
            .found
            .iter()
            .any(|(_, family)| *family == declared.family)
    {
        let names: Vec<String> = lock
            .found
            .iter()
            .map(|(name, _)| shown(&lock.dir.join(name)))
            .collect();
        notes.push(format!(
            "{} in {} says {}, but {} belongs to another package manager; using {}",
            declared.field.label(),
            shown(&declared.manifest),
            declared.family.name(),
            names.join(", "),
            declared.family.name(),
        ));
    }
    let source = match declared.field {
        Field::PackageManager => DetectionSource::PackageManagerField(declared.manifest),
        Field::DevEngines => DetectionSource::DevEngines(declared.manifest),
    };
    Ok(Detection {
        manager,
        source,
        version: declared.version,
        yarn_linker: None,
        notes,
    })
}

/// Picks a manager from the nearest lockfiles when no field declares one.
fn from_lockfiles(
    lockfiles: Option<&Lockfiles<'_>>,
    root: &Path,
    shown: &dyn Fn(&Path) -> String,
    mut notes: Vec<String>,
) -> Result<Detection, NpmError> {
    let root_manifest = shown(&root.join(MANIFEST));
    let Some(lock) = lockfiles else {
        notes.push(format!(
            "no packageManager field or lockfile; using npm. Add \"packageManager\": \"<manager>@<version>\" to {root_manifest}"
        ));
        return Ok(Detection {
            manager: PackageManager::Npm,
            source: DetectionSource::Default,
            version: None,
            yarn_linker: None,
            notes,
        });
    };
    let (name, family) = lock.found[0];
    if lock.found.iter().any(|(_, f)| *f != family) {
        return Err(NpmError::AmbiguousLockfiles {
            dir: lock.dir.to_owned(),
            lockfiles: lock.found.iter().map(|(n, _)| (*n).to_owned()).collect(),
        });
    }
    let lockfile = lock.dir.join(name);
    let manager = match family {
        Family::Npm => PackageManager::Npm,
        Family::Pnpm => PackageManager::Pnpm,
        Family::Bun => PackageManager::Bun,
        Family::Yarn => yarn::flavor(None, Some(&lockfile), lock.dir)?,
    };
    notes.push(format!(
        "no packageManager field; detected {} from {}. Add \"packageManager\": \"{}@<version>\" to {root_manifest}",
        family.name(),
        shown(&lockfile),
        family.name(),
    ));
    Ok(Detection {
        manager,
        source: DetectionSource::Lockfile(lockfile),
        version: None,
        yarn_linker: None,
        notes,
    })
}

/// Lockfiles present in `dir`, in [`LOCKFILES`] order.
fn lockfiles_in(dir: &Path) -> Vec<(&'static str, Family)> {
    LOCKFILES
        .iter()
        .copied()
        .filter(|(name, _)| dir.join(name).is_file())
        .collect()
}

/// Reads the declared manager of one manifest. `packageManager` wins over
/// `devEngines.packageManager`; disagreement and unknown names become notes.
fn declared_in(
    manifest: &Manifest,
    path: &Path,
    shown: &str,
    notes: &mut Vec<String>,
) -> Option<Declared> {
    let from_field = manifest.package_manager.as_deref().and_then(|value| {
        let (name, version) = value.split_once('@').unwrap_or((value, ""));
        // Corepack appends `+sha512.<hash>`; the version is what users compare.
        let version = version.split('+').next().unwrap_or_default();
        match Family::from_name(name) {
            Some(family) => Some((family, (!version.is_empty()).then(|| version.to_owned()))),
            None => {
                notes.push(format!(
                    "packageManager \"{value}\" in {shown} is not npm, pnpm, yarn or bun; ignored"
                ));
                None
            }
        }
    });
    let from_dev_engines = manifest
        .dev_engines
        .as_ref()
        .and_then(|engines| engines.get("packageManager"))
        .and_then(|value| dev_engines_entry(value, shown, notes));
    let declared = |(family, version), field| Declared {
        family,
        version,
        field,
        manifest: path.to_owned(),
    };
    match (from_field, from_dev_engines) {
        (Some(field), Some(engines)) => {
            if field.0 != engines.0 {
                notes.push(format!(
                    "packageManager ({}) and devEngines.packageManager ({}) in {shown} disagree; using packageManager",
                    field.0.name(),
                    engines.0.name()
                ));
            }
            Some(declared(field, Field::PackageManager))
        }
        (Some(field), None) => Some(declared(field, Field::PackageManager)),
        (None, Some(engines)) => Some(declared(engines, Field::DevEngines)),
        (None, None) => None,
    }
}

/// Reads `devEngines.packageManager`: one `{name, version?, onFail?}` object
/// or an array of them, of which the first supported entry is used.
fn dev_engines_entry(
    value: &serde_json::Value,
    shown: &str,
    notes: &mut Vec<String>,
) -> Option<(Family, Option<String>)> {
    let parse = |entry: &serde_json::Value| {
        let family = Family::from_name(entry.get("name")?.as_str()?)?;
        let version = entry
            .get("version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Some((family, version))
    };
    let entries: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    let supported: Vec<(Family, Option<String>)> =
        entries.iter().filter_map(|e| parse(e)).collect();
    match supported.as_slice() {
        [] => {
            notes.push(format!(
                "devEngines.packageManager in {shown} names no supported package manager; ignored"
            ));
            None
        }
        [only] => Some(only.clone()),
        [first, ..] => {
            notes.push(format!(
                "devEngines.packageManager in {shown} lists several package managers; using the first, {}",
                first.0.name()
            ));
            Some(first.clone())
        }
    }
}

/// Leading major version of `10.4.1`, `^4.0.0` or `>=2`.
fn major(version: Option<&str>) -> Option<u64> {
    let digits: String = version?
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `path` relative to the worktree for notes; `.` for the worktree itself.
fn relative(path: &Path, boundary: &Path) -> String {
    match path.strip_prefix(boundary) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".to_owned(),
        Ok(rest) => rest.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}
