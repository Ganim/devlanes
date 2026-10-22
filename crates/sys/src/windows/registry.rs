//! String values under `HKEY_CURRENT_USER`, for `daemon install`'s logon entry
//! (`Software\Microsoft\Windows\CurrentVersion\Run`). Only the current user's
//! hive is reachable, so nothing here needs elevation.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
};

use super::security::wide;

/// Maps a registry status to a result.
fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

/// The `REG_SZ` value `name` of `HKCU\<subkey>`, or `None` when the key or the
/// value does not exist. Other value types are an error.
pub fn read_string(subkey: &str, name: &str) -> io::Result<Option<String>> {
    let key = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    let mut bytes: u32 = 0;
    // SAFETY: both strings are NUL-terminated and outlive the call; a null buffer
    // asks only for the size, written to `bytes`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status)?;
    // The value may grow between the two calls; one retry with the new size is
    // enough for a value nobody else writes in a loop.
    for _ in 0..2 {
        let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
        let mut size = bytes;
        // SAFETY: `buffer` holds `size` writable bytes; the strings outlive the call.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        match status {
            ERROR_FILE_NOT_FOUND => return Ok(None),
            windows_sys::Win32::Foundation::ERROR_MORE_DATA => {
                bytes = size;
                continue;
            }
            status => check(status)?,
        }
        // RegGetValueW guarantees the terminating NUL for REG_SZ.
        let units = (size as usize / 2).min(buffer.len());
        let text = &buffer[..units];
        let end = text
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(text.len());
        return std::ffi::OsString::from_wide(&text[..end])
            .into_string()
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "value is not Unicode"));
    }
    Err(io::Error::other("registry value kept changing size"))
}

/// Writes `value` as the `REG_SZ` value `name` of `HKCU\<subkey>`, creating the
/// key when it is missing.
pub fn write_string(subkey: &str, name: &str, value: &str) -> io::Result<()> {
    let key_name = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    let data = wide(OsStr::new(value));
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: the key name outlives the call; `key` receives an open handle.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_name.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            ptr::null(),
            &raw mut key,
            ptr::null_mut(),
        )
    })?;
    let bytes = u32::try_from(data.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "value is too long"))?;
    // SAFETY: `key` is open with KEY_SET_VALUE; `data` holds `bytes` bytes
    // including the terminating NUL that REG_SZ needs.
    let status =
        unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), bytes) };
    // SAFETY: `key` was opened above and is closed once.
    unsafe { RegCloseKey(key) };
    check(status)
}

/// Deletes the value `name` of `HKCU\<subkey>`; `false` when there was none.
pub fn delete_value(subkey: &str, name: &str) -> io::Result<bool> {
    let key = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(false),
        status => check(status).map(|()| true),
    }
}

/// Deletes `HKCU\<subkey>` with everything under it; a missing key is fine.
/// For tests that created a scratch key. An empty `subkey` is refused, since it
/// would empty the whole hive.
pub fn delete_tree(subkey: &str) -> io::Result<()> {
    if subkey.trim_matches('\\').is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete HKEY_CURRENT_USER itself",
        ));
    }
    let key = wide(OsStr::new(subkey));
    // SAFETY: the key name is NUL-terminated and outlives the call.
    match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(()),
        status => check(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch key under `HKCU\Software`, removed when dropped.
    struct Scratch(String);

    impl Scratch {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or_default();
            Self(format!(
                r"Software\devlanes-test-{}-{nanos}",
                std::process::id()
            ))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = delete_tree(&self.0);
        }
    }

    /// Values round-trip, including non-ASCII text, and deleting reports
    /// whether there was one; missing keys and values read as `None`.
    #[test]
    fn strings_round_trip() -> io::Result<()> {
        let scratch = Scratch::new();
        assert_eq!(read_string(&scratch.0, "devlanes")?, None);
        assert!(!delete_value(&scratch.0, "devlanes")?);
        let value = r#""C:\Program Files\dévlanes\devlanesw.exe" --home "C:\Users\a b\.devlanes""#;
        write_string(&scratch.0, "devlanes", value)?;
        assert_eq!(read_string(&scratch.0, "devlanes")?.as_deref(), Some(value));
        assert_eq!(read_string(&scratch.0, "other")?, None);
        write_string(&scratch.0, "devlanes", "short")?;
        assert_eq!(
            read_string(&scratch.0, "devlanes")?.as_deref(),
            Some("short")
        );
        assert!(delete_value(&scratch.0, "devlanes")?);
        assert_eq!(read_string(&scratch.0, "devlanes")?, None);
        delete_tree(&scratch.0)?;
        delete_tree(&scratch.0)?;
        assert!(delete_tree(r"\").is_err());
        Ok(())
    }
}
