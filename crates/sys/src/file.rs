//! Stable file identities, for noticing that a path now names a different file.

use std::fs::File;
use std::io;
use std::path::Path;

/// Names one file on one machine: device and inode on Unix, volume serial number
/// and file index on Windows. Compare values for equality only.
pub type FileIdentity = (u64, u64);

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

    pub(super) fn file(file: &File) -> io::Result<FileIdentity> {
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    pub(super) fn path(path: &Path) -> io::Result<FileIdentity> {
        let metadata = fs::symlink_metadata(path)?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    use super::FileIdentity;

    pub(super) fn file(file: &File) -> io::Result<FileIdentity> {
        crate::os::file_identity(file)
    }

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
}
