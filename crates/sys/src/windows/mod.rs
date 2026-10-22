//! Win32 backend: the process table (`proc`) plus safe wrappers for the endpoint,
//! daemon and runner crates.
//!
//! Processes have no session, process group or numeric user ID here, so
//! [`crate::ProcessInfo`] reports `sid`, `pgid` and `uid` as `None`; units are
//! grouped by Job Objects instead.

pub mod cert;
pub mod clipboard;
pub mod conpty;
pub mod console;
pub mod ctrl;
pub mod dirs;
pub mod handles;
pub mod job;
pub mod names;
pub mod net;
pub mod pipe;
pub mod proc;
pub mod process;
pub mod registry;
pub mod replace;
pub mod security;
pub mod service;
pub mod session;
pub mod shell;
pub mod spawn;
pub mod token;
pub mod usage;
pub mod window;

pub(crate) use proc::{boot_id, listening_tcp, process, processes};

use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
};

use crate::FileIdentity;

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
