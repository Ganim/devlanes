//! Replacing the hosts file without a reader ever seeing half of it.
//!
//! As in the Unix helper, the new content goes to a temp file next to the hosts
//! file, is flushed, and is renamed over it. The temp file inherits the `etc`
//! directory's access list, which is all a stock hosts file has (it has no
//! explicit entries). A rename over a file another process holds without
//! delete sharing fails with access denied, so the rename waits a moment for
//! the holder.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Attempts when another writer changes the file between our read and our rename.
const ATTEMPTS: usize = 3;

/// `FILE_ATTRIBUTE_REPARSE_POINT`, set on symlinks and junctions.
const REPARSE_POINT: u32 = 0x400;

/// `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS`: open a link
/// itself, never its target, and let a directory open so it is reported.
const OPEN_ENTRY_ITSELF: u32 = 0x0020_0000 | 0x0200_0000;

/// Failure to read or replace the hosts file.
#[derive(Debug)]
pub enum WriteError {
    /// The path is a symlink or junction; writing would replace the link.
    Symlink(PathBuf),
    /// The path exists but is not a regular file.
    NotRegular(PathBuf),
    /// The file has the read-only attribute, which the rename would fail on.
    ReadOnly(PathBuf),
    /// Another writer kept changing the file while we tried to replace it.
    Changed(PathBuf),
    /// An I/O call failed.
    Io {
        /// What the service was doing.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
}

impl WriteError {
    /// Builds a [`WriteError::Io`].
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
                "{} is a symlink or junction; refusing to replace it",
                path.display()
            ),
            Self::NotRegular(path) => write!(f, "{} is not a regular file", path.display()),
            Self::ReadOnly(path) => write!(
                f,
                "{} is marked read-only; clear the attribute (`attrib -r` as administrator) if devlanes may edit it",
                path.display()
            ),
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

/// Reads `path`, refusing a symlink or junction and anything that is not a
/// regular file. The entry itself is opened and checked through its handle, so
/// a swap between the check and the read is caught too.
pub fn read(path: &Path) -> Result<Vec<u8>, WriteError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_ENTRY_ITSELF)
        .open(path)
        .map_err(|error| WriteError::io("opening", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| WriteError::io("reading", path, error))?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(WriteError::Symlink(path.to_owned()));
    }
    if !metadata.is_file() {
        return Err(WriteError::NotRegular(path.to_owned()));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| WriteError::io("reading", path, error))?;
    Ok(bytes)
}

/// Temp file next to `path`, unique per process.
fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "hosts".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.devlanes-{}.tmp", std::process::id()))
}

/// Creates the temp file exclusively; `create_new` also refuses to follow a link.
///
/// A leftover from a crashed run with the same pid is removed once and retried.
fn create_temp(temp: &Path) -> Result<File, WriteError> {
    let open = || OpenOptions::new().write(true).create_new(true).open(temp);
    match open() {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(temp).map_err(|error| WriteError::io("removing stale", temp, error))?;
            open().map_err(|error| WriteError::io("creating", temp, error))
        }
        Err(error) => Err(WriteError::io("creating", temp, error)),
    }
}

