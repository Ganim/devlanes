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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_directory_holds_windows_tools() {
        let dir = system_directory().expect("system directory");
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.join("cmd.exe").is_file(), "{}", dir.display());
    }
}
