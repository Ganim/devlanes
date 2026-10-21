//! Win32 backend. The process-table queries are not implemented yet and report
//! [`Error::Unsupported`]. Callers differ in what they make of that (the daemon's
//! orphan check reads it as "not alive"), so no Windows runner may depend on them
//! before they exist.

pub mod pipe;
pub mod process;
pub mod security;
pub mod token;

use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
};

use crate::{Error, FileIdentity, ListeningSocket, ProcessInfo};

/// Not implemented yet.
pub(crate) fn processes() -> Result<Vec<ProcessInfo>, Error> {
    Err(Error::Unsupported("process enumeration"))
}

/// Not implemented yet.
pub(crate) fn process(_pid: u32) -> Result<Option<ProcessInfo>, Error> {
    Err(Error::Unsupported("process lookup"))
}

/// Not implemented yet.
pub(crate) fn boot_id() -> Result<String, Error> {
    Err(Error::Unsupported("boot id"))
}

/// Not implemented yet.
pub(crate) fn listening_tcp(_pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    Err(Error::Unsupported("listening sockets"))
}

/// Volume serial number and 128-bit file id of an open handle. File systems
/// without `FILE_ID_INFO` (FAT) fall back to the 64-bit file index.
pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let mut info = MaybeUninit::<FILE_ID_INFO>::zeroed();
    // SAFETY: the handle is owned by `file` and stays open for the call; `info`
    // points to writable memory of the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok != 0 {
        // SAFETY: the call succeeded, so it filled the whole structure.
        let info = unsafe { info.assume_init() };
        return Ok(FileIdentity::new(
            info.VolumeSerialNumber,
            u128::from_le_bytes(info.FileId.Identifier),
        ));
    }
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: as above, for the older structure.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled the whole structure.
    let info = unsafe { info.assume_init() };
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok(FileIdentity::new(
        u64::from(info.dwVolumeSerialNumber),
        u128::from(index),
    ))
}
