//! String and binary values under `HKEY_CURRENT_USER`, for `daemon install`'s
//! logon entry (`Software\Microsoft\Windows\CurrentVersion\Run`) and its Task
//! Manager state (`...\Explorer\StartupApproved\Run`). Only the current user's
//! hive is reachable through these, so nothing here needs elevation.
//!
//! [`machine`] reads `HKEY_LOCAL_MACHINE` for the hosts service (where the hosts
//! file lives, which user it serves) and writes there for its elevated installer.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_BINARY,
    REG_OPTION_NON_VOLATILE, REG_ROUTINE_FLAGS, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_BINARY,
    RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW,
    RegSetValueExW,
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
    read_string_in(HKEY_CURRENT_USER, subkey, name)
}

/// The `REG_BINARY` value `name` of `HKCU\<subkey>`, or `None` when the key or
/// the value does not exist. Other value types are an error.
pub fn read_binary(subkey: &str, name: &str) -> io::Result<Option<Vec<u8>>> {
    read_raw(HKEY_CURRENT_USER, subkey, name, RRF_RT_REG_BINARY)
}

/// Writes `value` as the `REG_SZ` value `name` of `HKCU\<subkey>`, creating the
/// key when it is missing.
pub fn write_string(subkey: &str, name: &str, value: &str) -> io::Result<()> {
    write_string_in(HKEY_CURRENT_USER, subkey, name, value)
}

/// Writes `value` as the `REG_BINARY` value `name` of `HKCU\<subkey>`, creating
/// the key when it is missing.
pub fn write_binary(subkey: &str, name: &str, value: &[u8]) -> io::Result<()> {
    write_raw(HKEY_CURRENT_USER, subkey, name, REG_BINARY, value)
}

/// Deletes the value `name` of `HKCU\<subkey>`; `false` when there was none.
pub fn delete_value(subkey: &str, name: &str) -> io::Result<bool> {
    delete_value_in(HKEY_CURRENT_USER, subkey, name)
}

/// Deletes `HKCU\<subkey>` with everything under it; a missing key is fine.
/// For tests that created a scratch key. An empty `subkey` is refused, since it
/// would empty the whole hive.
pub fn delete_tree(subkey: &str) -> io::Result<()> {
    delete_tree_in(HKEY_CURRENT_USER, subkey)
}

/// Values under `HKEY_LOCAL_MACHINE`. Reading most keys needs no elevation;
/// writing and deleting need an elevated process (or SYSTEM).
pub mod machine {
    use super::*;

    /// The string value `name` of `HKLM\<subkey>`, or `None` when the key or the
    /// value does not exist. A `REG_EXPAND_SZ` value comes back expanded with
    /// this process's environment (`RegGetValueW` does that for `RRF_RT_REG_SZ`).
    pub fn read_string(subkey: &str, name: &str) -> io::Result<Option<String>> {
        read_string_in(HKEY_LOCAL_MACHINE, subkey, name)
    }

    /// Writes `value` as the `REG_SZ` value `name` of `HKLM\<subkey>`, creating
    /// the key when it is missing. The key inherits its parent's access list.
    pub fn write_string(subkey: &str, name: &str, value: &str) -> io::Result<()> {
        write_string_in(HKEY_LOCAL_MACHINE, subkey, name, value)
    }

    /// Deletes the value `name` of `HKLM\<subkey>`; `false` when there was none.
    pub fn delete_value(subkey: &str, name: &str) -> io::Result<bool> {
        delete_value_in(HKEY_LOCAL_MACHINE, subkey, name)
    }

    /// Deletes `HKLM\<subkey>` with everything under it; a missing key is fine.
    /// Keys with fewer than three levels (such as `SYSTEM\CurrentControlSet`)
    /// are refused, so a caller bug cannot empty a hive or a top-level branch.
    pub fn delete_tree(subkey: &str) -> io::Result<()> {
        let levels = subkey.split('\\').filter(|level| !level.is_empty()).count();
        if levels < 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to delete HKEY_LOCAL_MACHINE\\{subkey}"),
            ));
        }
        delete_tree_in(HKEY_LOCAL_MACHINE, subkey)
    }
}

/// The string value `name` of `<hive>\<subkey>`.
fn read_string_in(hive: HKEY, subkey: &str, name: &str) -> io::Result<Option<String>> {
    let Some(bytes) = read_raw(hive, subkey, name, RRF_RT_REG_SZ)? else {
        return Ok(None);
    };
    let text: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    // RegGetValueW guarantees the terminating NUL for REG_SZ.
    let end = text
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(text.len());
    std::ffi::OsString::from_wide(&text[..end])
        .into_string()
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "value is not Unicode"))
}

