//! Replacing the hosts file without a reader ever seeing half of it, and
//! without ever losing it.
//!
//! The Unix helper writes a temp file and renames it over `/etc/hosts`. On Windows
//! a rename would give the hosts file the temp file's inherited access list, so
//! the temp file replaces it with `ReplaceFileW`, which keeps the hosts file's
//! access list and attributes. Antivirus scanners and editors briefly hold the
//! file without delete sharing, so the replace is retried with backoff.
//!
//! `ReplaceFileW` does not keep the owner: the result is owned by whoever owns
//! the temp file, the service's default owner (SYSTEM or Administrators). Both
//! already have full control through the stock access list, so no account gains
//! anything; a hosts file someone else owned only loses that owner's implicit
//! right to change its access list.
//!
//! During the swap the old content passes through `.hosts.devlanes-<pid>.bak`.
//! That file is deleted only while `hosts` exists, so a failure (or a crash)
//! between the two renames leaves the content there, and the next write or
//! service start puts it back ([`recover`]).

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use devlanes_sys::windows::replace::{self, replace_file};
use devlanes_sys::windows::security::{PlainFileProblem, open_plain_file};

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

/// `FILE_ATTRIBUTE_READONLY`.
const READ_ONLY: u32 = 0x1;

/// `FILE_ATTRIBUTE_REPARSE_POINT`, set on symlinks and junctions.
const REPARSE_POINT: u32 = 0x400;

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
    /// The file has more than one name (hard link).
    HardLinks(PathBuf, u32),
    /// The file has the read-only attribute, which the replace would fail on.
    ReadOnly(PathBuf),
    /// Another writer kept changing the file while we tried to replace it.
    Changed(PathBuf),
    /// A replace failed after moving the hosts file to its backup name, and
    /// moving it back failed too. The content is intact under `backup`, which is
    /// kept; the next write or service start tries again.
    LeftAtBackup {
        /// The hosts file.
        path: PathBuf,
        /// Where its content is.
        backup: PathBuf,
        /// Why it could not be moved back.
        source: io::Error,
    },
    /// The hosts file is missing and several backups could be it.
    Backups {
        /// The hosts file.
        path: PathBuf,
        /// The candidates.
        found: Vec<PathBuf>,
    },
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
            Self::HardLinks(path, links) => write!(
                f,
                "{} has {links} names (hard links); refusing to write it",
                path.display()
            ),
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
            Self::LeftAtBackup {
                path,
                backup,
                source,
            } => write!(
                f,
                "hosts file left at {}: moving it back to {} failed ({source}); the next update retries, or rename it back as administrator",
                backup.display(),
                path.display()
            ),
            Self::Backups { path, found } => {
                let names: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "{} is missing and several devlanes backups could be it ({}); rename the right one to hosts as administrator",
                    path.display(),
                    names.join(", ")
                )
            }
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "{action} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for WriteError {
    /// The OS error for [`WriteError::Io`] and [`WriteError::LeftAtBackup`].
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::LeftAtBackup { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The file operations a replace consists of, so tests can make them fail
/// without touching real system files.
trait Ops {
    /// `ReplaceFileW(path, temp, backup)`.
    fn replace(&self, path: &Path, temp: &Path, backup: &Path) -> io::Result<()>;
    /// A rename that rides out sharing violations.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
}

/// The real operations.
struct System;

impl Ops for System {
    fn replace(&self, path: &Path, temp: &Path, backup: &Path) -> io::Result<()> {
        replace_file(path, temp, backup)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        devlanes_sys::rename_retrying(from, to)
    }
}

/// Reads `path`, refusing a link (of the file or its directory), anything that
/// is not a regular file, and a file with more than one name.
///
/// The file itself is opened, never a link's target, and the checks run on the
/// opened handle, so a swap between the check and the open is caught too.
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
    let file = match open_plain_file(path) {
        Ok(Ok(file)) => file,
        Ok(Err(PlainFileProblem::ReparsePoint)) => {
            return Err(WriteError::ReparsePoint(path.to_owned()));
        }
        Ok(Err(PlainFileProblem::NotAFile)) => return Err(WriteError::NotRegular(path.to_owned())),
        Ok(Err(PlainFileProblem::HardLinks(links))) => {
            return Err(WriteError::HardLinks(path.to_owned(), links));
        }
        Err(error) => return Err(WriteError::io("opening", path, error)),
    };
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

/// `.<name>.devlanes-` for `path`, the start of every temp and backup name.
fn sibling_prefix(path: &Path) -> String {
    let name = path.file_name().map_or_else(
        || "hosts".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!(".{name}.devlanes-")
}

/// `.<name>.devlanes-<pid>.<suffix>` next to `path`, unique per process.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    path.with_file_name(format!(
        "{}{}.{suffix}",
        sibling_prefix(path),
        std::process::id()
    ))
}

