//! yarn: `yarn run <script> <args>`, telling yarn 1 from yarn 2+ and noticing
//! Plug'n'Play installs.
//!
//! Neither major version gets a `--`. Yarn 2+ "forwards everything verbatim",
//! `--` included (`yarn run <scriptName> ...`, <https://yarnpkg.com/cli/run>;
//! checked with 4.18). Yarn 1 passes "additional arguments to your script by
//! passing them after the script name" (<https://classic.yarnpkg.com/en/docs/cli/run>)
//! and drops a `--` with a warning that a future version will forward it.

use crate::NpmError;
use crate::manager::{PackageManager, Rules};
use std::io::Read;
use std::path::Path;

/// Yarn 1's argv rules.
pub(crate) struct YarnClassic;

impl Rules for YarnClassic {
    fn program(&self) -> &'static str {
        "yarn"
    }

    fn needs_separator(&self) -> bool {
        false
    }
}

/// Yarn 2+'s argv rules.
pub(crate) struct YarnBerry;

impl Rules for YarnBerry {
    fn program(&self) -> &'static str {
        "yarn"
    }

    fn needs_separator(&self) -> bool {
        false
    }
}

/// How yarn 2+ installs dependencies (`nodeLinker` in `.yarnrc.yml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YarnLinker {
    /// Plug'n'Play, the default: no `node_modules`, resolution through `.pnp.cjs`.
    Pnp,
    NodeModules,
    Pnpm,
}

/// Picks yarn 1 or 2+.
///
/// Evidence, strongest first: the declared major version, the lockfile
/// header (`__metadata:` only exists in 2+ lockfiles, `# yarn lockfile v1`
/// only in 1.x), then `.yarnrc.yml` in `root`, which only 2+ reads. Yarn 1
/// is the fallback because a bare global `yarn` is usually 1.x.
pub(crate) fn flavor(
    declared_major: Option<u64>,
    lockfile: Option<&Path>,
    root: &Path,
) -> Result<PackageManager, NpmError> {
    if let Some(major) = declared_major {
        return Ok(if major >= 2 {
            PackageManager::YarnBerry
        } else {
            PackageManager::YarnClassic
        });
    }
    if let Some(manager) = lockfile.map(from_lockfile).transpose()?.flatten() {
        return Ok(manager);
    }
    Ok(if root.join(".yarnrc.yml").is_file() {
        PackageManager::YarnBerry
    } else {
        PackageManager::YarnClassic
    })
}

/// Classifies a `yarn.lock` by its header; `None` when it has neither marker
/// (for example an empty file created by hand).
fn from_lockfile(path: &Path) -> Result<Option<PackageManager>, NpmError> {
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

/// Reads `nodeLinker` from `root/.yarnrc.yml`; Plug'n'Play when unset, as in
/// yarn 2+ itself.
pub fn linker(root: &Path) -> YarnLinker {
    let Ok(text) = std::fs::read_to_string(root.join(".yarnrc.yml")) else {
        return YarnLinker::Pnp;
    };
    let value = text.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix("nodeLinker:")
            .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_owned())
    });
    match value.as_deref() {
        Some("node-modules") => YarnLinker::NodeModules,
        Some("pnpm") => YarnLinker::Pnpm,
        _ => YarnLinker::Pnp,
    }
}

/// A note when Plug'n'Play is on but `.pnp.cjs` has not been generated, which
/// makes `yarn run` fail before the script starts. `shown` is `root` as the
/// note should print it.
pub(crate) fn pnp_note(root: &Path, shown: &str) -> Option<String> {
    (linker(root) == YarnLinker::Pnp && !root.join(".pnp.cjs").is_file()).then(|| {
        let file = if shown == "." {
            ".pnp.cjs".to_owned()
        } else {
            format!("{shown}/.pnp.cjs")
        };
        format!("yarn Plug'n'Play is enabled but {file} is missing; run `yarn install`")
    })
}
