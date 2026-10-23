//! String and binary values under `HKEY_CURRENT_USER`, such as the logon
//! entry under `Software\Microsoft\Windows\CurrentVersion\Run` and its Task
//! Manager state under `...\Explorer\StartupApproved\Run`. Every write goes to
//! the current user's hive, so nothing needs elevation. [`read_value`] and
//! [`subkeys`] also read `HKEY_LOCAL_MACHINE`, such as the NRPT rules under
//! `SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, WIN32_ERROR,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_SET_VALUE, REG_BINARY,
    REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_OPTION_NON_VOLATILE, REG_ROUTINE_FLAGS, REG_SZ,
    REG_VALUE_TYPE, RRF_NOEXPAND, RRF_RT_ANY, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW, RegSetValueExW,
};

use crate::util::{wide, win32_error};

/// A hive [`read_value`] and [`subkeys`] can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hive {
    /// `HKEY_CURRENT_USER`.
    CurrentUser,
    /// `HKEY_LOCAL_MACHINE`, read-only here.
    LocalMachine,
}

impl Hive {
    /// The predefined handle.
    fn handle(self) -> HKEY {
        match self {
            Self::CurrentUser => HKEY_CURRENT_USER,
            Self::LocalMachine => HKEY_LOCAL_MACHINE,
        }
    }
}

/// A value as [`read_value`] found it, by its registry type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    /// `REG_SZ` or `REG_EXPAND_SZ` (not expanded).
    String(String),
    /// `REG_MULTI_SZ`, without the empty string that ends the list.
    MultiString(Vec<String>),
    /// `REG_DWORD`.
    Dword(u32),
    /// Any other type, by its `REG_*` number.
    Other(u32),
}

/// The `REG_SZ` value `name` of `HKCU\<subkey>`, or `None` when the key or
/// the value does not exist. A value of another type is an error.
pub fn read_string(subkey: &str, name: &str) -> io::Result<Option<String>> {
    let Some((_, bytes)) = read_raw(HKEY_CURRENT_USER, subkey, name, RRF_RT_REG_SZ)? else {
        return Ok(None);
    };
    // RegGetValueW guarantees the terminating NUL for REG_SZ.
    let units = utf16(&bytes);
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    OsString::from_wide(&units[..end])
        .into_string()
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "value is not Unicode"))
}

/// The `REG_BINARY` value `name` of `HKCU\<subkey>`, or `None` when the key or
/// the value does not exist. A value of another type is an error.
pub fn read_binary(subkey: &str, name: &str) -> io::Result<Option<Vec<u8>>> {
    Ok(read_raw(HKEY_CURRENT_USER, subkey, name, RRF_RT_REG_BINARY)?.map(|(_, bytes)| bytes))
}

/// The value `name` of `<hive>\<subkey>` whatever its type, or `None` when
/// the key or the value does not exist. Text that is not valid UTF-16 is
/// decoded lossily, since callers only compare it with known values.
pub fn read_value(hive: Hive, subkey: &str, name: &str) -> io::Result<Option<RegValue>> {
    // NOEXPAND: an expandable string is compared as stored, never with the
    // environment of whoever reads it.
    let Some((kind, bytes)) = read_raw(hive.handle(), subkey, name, RRF_RT_ANY | RRF_NOEXPAND)?
    else {
        return Ok(None);
    };
    Ok(Some(decode(kind, &bytes)))
}

/// The names of the keys directly under `<hive>\<subkey>`, or `None` when
/// that key does not exist. Their order is the registry's.
pub fn subkeys(hive: Hive, subkey: &str) -> io::Result<Option<Vec<String>>> {
    let name = wide(OsStr::new(subkey))?;
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: the name is NUL-terminated and outlives the call; `key`
    // receives an open handle on success.
    let status = unsafe {
        RegOpenKeyExW(
            hive.handle(),
            name.as_ptr(),
            0,
            KEY_ENUMERATE_SUB_KEYS,
            &raw mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status)?;
    let key = OpenKey(key);
    let mut names = Vec::new();
    for index in 0.. {
        // Key names are at most 255 characters.
        let mut buffer = [0_u16; 256];
        let mut len = buffer.len() as u32;
        // SAFETY: `key` is open with KEY_ENUMERATE_SUB_KEYS; `buffer` holds
        // `len` writable units; the optional outputs are null.
        let status = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                buffer.as_mut_ptr(),
                &raw mut len,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        match status {
            ERROR_NO_MORE_ITEMS => break,
            status => check(status)?,
        }
        names.push(String::from_utf16_lossy(&buffer[..len as usize]));
    }
    Ok(Some(names))
}

/// Writes `value` as the `REG_SZ` value `name` of `HKCU\<subkey>`, creating
/// the key when it is missing.
pub fn write_string(subkey: &str, name: &str, value: &str) -> io::Result<()> {
    // `wide` ends with the terminating NUL that REG_SZ needs.
    let data = wide(OsStr::new(value))?;
    write_raw(subkey, name, REG_SZ, &le_bytes(&data))
}

/// Writes `values` as the `REG_MULTI_SZ` value `name` of `HKCU\<subkey>`,
/// creating the key when it is missing. An empty string inside the list is
/// refused, since it would end the list there.
pub fn write_multi_string(subkey: &str, name: &str, values: &[&str]) -> io::Result<()> {
    let mut data = Vec::new();
    for value in values {
        if value.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a multi-string entry cannot be empty",
            ));
        }
        data.extend(wide(OsStr::new(value))?);
    }
    data.push(0);
    write_raw(subkey, name, REG_MULTI_SZ, &le_bytes(&data))
}

