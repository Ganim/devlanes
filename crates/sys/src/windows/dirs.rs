//! Windows' own directories, from the API rather than the environment, which the
//! user (or anything running as the user) controls.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

/// The System32 directory (`C:\Windows\System32`), where Windows' own tools such as
/// `certutil.exe` and `netsh.exe` live. Unlike `%SystemRoot%`, a changed
/// environment cannot point it elsewhere.
pub fn system_directory() -> io::Result<PathBuf> {
    // MAX_PATH covers it on every supported Windows; the loop handles the rest.
    let mut buffer = vec![0u16; 260];
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

/// The 64-bit Program Files directory (`C:\Program Files`), from the known-folder
/// API rather than `%ProgramFiles%`, which the caller's environment controls.
pub fn program_files() -> io::Result<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    let mut path: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: the folder id is a static GUID; on success `path` receives a
    // CoTaskMemAlloc'ed NUL-terminated string, freed below on every path (the API
    // asks for that even on failure).
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramFiles,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &raw mut path,
        )
    };
    let result = if status < 0 || path.is_null() {
        Err(io::Error::from_raw_os_error(status))
    } else {
        // SAFETY: `path` is NUL-terminated per the API contract.
        let len = unsafe { (0..).take_while(|&i| *path.add(i) != 0).count() };
        // SAFETY: `len` units before the terminator are readable.
        let units = unsafe { std::slice::from_raw_parts(path, len) };
        Ok(PathBuf::from(OsString::from_wide(units)))
    };
    // SAFETY: allocated by SHGetKnownFolderPath (null is allowed).
    unsafe { CoTaskMemFree(path.cast()) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_directory_holds_windows_tools() {
        let dir = system_directory().expect("system directory");
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.join("cmd.exe").is_file(), "{}", dir.display());
    }

    #[test]
    fn program_files_is_an_absolute_directory() {
        let dir = program_files().expect("program files");
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.is_dir(), "{}", dir.display());
    }
}
