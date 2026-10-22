//! Permission bits per OS. Unix syncs the bits in [`MODE_MASK`]. Windows has
//! only the read-only attribute: it reports `0o444` or `0o644`, sets read-only
//! when a mode has no write bits, and ignores exec bits.
//!
//! [`MODE_MASK`]: crate::MODE_MASK

#[cfg(unix)]
pub(crate) use unix::{mode_of, persist_with_mode};
#[cfg(windows)]
pub(crate) use windows::{mode_of, persist_with_mode};

/// Whether a [`LocalTarget`](crate::LocalTarget) on this OS keeps only the
/// read-only attribute instead of permission bits. A remote source then compares
/// modes as [`SyncOptions::read_only_modes`](crate::SyncOptions) describes, so a
/// `0o755` file is not resent to a target that reports `0o644`.
#[cfg(unix)]
pub const READ_ONLY_MODES: bool = false;
/// Whether a [`LocalTarget`](crate::LocalTarget) on this OS keeps only the
/// read-only attribute instead of permission bits. A remote source then compares
/// modes as [`SyncOptions::read_only_modes`](crate::SyncOptions) describes, so a
/// `0o755` file is not resent to a target that reports `0o644`.
#[cfg(windows)]
pub const READ_ONLY_MODES: bool = true;

/// Any write bit in a mode.
const WRITE_BITS: u32 = 0o222;

/// Whether a source file's `mode` matches the `held` mode a target reported.
/// Exact, unless the target keeps only a read-only attribute (`read_only`):
/// then only "has some write bit" has to agree.
pub(crate) fn modes_match(mode: u32, held: u32, read_only: bool) -> bool {
    if read_only {
        (mode & WRITE_BITS == 0) == (held & WRITE_BITS == 0)
    } else {
        mode == held
    }
}

#[cfg(unix)]
mod unix {
    use std::fs::{Metadata, Permissions};
    use std::io;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use tempfile::NamedTempFile;

    use crate::target::MODE_MASK;

    /// Permission bits of `meta`, without setuid, setgid and sticky.
    pub(crate) fn mode_of(meta: &Metadata) -> u32 {
        meta.permissions().mode() & MODE_MASK
    }

    /// Sets the permission bits of `mode` on `temp`, then renames it over `dest`.
    pub(crate) fn persist_with_mode(temp: NamedTempFile, dest: &Path, mode: u32) -> io::Result<()> {
        temp.as_file()
            .set_permissions(Permissions::from_mode(mode & MODE_MASK))?;
        temp.persist(dest).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::{self, Metadata};
    use std::io;
    use std::path::Path;

    use tempfile::NamedTempFile;

    use super::WRITE_BITS;

    /// `0o444` for read-only files, `0o644` for the rest.
    pub(crate) fn mode_of(meta: &Metadata) -> u32 {
        if meta.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }

    /// Renames `temp` over `dest`, then marks `dest` read-only when `mode` has
    /// no write bits. The attribute is set after the rename because tempfile
    /// resets attributes when persisting and Windows refuses to replace a
    /// read-only file, so an existing read-only `dest` is made writable first.
    pub(crate) fn persist_with_mode(temp: NamedTempFile, dest: &Path, mode: u32) -> io::Result<()> {
        if let Ok(meta) = fs::symlink_metadata(dest)
            && meta.is_file()
            && meta.permissions().readonly()
        {
            set_readonly(dest, meta, false)?;
        }
        temp.persist(dest).map_err(|error| error.error)?;
        if mode & WRITE_BITS == 0 {
            set_readonly(dest, fs::symlink_metadata(dest)?, true)?;
        }
        Ok(())
    }

    /// Sets or clears the read-only attribute of the file at `path`.
    fn set_readonly(path: &Path, meta: Metadata, readonly: bool) -> io::Result<()> {
        let mut permissions = meta.permissions();
        permissions.set_readonly(readonly);
        fs::set_permissions(path, permissions)
    }
}

#[cfg(test)]
mod tests {
    use super::modes_match;

    /// Exact by default; a read-only-attribute target only compares write access.
    #[test]
    fn modes_match_exactly_or_by_write_access() {
        assert!(modes_match(0o755, 0o755, false));
        assert!(!modes_match(0o755, 0o644, false));
        assert!(modes_match(0o755, 0o644, true));
        assert!(modes_match(0o600, 0o644, true));
        assert!(modes_match(0o555, 0o444, true));
        assert!(!modes_match(0o444, 0o644, true));
        assert!(!modes_match(0o644, 0o444, true));
    }
}
