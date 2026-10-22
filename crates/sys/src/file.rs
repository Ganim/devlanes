//! Stable file identities, for noticing that a path now names a different file,
//! and renames and removals that ride out Windows sharing violations.

use std::fs::File;
use std::io;
use std::path::Path;

/// `std::fs::rename`, retried for up to about 0.6 s while another process
/// holds `from` or `to` open without delete sharing (an antivirus scan, an
/// editor, `Get-Content -Wait`). Access denied is retried once, for a file
/// whose deletion is still pending. A plain rename on Unix. Blocking: it sleeps
/// between attempts.
pub fn rename_retrying(from: &Path, to: &Path) -> io::Result<()> {
    retrying(|| std::fs::rename(from, to))
}

/// `std::fs::remove_file`, retried like [`rename_retrying`]. A plain removal on
/// Unix.
pub fn remove_file_retrying(path: &Path) -> io::Result<()> {
    retrying(|| std::fs::remove_file(path))
}

/// Runs `operation` until it succeeds, fails with an error other than a
/// Windows sharing violation, or the retries run out; access denied is retried
/// at most once. On Unix it runs once. For file operations std has no wrapper
/// for here, such as a tempfile persist. Blocking: it sleeps between attempts.
pub fn retrying<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut delays = imp::RETRY_DELAYS.iter();
    let mut denied_retried = false;
    loop {
        match operation() {
            Err(error) if imp::is_transient(&error, &mut denied_retried) => match delays.next() {
                Some(delay) => std::thread::sleep(*delay),
                None => return Err(error),
            },
            result => return result,
        }
    }
}

/// Names one file on one machine: device and inode on Unix, volume serial number
/// and 128-bit file id on Windows (ReFS, used by Dev Drives, has ids that 64 bits
/// cannot hold). Opaque: compare values for equality only. The default value names
/// no file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    volume: u64,
    file: u128,
}

impl FileIdentity {
    /// Built by the platform modules from what the OS reports.
    pub(crate) fn new(volume: u64, file: u128) -> Self {
        Self { volume, file }
    }
}

/// Identity of an open file. Log followers compare it across reads to detect rotation.
pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
    imp::file(file)
}

/// Identity of `path` itself, without following a final symlink or junction.
/// Directories are supported.
pub fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    imp::path(path)
}

#[cfg(unix)]
mod imp {
    use std::fs::{self, File};
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::FileIdentity;

    /// Unix renames and removals do not fail because a file is open, so there
    /// is nothing to wait for.
    pub(super) const RETRY_DELAYS: [std::time::Duration; 0] = [];

    /// No error is worth retrying on Unix.
    pub(super) fn is_transient(_error: &io::Error, _denied_retried: &mut bool) -> bool {
        false
    }

    /// Device and inode of an open file.
    pub(super) fn file(file: &File) -> io::Result<FileIdentity> {
        let metadata = file.metadata()?;
        Ok(FileIdentity::new(
            metadata.dev(),
            u128::from(metadata.ino()),
        ))
    }

    /// Device and inode of `path` itself (`lstat`).
    pub(super) fn path(path: &Path) -> io::Result<FileIdentity> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(FileIdentity::new(
            metadata.dev(),
            u128::from(metadata.ino()),
        ))
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    use super::FileIdentity;

    /// Waits between attempts, about 0.6 s in total: scanners and indexers
    /// usually release a fresh file within tens of milliseconds, and a holder
    /// that keeps it longer is not going away soon.
    pub(super) const RETRY_DELAYS: [Duration; 6] = [
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::from_millis(40),
        Duration::from_millis(80),
        Duration::from_millis(160),
        Duration::from_millis(320),
    ];