/// Writes `value` as the `REG_DWORD` value `name` of `HKCU\<subkey>`,
/// creating the key when it is missing.
pub fn write_dword(subkey: &str, name: &str, value: u32) -> io::Result<()> {
    write_raw(subkey, name, REG_DWORD, &value.to_le_bytes())
}

/// Writes `value` as the `REG_BINARY` value `name` of `HKCU\<subkey>`,
/// creating the key when it is missing.
pub fn write_binary(subkey: &str, name: &str, value: &[u8]) -> io::Result<()> {
    write_raw(subkey, name, REG_BINARY, value)
}

/// Deletes the value `name` of `HKCU\<subkey>`; `false` when there was none.
pub fn delete_value(subkey: &str, name: &str) -> io::Result<bool> {
    let key = wide(OsStr::new(subkey))?;
    let name = wide(OsStr::new(name))?;
    // SAFETY: both strings are NUL-terminated and outlive the call.
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(false),
        status => check(status).map(|()| true),
    }
}

/// Deletes `HKCU\<subkey>` with everything under it; a missing key is fine.
/// An empty `subkey` is refused, since it would empty the whole hive.
pub fn delete_tree(subkey: &str) -> io::Result<()> {
    if subkey.trim_matches('\\').is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete a whole registry hive",
        ));
    }
    let key = wide(OsStr::new(subkey))?;
    // SAFETY: the key name is NUL-terminated and outlives the call.
    match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(()),
        status => check(status),
    }
}

/// A key opened with `RegOpenKeyExW`, closed on drop.
struct OpenKey(HKEY);

impl Drop for OpenKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegOpenKeyExW and is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Maps a registry status, a Win32 error code, to a result.
fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win32_error(status))
    }
}

/// Little-endian UTF-16 units of `bytes`; an odd last byte is dropped.
fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect()
}

/// The bytes of UTF-16 `units` as the registry stores them.
fn le_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// A value of type `kind` from its raw bytes.
fn decode(kind: REG_VALUE_TYPE, bytes: &[u8]) -> RegValue {
    let units = utf16(bytes);
    match kind {
        REG_SZ | REG_EXPAND_SZ => {
            let end = units
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(units.len());
            RegValue::String(String::from_utf16_lossy(&units[..end]))
        }
        // The list ends at the first empty string (the double NUL).
        REG_MULTI_SZ => RegValue::MultiString(
            units
                .split(|&unit| unit == 0)
                .take_while(|entry| !entry.is_empty())
                .map(String::from_utf16_lossy)
                .collect(),
        ),
        REG_DWORD => match bytes {
            [a, b, c, d] => RegValue::Dword(u32::from_le_bytes([*a, *b, *c, *d])),
            _ => RegValue::Other(kind),
        },
        other => RegValue::Other(other),
    }
}

/// The type and bytes of a value whose type `flags` allows, or `None` when
/// the key or the value does not exist.
fn read_raw(
    hive: HKEY,
    subkey: &str,
    name: &str,
    flags: REG_ROUTINE_FLAGS,
) -> io::Result<Option<(REG_VALUE_TYPE, Vec<u8>)>> {
    let key = wide(OsStr::new(subkey))?;
    let name = wide(OsStr::new(name))?;
    let mut bytes: u32 = 0;
    // SAFETY: both strings are NUL-terminated and outlive the call; a null
    // buffer asks only for the size, written to `bytes`.
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
    // The value may change size between the calls; a few retries are enough
    // for a value nobody rewrites in a loop.
    for _ in 0..3 {
        let mut buffer = vec![0_u8; bytes as usize];
        let mut size = bytes;
        let mut kind: REG_VALUE_TYPE = 0;
        // SAFETY: `buffer` holds `size` writable bytes; `kind` is writable;
        // the strings outlive the call.
        let status = unsafe {
            RegGetValueW(
                hive,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                &raw mut kind,
                buffer.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        match status {
            ERROR_FILE_NOT_FOUND => return Ok(None),
            ERROR_MORE_DATA => {
                bytes = size;
                continue;
            }
            status => check(status)?,
        }
        buffer.truncate(size as usize);
        return Ok(Some((kind, buffer)));
    }
    Err(io::Error::other("registry value kept changing size"))
}

/// Writes `data` as a value of type `kind`, creating the key when it is
/// missing with the default security, inherited from its parent.
fn write_raw(subkey: &str, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> io::Result<()> {
    let key_name = wide(OsStr::new(subkey))?;
    let name = wide(OsStr::new(name))?;
    let length = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "value is too long"))?;
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: the key name outlives the call; null security attributes are
    // allowed; `key` receives an open handle on success.
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
    let key = OpenKey(key);
    // SAFETY: `key` is open with KEY_SET_VALUE; `data` holds `length` bytes.
    check(unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, kind, data.as_ptr(), length) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_decode_by_type() {
        let text = |s: &str| le_bytes(&s.encode_utf16().collect::<Vec<_>>());
        assert_eq!(
            decode(REG_SZ, &text("127.53.0.1\0")),
            RegValue::String("127.53.0.1".into())
        );
        assert_eq!(decode(REG_SZ, &text("")), RegValue::String(String::new()));
        assert_eq!(
            decode(REG_MULTI_SZ, &text(".test\0.dev.internal\0\0")),
            RegValue::MultiString(vec![".test".into(), ".dev.internal".into()])
        );
        assert_eq!(
            decode(REG_MULTI_SZ, &text("\0")),
            RegValue::MultiString(Vec::new())
        );
        assert_eq!(decode(REG_DWORD, &8_u32.to_le_bytes()), RegValue::Dword(8));
        assert_eq!(decode(REG_DWORD, &[1, 2]), RegValue::Other(REG_DWORD));
        assert_eq!(decode(REG_BINARY, &[1]), RegValue::Other(REG_BINARY));
    }
}
