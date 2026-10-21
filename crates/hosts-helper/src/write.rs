//! Atomic replacement of the hosts file.
//!
//! A reader (mDNSResponder, glibc, systemd-resolved) must never see a half-written
//! file, so the new content goes to a temp file in the same directory, is synced, and
//! is renamed over the original. The temp file gets the original's mode and owner
//! before the rename.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// Attempts when another writer changes the file between our read and our rename.
const ATTEMPTS: usize = 3;

/// Failure to read or replace the hosts file.
#[derive(Debug)]
pub enum WriteError {
    /// The path is a symlink; writing would replace the link instead of its target.
    Symlink(PathBuf),
    /// The path exists but is not a regular file.
    NotRegular(PathBuf),
    /// Another writer kept changing the file while we tried to replace it.
    Changed(PathBuf),
    /// An I/O call failed.
    Io {
        /// What the helper was doing.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
}

impl WriteError {
    /// Builds an [`WriteError::Io`].
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

impl fmt::Display for WriteError {
    /// Message naming the path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink(path) => write!(
                f,
                "{} is a symlink; refusing to replace it (point the link's target at a regular file or remove the link)",
                path.display()
            ),
            Self::NotRegular(path) => write!(f, "{} is not a regular file", path.display()),
            Self::Changed(path) => write!(
                f,
                "{} kept changing while it was being updated; try again",
                path.display()
            ),
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "{action} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for WriteError {
    /// The OS error for [`WriteError::Io`].
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The file as read, with the attributes the replacement must keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    /// Full content.
    pub bytes: Vec<u8>,
    /// Permission bits, including setuid/setgid/sticky.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
}

/// Reads `path`, refusing symlinks and anything that is not a regular file.
///
/// The `lstat` result is compared with the opened file's device and inode so a
/// swap between the check and the open is caught too.
pub fn read(path: &Path) -> Result<Original, WriteError> {
    let link =
        fs::symlink_metadata(path).map_err(|error| WriteError::io("reading", path, error))?;
    if link.file_type().is_symlink() {
        return Err(WriteError::Symlink(path.to_owned()));
    }
    if !link.is_file() {
        return Err(WriteError::NotRegular(path.to_owned()));
    }
    let mut file = File::open(path).map_err(|error| WriteError::io("opening", path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| WriteError::io("reading", path, error))?;
    if opened.dev() != link.dev() || opened.ino() != link.ino() {
        return Err(WriteError::Changed(path.to_owned()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| WriteError::io("reading", path, error))?;
    Ok(Original {
        bytes,
        mode: link.permissions().mode() & 0o7777,
        uid: link.uid(),
        gid: link.gid(),
    })
}

/// Temp file next to `path`, unique per process so concurrent helpers never share one.
fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "hosts".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.devlanes-{}.tmp", std::process::id()))
}

/// Creates the temp file exclusively; `create_new` also refuses to follow a symlink.
///
/// A leftover from a crashed run with the same pid is removed once and retried.
fn create_temp(temp: &Path) -> Result<File, WriteError> {
    let open = || {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temp)
    };
    match open() {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(temp).map_err(|error| WriteError::io("removing stale", temp, error))?;
            open().map_err(|error| WriteError::io("creating", temp, error))
        }
        Err(error) => Err(WriteError::io("creating", temp, error)),
    }
}

/// Writes, syncs and renames the temp file over `path`, keeping `original`'s attributes.
///
/// Right before the rename the file is read again; if it no longer matches
/// `original.bytes`, someone else wrote it and [`WriteError::Changed`] is returned
/// so the caller can recompute from the new content.
pub fn replace(path: &Path, original: &Original, bytes: &[u8]) -> Result<(), WriteError> {
    let temp = temp_path(path);
    let file = create_temp(&temp)?;
    let result = (|| {
        let mut file = file;
        file.write_all(bytes)
            .map_err(|error| WriteError::io("writing", &temp, error))?;
        // Owner first: chown clears setuid/setgid bits, so the mode must come after.
        std::os::unix::fs::fchown(&file, Some(original.uid), Some(original.gid))
            .map_err(|error| WriteError::io("setting the owner of", &temp, error))?;
        file.set_permissions(fs::Permissions::from_mode(original.mode))
            .map_err(|error| WriteError::io("setting the mode of", &temp, error))?;
        file.sync_all()
            .map_err(|error| WriteError::io("syncing", &temp, error))?;
        drop(file);
        let current = read(path)?;
        if current.bytes != original.bytes {
            return Err(WriteError::Changed(path.to_owned()));
        }
        fs::rename(&temp, path).map_err(|error| WriteError::io("renaming over", path, error))
    })();
    if result.is_err() {
        // Best effort: a leftover dotfile in /etc is harmless and replaced next run.
        let _ = fs::remove_file(&temp);
        return result;
    }
    sync_parent(path);
    Ok(())
}

/// Syncs the directory so the rename survives a crash.
///
/// Best effort: the new content is already durable in its own inode, and some
/// filesystems refuse to sync a directory handle.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// Result of [`update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The file was replaced.
    Changed,
    /// The new content equals the old; nothing was written.
    Unchanged,
}

