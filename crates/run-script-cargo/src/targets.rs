//! Binary targets of a package and the members of a workspace.
//!
//! Follows Cargo's target auto-discovery
//! (<https://doc.rust-lang.org/cargo/reference/cargo-targets.html#target-auto-discovery>):
//! `src/main.rs` builds a binary named after the package, `src/bin/<name>.rs`
//! and `src/bin/<name>/main.rs` build `<name>`, `[[bin]]` entries add or
//! rename targets, and `autobins = false` turns discovery off. Edition 2015
//! packages that disable discovery implicitly are not modelled.

use crate::{CargoError, MANIFEST};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The parts of a `Cargo.toml` this runner reads.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Manifest {
    package: Option<PackageSection>,
    #[serde(default)]
    bin: Vec<BinSection>,
    pub(crate) workspace: Option<Workspace>,
}

#[derive(Debug, Deserialize)]
struct PackageSection {
    name: Option<String>,
    autobins: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct BinSection {
    name: Option<String>,
    path: Option<String>,
}

/// `[workspace]` members and exclusions, as path patterns relative to the root.
#[derive(Debug, Deserialize)]
pub(crate) struct Workspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

/// A package and the binaries it builds, sorted by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub bins: Vec<String>,
}

/// Parses a manifest.
pub(crate) fn read(path: &Path) -> Result<Manifest, CargoError> {
    let text = std::fs::read_to_string(path).map_err(|error| CargoError::Read {
        path: path.to_owned(),
        error,
    })?;
    toml::from_str(&text).map_err(|error| CargoError::Parse {
        path: path.to_owned(),
        error,
    })
}

/// Reads the package in `manifest` and its binaries; `None` for a virtual
/// workspace manifest. Blocking.
pub fn package_bins(manifest: &Path) -> Result<Option<Package>, CargoError> {
    let parsed = read(manifest)?;
    let dir = manifest.parent().unwrap_or(Path::new("."));
    Ok(package(dir, &parsed))
}

/// The package declared in `manifest`, whose directory is `dir`.
pub(crate) fn package(dir: &Path, manifest: &Manifest) -> Option<Package> {
    let section = manifest.package.as_ref()?;
    let name = section.name.clone()?;
    let mut bins: Vec<String> = manifest
        .bin
        .iter()
        .filter_map(|bin| bin.name.clone())
        .collect();
    let explicit_paths: Vec<PathBuf> = manifest
        .bin
        .iter()
        .filter_map(|bin| bin.path.as_deref().map(|path| dir.join(path)))
        .collect();
    if section.autobins != Some(false) {
        let main = dir.join("src/main.rs");
        if main.is_file() && !explicit_paths.contains(&main) && !bins.contains(&name) {
            bins.push(name.clone());
        }
        for (bin, path) in discovered_bins(&dir.join("src/bin")) {
            if !explicit_paths.contains(&path) && !bins.contains(&bin) {
                bins.push(bin);
            }
        }
    }
    bins.sort();
    bins.dedup();
    Some(Package { name, bins })
}

/// `src/bin/<name>.rs` and `src/bin/<name>/main.rs` entries.
fn discovered_bins(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                found.push((stem.to_owned(), path.clone()));
            }
        } else if path.join("main.rs").is_file() {
            found.push((name, path.join("main.rs")));
        }
    }
    found
}

/// Packages of the workspace rooted at `root`, skipping the root itself,
/// excluded paths and members without a readable manifest.
pub(crate) fn members(root: &Path, workspace: &Workspace) -> Result<Vec<Package>, CargoError> {
    let excluded: Vec<PathBuf> = workspace
        .exclude
        .iter()
        .map(|path| root.join(path))
        .collect();
    let mut dirs: Vec<PathBuf> = workspace
        .members
        .iter()
        .flat_map(|pattern| expand(root, pattern))
        .filter(|dir| dir != root && !excluded.iter().any(|ex| dir.starts_with(ex)))
        .collect();
    dirs.sort();
    dirs.dedup();
    let mut packages = Vec::new();
    for dir in dirs {
        let manifest = dir.join(MANIFEST);
        if !manifest.is_file() {
            continue;
        }
        if let Some(package) = package(&dir, &read(&manifest)?) {
            packages.push(package);
        }
    }
    Ok(packages)
}

/// Expands a member pattern; `*` and `?` match within one path segment.
fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![root.to_path_buf()];
    for segment in pattern.split('/').filter(|s| !s.is_empty() && *s != ".") {
        if !segment.contains(['*', '?']) {
            current = current.into_iter().map(|dir| dir.join(segment)).collect();
            continue;
        }
        let mut next = Vec::new();
        for dir in current {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if entry.path().is_dir() && wildcard(segment, &name.to_string_lossy()) {
                    next.push(entry.path());
                }
            }
        }
        current = next;
    }
    current
}

/// Matches `name` against a pattern with `*` (any run) and `?` (one char).
fn wildcard(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(c) if *c == '?' || *c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Member patterns use shell-style wildcards within a segment.
    #[test]
    fn wildcard_matching() {
        assert!(wildcard("*", "api"));
        assert!(wildcard("svc-*", "svc-api"));
        assert!(wildcard("a?c", "abc"));
        assert!(wildcard("*-worker*", "billing-worker-v2"));
        assert!(!wildcard("svc-*", "api"));
        assert!(!wildcard("a?c", "ac"));
    }
}
