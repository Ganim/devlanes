//! The install location check: the service binary, its directory and
//! `%ProgramFiles%` must be out of reach of anyone but administrators, and the
//! directory must hold nothing but the service's own files, or whoever could
//! change them would run code as SYSTEM. The counterpart of the helper's
//! "root-owned, not group or world writable" check.
//!
//! The installer runs it before it starts the service; that is the check that
//! protects. The service runs it again on every start, which only catches a
//! later misconfiguration (an access list changed by an administrator): code
//! able to change the binary would not run this check at all. The hosts file's
//! own directory gets the same check before each write ([`check_hosts_directory`]).

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
    /// The install directory holds something besides the service's own files.
    #[error(
        "{} is not part of the devlanes hosts service; an administrator must remove it",
        .0.display()
    )]
    UnknownEntry(PathBuf),
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

/// Checks that `directory` holds only [`config::INSTALL_ENTRIES`]. A planted DLL
/// there would be loaded by the service ahead of the system's own copy.
pub fn check_contents(directory: &Path) -> Result<(), CheckError> {
    let io = |source| CheckError::Io {
        path: directory.to_owned(),
        source,
    };
    for entry in std::fs::read_dir(directory).map_err(io)? {
        let name = entry.map_err(io)?.file_name();
        let known = name
            .to_str()
            .is_some_and(|name| config::INSTALL_ENTRIES.contains(&name));
        if !known {
            return Err(CheckError::UnknownEntry(directory.join(name)));
        }
    }
    Ok(())
}

/// Checks the program directory before anything is put there: `%ProgramFiles%`
/// and `directory` admin-only, and nothing unknown in `directory`.
pub fn check_install_directory(directory: &Path) -> Result<(), CheckError> {
    let program_files =
        devlanes_sys::windows::dirs::program_files().map_err(|source| CheckError::Io {
            path: PathBuf::from("%ProgramFiles%"),
            source,
        })?;
    admin_only(&program_files, true)?;
    admin_only(directory, true)?;
    check_contents(directory)
}

/// Checks that `exe` is the installed service binary in `%ProgramFiles%\devlanes`
/// and that it, its directory and `%ProgramFiles%` are admin-only, with nothing
/// unknown next to it.
pub fn check_install(exe: &Path) -> Result<(), CheckError> {
    let directory = config::install_dir()?;
    let expected = directory.join(config::SERVICE_BINARY);
    if !same_path(exe, &expected) {
        return Err(CheckError::Location {
            found: exe.display().to_string(),
            expected: expected.display().to_string(),
        });
    }
    check_install_directory(&directory)?;
    admin_only(exe, false)
}

/// Checks that the directory of `hosts` is admin-only, so nobody else can swap
/// the file, add hard links or plant names the service would trust.
pub fn check_hosts_directory(hosts: &Path) -> Result<(), CheckError> {
    let directory = hosts.parent().unwrap_or(hosts);
    admin_only(directory, true)
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

    /// The service's three names are accepted; a DLL or a directory is not.
    #[test]
    fn unknown_entries_in_the_install_directory_are_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        for name in config::INSTALL_ENTRIES {
            std::fs::write(temp.path().join(name), "MZ").expect("entry");
        }
        check_contents(temp.path()).expect("known entries");
        std::fs::write(temp.path().join("version.dll"), "MZ").expect("dll");
        assert!(matches!(
            check_contents(temp.path()),
            Err(CheckError::UnknownEntry(path)) if path.ends_with("version.dll")
        ));
        std::fs::remove_file(temp.path().join("version.dll")).expect("remove");
        std::fs::create_dir(temp.path().join("sub")).expect("dir");
        assert!(matches!(
            check_contents(temp.path()),
            Err(CheckError::UnknownEntry(_))
        ));
    }

    /// Read-only: this machine's `%ProgramFiles%` and hosts file directory pass,
    /// so the service does not refuse a stock system.
    #[test]
    fn stock_system_directories_pass() {
        let program_files = devlanes_sys::windows::dirs::program_files().expect("program files");
        admin_only(&program_files, true).expect("program files is admin-only");
        let hosts = config::hosts_path().expect("hosts path");
        check_hosts_directory(&hosts).expect("etc is admin-only");
    }
}
