//! Win32 backend. Process queries are not implemented yet and report
//! [`Error::Unsupported`]; callers already treat that as "no information".

use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
};

use crate::{Error, ListeningSocket, ProcessInfo};

pub(crate) fn processes() -> Result<Vec<ProcessInfo>, Error> {
    Err(Error::Unsupported("process enumeration"))
}

pub(crate) fn process(_pid: u32) -> Result<Option<ProcessInfo>, Error> {
    Err(Error::Unsupported("process lookup"))
}

pub(crate) fn boot_id() -> Result<String, Error> {
    Err(Error::Unsupported("boot id"))
}

pub(crate) fn listening_tcp(_pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    Err(Error::Unsupported("listening sockets"))
}

/// Volume serial number and 64-bit file index of an open handle.
pub(crate) fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the handle is owned by `file` and stays open for the call; `info` points
    // to writable memory of the size the function expects.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled the whole structure.
    let info = unsafe { info.assume_init() };
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}