/// Temp (`tmp`) or backup (`bak`) files of any process next to `path`.
fn leftovers(path: &Path, suffix: &str) -> io::Result<Vec<PathBuf>> {
    let Some(directory) = path.parent() else {
        return Ok(Vec::new());
    };
    let prefix = sibling_prefix(path);
    let ending = format!(".{suffix}");
    let mut found = Vec::new();
    for entry in fs::read_dir(directory)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(&ending))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()));
        if ours {
            found.push(directory.join(name));
        }
    }
    found.sort();
    Ok(found)
}

/// Puts the hosts file back from a backup left by a failed or interrupted
/// replace, when `path` is missing and exactly one backup exists; returns that
/// backup. With `path` present, leftover temp and backup files are old copies
/// and are removed. Runs before every write and when the service starts.
pub fn recover(path: &Path) -> Result<Option<PathBuf>, WriteError> {
    recover_with(&System, path)
}

/// [`recover`] with `ops`.
fn recover_with(ops: &dyn Ops, path: &Path) -> Result<Option<PathBuf>, WriteError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            // Best effort: a leftover that cannot be removed now is tried again later.
            for suffix in ["tmp", "bak"] {
                for stale in leftovers(path, suffix).unwrap_or_default() {
                    let _ = fs::remove_file(stale);
                }
            }
            Ok(None)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let backups =
                leftovers(path, "bak").map_err(|error| WriteError::io("listing", path, error))?;
            match backups.as_slice() {
                [] => Ok(None),
                [backup] => {
                    ops.rename(backup, path)
                        .map_err(|source| WriteError::LeftAtBackup {
                            path: path.to_owned(),
                            backup: backup.clone(),
                            source,
                        })?;
                    Ok(Some(backup.clone()))
                }
                _ => Err(WriteError::Backups {
                    path: path.to_owned(),
                    found: backups,
                }),
            }
        }
        Err(error) => Err(WriteError::io("reading", path, error)),
    }
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
    replace_with(&System, path, original, bytes)
}

/// [`replace`] with `ops`.
fn replace_with(
    ops: &dyn Ops,
    path: &Path,
    original: &[u8],
    bytes: &[u8],
) -> Result<(), WriteError> {
    // ReplaceFileW fails on a read-only file with a plain access denied, which
    // would be retried and then reported without the reason.
    let attributes = fs::symlink_metadata(path)
        .map_err(|error| WriteError::io("reading", path, error))?
        .file_attributes();
    if attributes & READ_ONLY != 0 {
        return Err(WriteError::ReadOnly(path.to_owned()));
    }
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
        replace_retrying(ops, path, &temp, &backup)
    })();
    // Best effort: a leftover temp file is harmless and removed later.
    let _ = fs::remove_file(&temp);
    // The backup may hold the only copy of the hosts file; it goes only once the
    // hosts file is back under its name.
    if fs::symlink_metadata(path).is_ok() {
        let _ = fs::remove_file(&backup);
    }
    result
}