/// Failure of [`update`]: either the transform refused the content or the write failed.
#[derive(Debug)]
pub enum UpdateError<E> {
    /// The transform refused the current content.
    Content(E),
    /// Reading or replacing failed.
    Write(WriteError),
}

/// Reads `path`, applies `transform`, and replaces the file when the result differs.
///
/// Retries from a fresh read when another writer changes the file mid-update.
pub fn update<E>(
    path: &Path,
    transform: impl Fn(&[u8]) -> Result<Vec<u8>, E>,
) -> Result<Outcome, UpdateError<E>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let original = read(path).map_err(UpdateError::Write)?;
        let next = transform(&original.bytes).map_err(UpdateError::Content)?;
        if next == original.bytes {
            return Ok(Outcome::Unchanged);
        }
        match replace(path, &original, &next) {
            Ok(()) => return Ok(Outcome::Changed),
            Err(WriteError::Changed(_)) if attempt < ATTEMPTS => {}
            Err(error) => return Err(UpdateError::Write(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;

    fn append(suffix: &'static [u8]) -> impl Fn(&[u8]) -> Result<Vec<u8>, Infallible> {
        move |bytes| {
            let mut out = bytes.to_vec();
            out.extend_from_slice(suffix);
            Ok(out)
        }
    }

    #[test]
    fn replaces_atomically_and_keeps_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"127.0.0.1 localhost\n").expect("seed");
        fs::set_permissions(&hosts, fs::Permissions::from_mode(0o644)).expect("chmod");
        let before = fs::metadata(&hosts).expect("stat");

        let outcome = update(&hosts, append(b"x\n")).expect("update");
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(fs::read(&hosts).expect("read"), b"127.0.0.1 localhost\nx\n");
        let after = fs::metadata(&hosts).expect("stat");
        assert_eq!(after.permissions().mode() & 0o7777, 0o644);
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert_ne!(
            after.ino(),
            before.ino(),
            "replaced by rename, not rewritten"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, ["hosts"]);
    }

    #[test]
    fn unusual_modes_survive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        fs::set_permissions(&hosts, fs::Permissions::from_mode(0o604)).expect("chmod");
        update(&hosts, append(b"b\n")).expect("update");
        let mode = fs::metadata(&hosts).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(mode, 0o604);
    }

    #[test]
    fn identical_content_is_not_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let before = fs::metadata(&hosts).expect("stat").ino();
        let outcome = update(&hosts, |bytes| Ok::<_, Infallible>(bytes.to_vec())).expect("update");
        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(fs::metadata(&hosts).expect("stat").ino(), before);
    }

    #[test]
    fn symlinks_and_directories_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real");
        fs::write(&target, b"a\n").expect("seed");
        let link = dir.path().join("hosts");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(matches!(
            update(&link, append(b"b\n")),
            Err(UpdateError::Write(WriteError::Symlink(_)))
        ));
        assert_eq!(fs::read(&target).expect("read"), b"a\n");
        assert!(
            fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink()
        );
        assert!(matches!(
            update(dir.path(), append(b"b\n")),
            Err(UpdateError::Write(WriteError::NotRegular(_)))
        ));
        assert!(matches!(
            update(&dir.path().join("missing"), append(b"b\n")),
            Err(UpdateError::Write(WriteError::Io { .. }))
        ));
    }

    #[test]
    fn content_errors_leave_the_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let result = update(&hosts, |_| Err::<Vec<u8>, _>("refused"));
        assert!(matches!(result, Err(UpdateError::Content("refused"))));
        assert_eq!(fs::read(&hosts).expect("read"), b"a\n");
    }

    #[test]
    fn concurrent_writer_triggers_a_fresh_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let calls = Cell::new(0);
        let outcome = update(&hosts, |bytes| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                // Another writer edits the file after our read, before our rename.
                fs::write(&hosts, b"a\nother\n").map_err(|_| "write")?;
            }
            let mut out = bytes.to_vec();
            out.extend_from_slice(b"ours\n");
            Ok::<_, &str>(out)
        })
        .expect("update");
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(calls.get(), 2);
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nother\nours\n");
    }

    #[test]
    fn a_stale_temp_file_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        fs::write(temp_path(&hosts), b"stale").expect("stale temp");
        update(&hosts, append(b"b\n")).expect("update");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert!(!temp_path(&hosts).exists());
    }
}