/// The bytes of a value whose type `flags` allows, or `None` when the key or the
/// value does not exist.
fn read_raw(
    hive: HKEY,
    subkey: &str,
    name: &str,
    flags: REG_ROUTINE_FLAGS,
) -> io::Result<Option<Vec<u8>>> {
    let key = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    let mut bytes: u32 = 0;
    // SAFETY: both strings are NUL-terminated and outlive the call; a null buffer
    // asks only for the size, written to `bytes`.
    let status = unsafe {
        RegGetValueW(
            hive,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status)?;
    // The value may grow between the two calls (and an expanded value may need
    // more than the size query said); one retry with the new size is enough for a
    // value nobody else writes in a loop.
    for _ in 0..2 {
        let mut buffer = vec![0u8; bytes as usize];
        let mut size = bytes;
        // SAFETY: `buffer` holds `size` writable bytes; the strings outlive the call.
        let status = unsafe {
            RegGetValueW(
                hive,
                key.as_ptr(),
                name.as_ptr(),
                flags,
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
        buffer.truncate(size as usize);
        return Ok(Some(buffer));
    }
    Err(io::Error::other("registry value kept changing size"))
}

/// Writes `value` as a `REG_SZ` value.
fn write_string_in(hive: HKEY, subkey: &str, name: &str, value: &str) -> io::Result<()> {
    let data = wide(OsStr::new(value));
    let bytes: Vec<u8> = data.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    // `data` ends with the terminating NUL that REG_SZ needs.
    write_raw(hive, subkey, name, REG_SZ, &bytes)
}

/// Writes `data` as a value of type `kind`, creating the key when it is missing.
fn write_raw(
    hive: HKEY,
    subkey: &str,
    name: &str,
    kind: REG_VALUE_TYPE,
    data: &[u8],
) -> io::Result<()> {
    let key_name = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: the key name outlives the call; `key` receives an open handle.
    check(unsafe {
        RegCreateKeyExW(
            hive,
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
    let Ok(bytes) = u32::try_from(data.len()) else {
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value is too long",
        ));
    };
    // SAFETY: `key` is open with KEY_SET_VALUE; `data` holds `bytes` bytes.
    let status = unsafe { RegSetValueExW(key, name.as_ptr(), 0, kind, data.as_ptr(), bytes) };
    // SAFETY: `key` was opened above and is closed once.
    unsafe { RegCloseKey(key) };
    check(status)
}

/// Deletes one value; `false` when there was none.
fn delete_value_in(hive: HKEY, subkey: &str, name: &str) -> io::Result<bool> {
    let key = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    match unsafe { RegDeleteKeyValueW(hive, key.as_ptr(), name.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(false),
        status => check(status).map(|()| true),
    }
}

/// Deletes a key with everything under it; a missing key is fine, an empty
/// `subkey` (the whole hive) is refused.
fn delete_tree_in(hive: HKEY, subkey: &str) -> io::Result<()> {
    if subkey.trim_matches('\\').is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete a whole registry hive",
        ));
    }
    let key = wide(OsStr::new(subkey));
    // SAFETY: the key name is NUL-terminated and outlives the call.
    match unsafe { RegDeleteTreeW(hive, key.as_ptr()) } {
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

    /// Binary values round-trip, including an empty one; a value of the other
    /// type is an error rather than a misread.
    #[test]
    fn binary_values_round_trip() -> io::Result<()> {
        let scratch = Scratch::new();
        assert_eq!(read_binary(&scratch.0, "devlanes")?, None);
        let value = [3, 0, 0, 0, 0x2c, 0xf1, 0x65, 0x7f, 0x99, 0xed, 0xd9, 0x01];
        write_binary(&scratch.0, "devlanes", &value)?;
        assert_eq!(
            read_binary(&scratch.0, "devlanes")?.as_deref(),
            Some(&value[..])
        );
        write_binary(&scratch.0, "empty", &[])?;
        assert_eq!(read_binary(&scratch.0, "empty")?.as_deref(), Some(&[][..]));
        assert!(read_string(&scratch.0, "devlanes").is_err());
        write_string(&scratch.0, "text", "x")?;
        assert!(read_binary(&scratch.0, "text").is_err());
        Ok(())
    }

    /// An empty string, stored as only its terminating NUL, reads back empty.
    #[test]
    fn empty_string_round_trips() -> io::Result<()> {
        let scratch = Scratch::new();
        write_string(&scratch.0, "devlanes", "")?;
        assert_eq!(read_string(&scratch.0, "devlanes")?.as_deref(), Some(""));
        delete_tree(&scratch.0)?;
        delete_tree(&scratch.0)?;
        assert!(delete_tree(r"\").is_err());
        Ok(())
    }

    /// The hosts file directory comes from a `REG_EXPAND_SZ` value under HKLM,
    /// readable without elevation and expanded on read. Read-only.
    #[test]
    fn machine_values_read_expanded() -> io::Result<()> {
        let directory = machine::read_string(
            r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters",
            "DataBasePath",
        )?
        .expect("DataBasePath");
        assert!(!directory.contains('%'), "{directory}");
        assert!(
            directory
                .to_ascii_lowercase()
                .ends_with(r"\system32\drivers\etc"),
            "{directory}"
        );
        assert_eq!(
            machine::read_string(
                r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters",
                "devlanes-missing"
            )?,
            None
        );
        // Refused before any call, so nothing is ever deleted here.
        for shallow in ["", r"\", "SYSTEM", r"SYSTEM\CurrentControlSet"] {
            assert_eq!(
                machine::delete_tree(shallow)
                    .err()
                    .map(|error| error.kind()),
                Some(io::ErrorKind::InvalidInput),
                "{shallow:?}"
            );
        }
        Ok(())
    }
}