    /// Errors another process's open handle causes. A file whose deletion is
    /// still pending reports access denied, but so does a real permission
    /// problem, which no wait fixes; it gets one retry, recorded in
    /// `denied_retried`.
    pub(super) fn is_transient(error: &io::Error, denied_retried: &mut bool) -> bool {
        match error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok())
        {
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => true,
            Some(ERROR_ACCESS_DENIED) => !std::mem::replace(denied_retried, true),
            _ => false,
        }
    }

    /// Volume serial and file id of an open file.
    pub(super) fn file(file: &File) -> io::Result<FileIdentity> {
        crate::os::file_identity(file)
    }

    /// Volume serial and file id of `path` itself, not what a link points to.
    pub(super) fn path(path: &Path) -> io::Result<FileIdentity> {
        // Access 0 only queries attributes, so files locked by other processes still
        // open. Backup semantics is what allows opening a directory, and the reparse
        // flag keeps a final symlink or junction from being followed.
        let file = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        crate::os::file_identity(&file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same file opened twice has one identity; a different file has another.
    #[test]
    fn identity_is_per_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, "a").expect("write a");
        std::fs::write(&b, "b").expect("write b");
        let first = file_identity(&File::open(&a).expect("open a")).expect("identity a");
        assert_eq!(path_identity(&a).expect("path a"), first);
        assert_ne!(path_identity(&b).expect("path b"), first);
        assert!(
            path_identity(dir.path()).is_ok(),
            "directories have identities"
        );
    }

    /// Without a competing handle the helpers are a plain rename and removal,
    /// and a missing file fails at once.
    #[test]
    fn rename_and_remove_without_contention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        std::fs::write(&from, "x").expect("write");
        std::fs::write(&to, "old").expect("write target");
        rename_retrying(&from, &to).expect("rename over");
        assert_eq!(std::fs::read_to_string(&to).expect("read"), "x");
        remove_file_retrying(&to).expect("remove");
        let started = std::time::Instant::now();
        let missing = remove_file_retrying(&to).expect_err("gone");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    /// Calls `retrying` makes for an operation that always fails with `code`.
    fn attempts(code: i32) -> usize {
        let mut calls = 0;
        let result: io::Result<()> = retrying(|| {
            calls += 1;
            Err(io::Error::from_raw_os_error(code))
        });
        assert_eq!(result.map_err(|e| e.raw_os_error()), Err(Some(code)));
        calls
    }

    /// Access denied gets one retry on Windows (a pending deletion clears
    /// quickly, a real permission problem never does); Unix never retries.
    #[cfg(windows)]
    #[test]
    fn access_denied_is_retried_once() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
        let code = |code: u32| i32::try_from(code).expect("fits");
        assert_eq!(attempts(code(ERROR_ACCESS_DENIED)), 2);
        assert_eq!(attempts(code(ERROR_FILE_NOT_FOUND)), 1);
    }

    /// Unix runs a failing operation once, whatever the error.
    #[cfg(unix)]
    #[test]
    fn unix_never_retries() {
        for code in [
            rustix::io::Errno::ACCESS.raw_os_error(),
            rustix::io::Errno::BUSY.raw_os_error(),
        ] {
            assert_eq!(attempts(code), 1);
        }
    }

    /// A handle without delete sharing blocks the rename until it closes; one
    /// that stays open makes the rename fail after the retries.
    #[cfg(windows)]
    #[test]
    fn rename_waits_for_a_sharing_violation() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("daemon.log");
        let to = dir.path().join("daemon.log.1");
        std::fs::write(&from, "x").expect("write");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&from)
            .expect("open without delete sharing");
        assert_eq!(
            std::fs::rename(&from, &to)
                .expect_err("plain rename fails")
                .raw_os_error(),
            i32::try_from(ERROR_SHARING_VIOLATION).ok()
        );
        let error = rename_retrying(&from, &to).expect_err("still held");
        assert_eq!(
            error.raw_os_error(),
            i32::try_from(ERROR_SHARING_VIOLATION).ok()
        );
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });
        rename_retrying(&from, &to).expect("renamed once released");
        release.join().expect("release thread");
        assert!(to.exists() && !from.exists());
    }
}
