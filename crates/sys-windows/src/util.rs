//! Helpers shared by the FFI modules.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::core::HRESULT;

/// A NUL-terminated UTF-16 copy for Win32 string parameters. A NUL inside
/// `text` is refused: Win32 would silently stop reading there.
pub(crate) fn wide(text: &OsStr) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = text.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "string contains a NUL character",
        ));
    }
    units.push(0);
    Ok(units)
}

/// Takes ownership of a handle a Win32 call just returned, or the last error
/// when it is null.
pub(crate) fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the caller just received `handle` from the OS and owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// The raw handle for a Win32 call; ownership stays with `handle`.
pub(crate) fn raw(handle: &impl AsRawHandle) -> HANDLE {
    handle.as_raw_handle()
}

/// An error for a failed `HRESULT`. One that wraps a Win32 code
/// (`HRESULT_FROM_WIN32`, facility 7: `0x8007xxxx`) becomes that code, so
/// callers can still tell `NotFound` or `PermissionDenied`; any other HRESULT
/// is not a Win32 code and never goes through `io::Error::from_raw_os_error`.
pub(crate) fn hresult_error(result: HRESULT) -> io::Error {
    let code = result.cast_unsigned();
    if code & 0xFFFF_0000 == 0x8007_0000 {
        return win32_error(code & 0xFFFF);
    }
    io::Error::other(format!("HRESULT {result:#x}"))
}

/// The UTF-16 units before the first NUL of a fixed-size field.
pub(crate) fn until_nul(field: &[u16]) -> &[u16] {
    let len = field
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(field.len());
    &field[..len]
}

/// The error `code` (a Win32 error) as an `io::Error`.
pub(crate) fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code.cast_signed())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapped Win32 codes keep their kind; other HRESULTs stay opaque.
    #[test]
    fn hresults_of_win32_codes_keep_their_kind() {
        let kind = |code: u32| hresult_error(code.cast_signed()).kind();
        assert_eq!(kind(0x8007_0002), io::ErrorKind::NotFound);
        assert_eq!(kind(0x8007_0005), io::ErrorKind::PermissionDenied);
        let other = hresult_error(0x8009_2004_u32.cast_signed());
        assert_eq!(other.raw_os_error(), None);
        assert_eq!(other.to_string(), "HRESULT 0x80092004");
    }
}
