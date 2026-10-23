//! File identity, renames and removals that ride out brief sharing
//! violations, and the System32 directory.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::util::raw;

/// Which file an open handle refers to: equal values are the same file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    /// The volume's serial number.
    pub volume: u64,
    /// The file's ID on that volume.
    pub file: u128,
}

/// The identity of an open file: the volume serial number and 128-bit file
/// ID (`FileIdInfo`). Volumes without 128-bit IDs (FAT) give the 64-bit
/// file index instead.
pub fn file_identity(file: &File) -> io::Result<FileId> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the handle stays open for the call; `info` is writable memory of
    // the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok != 0 {
        return Ok(FileId {
            volume: info.VolumeSerialNumber,
            file: u128::from_le_bytes(info.FileId.Identifier),
        });
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: as above, for the older structure.
    if unsafe { GetFileInformationByHandle(raw(file), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok(FileId {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: u128::from(index),
    })
}

/// The identity of the file or directory at `path` itself, never a link's
/// target, as [`file_identity`] gives it. Opens it only to read attributes
/// and shares everything, so it never blocks a rename or removal.
pub fn path_identity(path: &Path) -> io::Result<FileId> {
    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    file_identity(&file)
}

/// Waits between attempts: five retries, 620 ms in all.
const DELAYS: [Duration; 5] = [
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
    Duration::from_millis(320),
];

/// [`std::fs::rename`], retried while another process (an editor, an
/// indexer, antivirus) has the file open without sharing delete: on sharing
/// and lock violations, and once on access denied, which a file being
/// deleted also gives. Blocks for up to about 0.6 s, so async callers run it
/// on a blocking thread.
pub fn rename_retrying(from: &Path, to: &Path) -> io::Result<()> {
    retrying(false, || std::fs::rename(from, to))
}

/// [`std::fs::remove_file`], retried as [`rename_retrying`] is. A file that
/// is gone by a retry counts as removed. Blocks for up to about 0.6 s.
pub fn remove_file_retrying(path: &Path) -> io::Result<()> {
    retrying(true, || std::fs::remove_file(path))
}

/// [`std::fs::remove_dir_all`], retried as [`rename_retrying`] is. A
/// directory that is gone by a retry counts as removed. Blocks for up to
/// about 0.6 s plus the removal itself.
pub fn remove_dir_all_retrying(path: &Path) -> io::Result<()> {
    retrying(true, || std::fs::remove_dir_all(path))
}

/// Runs `operation`, a rename or removal of a file devlanes owns that the
/// helpers above do not cover (such as `tempfile`'s `persist`), with their
/// retries. Blocks for up to about 0.6 s.
pub fn retry_while_held(operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    retrying(false, operation)
}

/// Whether `error` (from one of the retrying helpers) means another process
/// still has the file, or a file below the directory, open: a sharing or lock
/// violation, or access denied, which an open file pending deletion gives.
/// Callers use it to tell the user to close that program.
pub fn is_held(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(i32::cast_unsigned),
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_ACCESS_DENIED)
    )
}

/// Runs `operation` until it succeeds, fails for another reason, or the
/// delays run out. With `gone_is_done`, `NotFound` after a retry is success.
fn retrying(gone_is_done: bool, mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut delays = DELAYS.iter();
    let mut denied_retried = false;
    let mut retried = false;
    loop {
        let error = match operation() {
            Ok(()) => return Ok(()),
            Err(error) if retried && gone_is_done && error.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => error,
        };
        let retry = match error.raw_os_error().map(i32::cast_unsigned) {
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => true,
            Some(ERROR_ACCESS_DENIED) if !denied_retried => {
                denied_retried = true;
                true
            }
            _ => false,
        };
        match delays.next() {
            Some(&delay) if retry => {
                std::thread::sleep(delay);
                retried = true;
            }
            _ => return Err(error),
        }
    }
}

/// The System32 directory (`C:\Windows\System32`), from the API rather than
/// `%SystemRoot%`, which the environment controls.
pub fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 260];
    loop {
        let capacity = u32::try_from(buffer.len())
            .map_err(|_| io::Error::other("system directory path too long"))?;
        // SAFETY: `buffer` holds `capacity` writable UTF-16 units.
        let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), capacity) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // A result larger than the buffer is the size needed, NUL included.
        if len >= capacity {
            buffer.resize(len as usize, 0);
            continue;
        }
        buffer.truncate(len as usize);
        return Ok(PathBuf::from(OsString::from_wide(&buffer)));
    }
}