/// Writes and flushes the temp file, then renames it over `path`.
///
/// Right before the rename the file is read again; if it no longer matches
/// `original`, someone else wrote it and [`WriteError::Changed`] is returned so
/// the caller can recompute from the new content.
pub fn replace(path: &Path, original: &[u8], bytes: &[u8]) -> Result<(), WriteError> {
    // A rename over a read-only file fails with a plain access denied.
    let metadata =
        fs::symlink_metadata(path).map_err(|error| WriteError::io("reading", path, error))?;
    if metadata.permissions().readonly() {
        return Err(WriteError::ReadOnly(path.to_owned()));
    }
    let temp = temp_path(path);
    let file = create_temp(&temp)?;
    let result = (|| {
        let mut file = file;
        file.write_all(bytes)
            .map_err(|error| WriteError::io("writing", &temp, error))?;
        // FlushFileBuffers: the content is on disk before it gets the hosts name.
        file.sync_all()
            .map_err(|error| WriteError::io("flushing", &temp, error))?;
        drop(file);
        if read(path)? != original {
            return Err(WriteError::Changed(path.to_owned()));
        }
        rename_over(&temp, path).map_err(|error| WriteError::io("renaming over", path, error))
    })();
    if result.is_err() {
        // Best effort: a leftover temp file is harmless and replaced next run.
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Renames `temp` over `path`, waiting about 1.5 s in total while another
/// program holds the hosts file without delete sharing. Defender and the DNS
/// Client read it often, and a failed write is only retried when the names
/// change again. The read-only case was refused before, so access denied here
/// means a holder.
fn rename_over(temp: &Path, path: &Path) -> io::Result<()> {
    const HELD_MS: [u64; 6] = [50, 100, 200, 300, 400, 500];
    let mut delays = HELD_MS.iter();
    loop {
        match devlanes_sys::rename_retrying(temp, path) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => match delays.next() {
                Some(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                None => return Err(error),
            },
            result => return result,
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
        let next = transform(&original).map_err(UpdateError::Content)?;
        if next == original {
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

    /// The names in the directory.
    fn names(dir: &Path) -> Vec<std::ffi::OsString> {
        fs::read_dir(dir)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect()
    }

    #[test]
    fn replaces_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"127.0.0.1 localhost\r\n").expect("seed");
        let before = devlanes_sys::path_identity(&hosts).expect("identity");
        let outcome = update(&hosts, append(b"x\r\n")).expect("update");
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(
            fs::read(&hosts).expect("read"),
            b"127.0.0.1 localhost\r\nx\r\n"
        );
        assert_ne!(
            devlanes_sys::path_identity(&hosts).expect("identity"),
            before,
            "replaced by rename, not rewritten"
        );
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    #[test]
    fn identical_content_is_not_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let before = devlanes_sys::path_identity(&hosts).expect("identity");
        let outcome = update(&hosts, |bytes| Ok::<_, Infallible>(bytes.to_vec())).expect("update");
        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(
            devlanes_sys::path_identity(&hosts).expect("identity"),
            before
        );
    }

    /// A junction named `hosts` is refused, never followed; junctions need no
    /// privilege to create, unlike file symlinks.
    #[test]
    fn links_and_directories_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real dir");
        let link = dir.path().join("hosts");
        let status = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("mklink");
        assert!(status.success(), "mklink /J");
        assert!(matches!(
            update(&link, append(b"b\n")),
            Err(UpdateError::Write(WriteError::Symlink(_)))
        ));
        assert!(matches!(
            update(&real, append(b"b\n")),
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
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    /// A reader holding the file without delete sharing makes the rename fail
    /// with access denied; the file and its directory are left as they were,
    /// and the next update works once the reader lets go.
    #[test]
    fn a_held_file_fails_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        // Read sharing only: nobody may rename over the file meanwhile.
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&hosts)
            .expect("hold");
        assert!(matches!(
            update(&hosts, append(b"b\n")),
            Err(UpdateError::Write(WriteError::Io { .. }))
        ));
        drop(held);
        assert_eq!(fs::read(&hosts).expect("read"), b"a\n");
        assert_eq!(names(dir.path()), ["hosts"]);
        update(&hosts, append(b"b\n")).expect("update after release");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
    }

    /// A reader that lets go within the wait does not fail the write.
    #[test]
    fn a_briefly_held_file_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&hosts)
            .expect("hold");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        update(&hosts, append(b"b\n")).expect("update once released");
        release.join().expect("release");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    /// The read-only attribute gets its own error, not an access denied.
    #[test]
    fn a_read_only_file_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let mut permissions = fs::metadata(&hosts).expect("metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&hosts, permissions.clone()).expect("read-only");
        let result = update(&hosts, append(b"b\n"));
        #[expect(clippy::permissions_set_readonly_false, reason = "Windows attribute")]
        permissions.set_readonly(false);
        fs::set_permissions(&hosts, permissions).expect("writable again");
        let error = match result {
            Err(UpdateError::Write(error @ WriteError::ReadOnly(_))) => error,
            other => panic!("{other:?}"),
        };
        assert!(error.to_string().contains("read-only"), "{error}");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\n");
        assert_eq!(names(dir.path()), ["hosts"]);
    }
}
