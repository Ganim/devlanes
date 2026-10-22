//! The install location check: the service binary and its directory must be
//! out of reach of anyone but administrators, or whoever could change them
//! would run code as SYSTEM. The counterpart of the helper's "root-owned, not
//! group or world writable" check; the installer runs it before starting the
//! service and the service on every start.

use std::io;
use std::path::{Path, PathBuf};

use devlanes_sys::windows::security::{AdminOnlyProblem, check_admin_only};

use crate::config::{self, ConfigError};

/// Why the service refuses its install location.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// The binary is not `%ProgramFiles%\devlanes\devlanes-hosts-service.exe`.
    #[error("the service binary {found} is not {expected}")]
    Location {
        /// Where the binary runs from.
        found: String,
        /// Where it must be.
        expected: String,
    },
    /// The binary or its directory could be changed by others.
    #[error("{} {problem}", path.display())]
    Unsafe {
        /// The file or directory.
        path: PathBuf,
        /// What is wrong.
        problem: AdminOnlyProblem,
    },
    /// The check itself failed.
    #[error("checking {}: {source}", path.display())]
    Io {
        /// The path checked.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The install directory could not be found.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Checks `directory` (when `is_dir`) or a file with [`check_admin_only`].
pub fn admin_only(path: &Path, is_dir: bool) -> Result<(), CheckError> {
    match check_admin_only(path, is_dir) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(problem)) => Err(CheckError::Unsafe {
            path: path.to_owned(),
            problem,
        }),
        Err(source) => Err(CheckError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Whether `a` and `b` name the same path, ignoring case and `\\?\` prefixes.
fn same_path(a: &Path, b: &Path) -> bool {
    let normal = |path: &Path| {
        dunce_like(path)
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_lowercase()
    };
    normal(a) == normal(b)
}

/// `path` without a verbatim `\\?\` prefix, which `current_exe` may report.
fn dunce_like(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_owned(),
    }
}

/// Checks that `exe` is the installed service binary in `%ProgramFiles%\devlanes`
/// and that both are admin-only.
pub fn check_install(exe: &Path) -> Result<(), CheckError> {
    let directory = config::install_dir()?;
    let expected = directory.join(config::SERVICE_BINARY);
    if !same_path(exe, &expected) {
        return Err(CheckError::Location {
            found: exe.display().to_string(),
            expected: expected.display().to_string(),
        });
    }
    admin_only(&directory, true)?;
    admin_only(exe, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_without_case_or_prefix() {
        assert!(same_path(
            Path::new(r"\\?\C:\Program Files\devlanes\x.exe"),
            Path::new(r"c:\program files\DEVLANES\x.exe")
        ));
        assert!(!same_path(
            Path::new(r"C:\Program Files\devlanes\x.exe"),
            Path::new(r"C:\Users\a\devlanes\x.exe")
        ));
    }

    /// A test binary runs from `target\`, which is neither the install location
    /// nor admin-only.
    #[test]
    fn the_test_binary_is_refused() {
        let exe = std::env::current_exe().expect("exe");
        assert!(matches!(
            check_install(&exe),
            Err(CheckError::Location { .. })
        ));
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            admin_only(temp.path(), true),
            Err(CheckError::Unsafe {
                problem: AdminOnlyProblem::Owner(_),
                ..
            })
        ));
    }
}