/// `ReplaceFileW` with backoff while the file is busy. The old content passes
/// through `backup`; a failure that leaves the hosts file under that name is put
/// back before anything else happens, and if that fails too, the backup stays.
fn replace_retrying(
    ops: &dyn Ops,
    path: &Path,
    temp: &Path,
    backup: &Path,
) -> Result<(), WriteError> {
    // The hosts file was just read, so a backup of that name is an old copy.
    let _ = fs::remove_file(backup);
    let mut delays = BACKOFF.iter();
    loop {
        let error = match ops.replace(path, temp, backup) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let code = error.raw_os_error();
        if code == Some(replace::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2) {
            // The hosts file now has the backup name; restore it first.
            ops.rename(backup, path)
                .map_err(|source| WriteError::LeftAtBackup {
                    path: path.to_owned(),
                    backup: backup.to_owned(),
                    source,
                })?;
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

/// Restores a leftover backup ([`recover`]), reads `path`, applies `transform`,
/// and replaces the file when the result differs.
///
/// Retries from a fresh read when another writer changes the file mid-update.
pub fn update<E>(
    path: &Path,
    transform: impl Fn(&[u8]) -> Result<Vec<u8>, E>,
) -> Result<Outcome, UpdateError<E>> {
    update_with(&System, path, transform)
}

/// [`update`] with `ops`.
fn update_with<E>(
    ops: &dyn Ops,
    path: &Path,
    transform: impl Fn(&[u8]) -> Result<Vec<u8>, E>,
) -> Result<Outcome, UpdateError<E>> {
    recover_with(ops, path).map_err(UpdateError::Write)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let original = read(path).map_err(UpdateError::Write)?;
        let next = transform(&original).map_err(UpdateError::Content)?;
        if next == original {
            return Ok(Outcome::Unchanged);
        }
        match replace_with(ops, path, &original, &next) {
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
    use std::os::windows::fs::OpenOptionsExt as _;

    fn append(suffix: &'static [u8]) -> impl Fn(&[u8]) -> Result<Vec<u8>, Infallible> {
        move |bytes| {
            let mut out = bytes.to_vec();
            out.extend_from_slice(suffix);
            Ok(out)
        }
    }

    /// The names in the directory, sorted.
    fn names(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        names
    }

    /// Real operations, except that the first `fail_replaces` replaces behave
    /// like `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (the hosts file is moved to the
    /// backup name and the call fails), and renames fail while `fail_renames`.
    struct Faulty {
        fail_replaces: Cell<usize>,
        fail_renames: bool,
    }

    impl Ops for Faulty {
        fn replace(&self, path: &Path, temp: &Path, backup: &Path) -> io::Result<()> {
            if self.fail_replaces.get() == 0 {
                return System.replace(path, temp, backup);
            }
            self.fail_replaces.set(self.fail_replaces.get() - 1);
            fs::rename(path, backup)?;
            Err(io::Error::from_raw_os_error(
                replace::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
            ))
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.fail_renames {
                return Err(io::Error::other("injected rename failure"));
            }
            System.rename(from, to)
        }
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
    fn junctions_directories_and_hard_links_are_refused() {
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
        fs::hard_link(real.join("hosts"), dir.path().join("elsewhere")).expect("hard link");
        assert!(matches!(
            update(&real.join("hosts"), append(b"b\n")),
            Err(UpdateError::Write(WriteError::HardLinks(_, 2)))
        ));
        assert!(matches!(
            read(&real.join("hosts")),
            Err(WriteError::HardLinks(_, 2))
        ));
        assert_eq!(fs::read(real.join("hosts")).expect("read"), b"a\n");
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

    /// Leftovers of this and other processes go once the hosts file exists.
    #[test]
    fn stale_temp_and_backup_files_are_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        fs::write(sibling(&hosts, "tmp"), b"stale").expect("stale temp");
        fs::write(sibling(&hosts, "bak"), b"stale").expect("stale backup");
        fs::write(dir.path().join(".hosts.devlanes-1.bak"), b"old").expect("other backup");
        fs::write(dir.path().join(".hosts.devlanes-x.bak"), b"not ours").expect("other name");
        update(&hosts, append(b"b\n")).expect("update");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert_eq!(names(dir.path()), [".hosts.devlanes-x.bak", "hosts"]);
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

    /// The replace moved the hosts file to the backup name and failed; the
    /// backup is moved back and the replace retried.
    #[test]
    fn a_failed_final_rename_is_undone_and_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let ops = Faulty {
            fail_replaces: Cell::new(1),
            fail_renames: false,
        };
        let outcome = update_with(&ops, &hosts, append(b"b\n")).expect("update");
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    /// When moving the backup back fails too, the backup is kept and named in
    /// the error; the next update puts it back first.
    #[test]
    fn a_failed_restore_keeps_the_backup_until_the_next_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, b"a\n").expect("seed");
        let ops = Faulty {
            fail_replaces: Cell::new(1),
            fail_renames: true,
        };
        let error = update_with(&ops, &hosts, append(b"b\n")).expect_err("left at backup");
        let backup = sibling(&hosts, "bak");
        match &error {
            UpdateError::Write(WriteError::LeftAtBackup { backup: left, .. }) => {
                assert_eq!(left, &backup);
            }
            other => panic!("{other:?}"),
        }
        if let UpdateError::Write(error) = &error {
            assert!(
                error.to_string().starts_with("hosts file left at "),
                "{error}"
            );
        }
        assert!(!hosts.exists());
        assert_eq!(fs::read(&backup).expect("backup kept"), b"a\n");
        assert_eq!(names(dir.path()), [backup.file_name().expect("name")]);

        // Restoring through the seam still fails: the backup stays put.
        assert!(matches!(
            recover_with(&ops, &hosts),
            Err(WriteError::LeftAtBackup { .. })
        ));
        assert_eq!(fs::read(&backup).expect("backup kept"), b"a\n");

        update(&hosts, append(b"b\n")).expect("recovered and updated");
        assert_eq!(fs::read(&hosts).expect("read"), b"a\nb\n");
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    /// A crash between the two renames (simulated here) leaves only a backup of
    /// another process; recovery restores it.
    #[test]
    fn a_crash_leftover_backup_is_restored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        let backup = dir.path().join(".hosts.devlanes-4242.bak");
        fs::write(&backup, b"a\n").expect("backup");
        assert_eq!(recover(&hosts).expect("recover"), Some(backup));
        assert_eq!(fs::read(&hosts).expect("restored"), b"a\n");
        assert_eq!(recover(&hosts).expect("nothing to do"), None);
        assert_eq!(names(dir.path()), ["hosts"]);
    }

    /// With several candidates nothing is guessed: they are reported and kept.
    #[test]
    fn several_backups_are_reported_not_guessed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(dir.path().join(".hosts.devlanes-1.bak"), b"one").expect("one");
        fs::write(dir.path().join(".hosts.devlanes-2.bak"), b"two").expect("two");
        let error = update(&hosts, append(b"b\n")).expect_err("ambiguous");
        assert!(
            matches!(&error, UpdateError::Write(WriteError::Backups { found, .. }) if found.len() == 2),
            "{error:?}"
        );
        assert!(!hosts.exists());
        assert_eq!(names(dir.path()).len(), 2);
    }

    /// The read-only attribute gets its own error at once, not retries ending in
    /// access denied.
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
