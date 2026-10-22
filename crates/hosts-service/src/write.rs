//! Replacing the hosts file without a reader ever seeing half of it.
//!
//! The Unix helper writes a temp file and renames it over `/etc/hosts`. On Windows
//! a rename would give the hosts file the temp file's inherited access list, so
//! the temp file replaces it with `ReplaceFileW`, which keeps the hosts file's
//! access list and attributes. Antivirus scanners and editors briefly hold the
//! file without delete sharing, so the replace is retried with backoff.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use devlanes_sys::windows::replace::{self, replace_file};

/// Attempts when another writer changes the file between our read and our replace.
const ATTEMPTS: usize = 3;

/// Pauses between replace attempts while the file is held open by someone else.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// Largest hosts file read: far above any real one, small enough for a service.
const MAX_FILE_BYTES: u64 = 64 << 20;

/// `FILE_ATTRIBUTE_REPARSE_POINT`, set on symlinks and junctions.
const REPARSE_POINT: u32 = 0x400;

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself, never its target.
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// `ERROR_SHARING_VIOLATION`, `ERROR_LOCK_VIOLATION` and `ERROR_ACCESS_DENIED`:
/// another process (antivirus, an editor) holds the file for the moment.
const BUSY: [i32; 3] = [32, 33, 5];

/// Failure to read or replace the hosts file.
#[derive(Debug)]
pub enum WriteError {
    /// The path or its directory is a symlink or junction.
    ReparsePoint(PathBuf),
    /// The path exists but is not a regular file.
    NotRegular(PathBuf),
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
            Self::ReparsePoint(path) => write!(
                f,
                "{} is a symlink or junction; refusing to replace it",
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

/// Reads `path`, refusing a link (of the file or its directory) and anything
/// that is not a regular file.
///
/// The file itself is opened, never a link's target, and the opened handle is
/// checked again, so a swap between the check and the open is caught too.
pub fn read(path: &Path) -> Result<Vec<u8>, WriteError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let metadata = fs::symlink_metadata(parent)
            .map_err(|error| WriteError::io("reading", parent, error))?;
        if metadata.file_attributes() & REPARSE_POINT != 0 {
            return Err(WriteError::ReparsePoint(parent.to_owned()));
        }
    }
    let link =
        fs::symlink_metadata(path).map_err(|error| WriteError::io("reading", path, error))?;
    if link.file_attributes() & REPARSE_POINT != 0 {
        return Err(WriteError::ReparsePoint(path.to_owned()));
    }
    if !link.is_file() {
        return Err(WriteError::NotRegular(path.to_owned()));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| WriteError::io("opening", path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| WriteError::io("reading", path, error))?;
    if opened.file_attributes() & REPARSE_POINT != 0 {
        return Err(WriteError::ReparsePoint(path.to_owned()));
    }
    if !opened.is_file() {
        return Err(WriteError::NotRegular(path.to_owned()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| WriteError::io("reading", path, error))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(WriteError::io(
            "reading",
            path,
            io::Error::other(format!("the file is larger than {MAX_FILE_BYTES} bytes")),
        ));
    }
    Ok(bytes)
}

/// `.<name>.devlanes-<pid>.<suffix>` next to `path`, unique per process.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "hosts".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.devlanes-{}.{suffix}", std::process::id()))
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

/// Writes and flushes a temp file, then replaces `path` with it, keeping the
/// hosts file's access list.
///
/// Right before the replace the file is read again; if it no longer matches
/// `original`, someone else wrote it and [`WriteError::Changed`] is returned so
/// the caller can recompute from the new content.
pub fn replace(path: &Path, original: &[u8], bytes: &[u8]) -> Result<(), WriteError> {
    let temp = sibling(path, "tmp");
    let backup = sibling(path, "bak");
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
        replace_retrying(path, &temp, &backup)
    })();
    // Best effort: a leftover dotfile is harmless and replaced on the next run.
    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(&backup);
    result
}

/// `ReplaceFileW` with backoff while the file is busy. The old content passes
/// through `backup`; a failure that leaves the hosts file under that name is put
/// back before anything else happens.
fn replace_retrying(path: &Path, temp: &Path, backup: &Path) -> Result<(), WriteError> {
    let _ = fs::remove_file(backup);
    let mut delays = BACKOFF.iter();
    loop {
        let error = match replace_file(path, temp, backup) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let code = error.raw_os_error();
        if code == Some(replace::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2) {
            // The hosts file now has the backup name; restore it first.
            devlanes_sys::rename_retrying(backup, path)
                .map_err(|error| WriteError::io("restoring", path, error))?;
        }
        let retryable = code.is_some_and(|code| {
            BUSY.contains(&code)
                || [
                    replace::ERROR_UNABLE_TO_REMOVE_REPLACED,
                    replace::ERROR_UNABLE_TO_MOVE_REPLACEMENT,
                    replace::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
                ]
                .contains(&code)
        });
        match delays.next() {
            Some(delay) if retryable => std::thread::sleep(*delay),
            _ => return Err(WriteError::io("replacing", path, error)),
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

    /// Only the hosts file is left in the directory.
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
        let outcome = update(&hosts, append(b"x\r\n")).expect("update");
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(
            fs::read(&hosts).expect("read"),
            b"127.0.0.1 localhost\r\nx\r\n"
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

    #[test]
    fn junctions_and_directories_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real dir");
        fs::write(real.join("hosts"), b"a\n").expect("seed");
        let link = dir.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("mklink");
        assert!(status.success(), "mklink /J");
        assert!(matches!(
            update(&link.join("hosts"), append(b"b\n")),
            Err(UpdateError::Write(WriteError::ReparsePoint(_)))
        ));
        assert_eq!(fs::read(real.join("hosts")).expect("read"), b"a\n");
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
                // Another writer edits the file after our read, before our replace.
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
    fn stale_temp_and_backup_files_are_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        fs::write(sibling(&hosts, "tmp"), b"stale").expect("stale temp");
        fs::write(sibling(&hosts, "bak"), b"stale").expect("stale backup");
        update(&hosts, append(b"b\n")).expect("update");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    /// A reader holding the file without delete sharing (as an editor or a
    /// scanner does) delays the replace; once it lets go, the replace succeeds.
    #[test]
    fn a_briefly_held_file_is_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        // Read sharing only: ReplaceFileW cannot rename the file away meanwhile.
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&hosts)
            .expect("hold");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
        });
        let outcome = update(&hosts, append(b"b\n")).expect("update after release");
        releaser.join().expect("releaser");
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert_eq!(names(dir.path()), ["hosts"]);
    }
}
