//! The names files are stored under. A path can reach a file under another
//! name: NTFS matches names without case (outside case-sensitive directories)
//! and also accepts 8.3 short names such as `README~1.MD`.

use std::ffi::OsString;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, Prefix};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, WIN32_FIND_DATAW,
};

/// The stored name of the directory entry `path` resolves to: its long name in
/// the case it was created with. That differs from `path`'s last component when
/// the component matched without case or was an 8.3 short name. `None` when
/// nothing is there. Blocking.
pub fn stored_name(path: &Path) -> io::Result<Option<OsString>> {
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?;
    // The search treats these as wildcards (`<`, `>` and `"` in their DOS
    // forms); none of them can appear in a file name.
    if leaf
        .encode_wide()
        .any(|unit| matches!(unit, 0x22 | 0x2a | 0x3c | 0x3e | 0x3f))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the name holds a wildcard character",
        ));
    }
    let wide = verbatim(&std::path::absolute(path)?);
    let mut data = MaybeUninit::<WIN32_FIND_DATAW>::zeroed();
    // SAFETY: `wide` is NUL-terminated and `data` is writable memory of the
    // structure the basic info level fills; no filter is passed.
    let handle = unsafe {
        FindFirstFileExW(
            wide.as_ptr(),
            FindExInfoBasic,
            data.as_mut_ptr().cast(),
            FindExSearchNameMatch,
            ptr::null(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        let missing = [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
            .iter()
            .any(|&code| error.raw_os_error() == Some(code as i32));
        return if missing { Ok(None) } else { Err(error) };
    }
    // SAFETY: `handle` is the search handle just returned, closed once here.
    unsafe { FindClose(handle) };
    // SAFETY: the search succeeded, so it filled the structure.
    let data = unsafe { data.assume_init() };
    let name = &data.cFileName;
    let len = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Ok(Some(OsString::from_wide(&name[..len])))
}

/// `path` (absolute) in its `\\?\` form, NUL-terminated, so a long sync path
/// is not cut at `MAX_PATH`.
fn verbatim(path: &Path) -> Vec<u16> {
    let text = path.as_os_str().encode_wide();
    let mut wide: Vec<u16> = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => r"\\?\".encode_utf16().chain(text).collect(),
            // `\\server\share` becomes `\\?\UNC\server\share`.
            Prefix::UNC(..) => r"\\?\UNC".encode_utf16().chain(text.skip(1)).collect(),
            _ => text.collect(),
        },
        _ => text.collect(),
    };
    wide.push(0);
    wide
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Another case finds the stored spelling; a missing entry is `None`; an
    /// 8.3 short name finds the long name when the volume creates short names.
    #[test]
    fn stored_names_are_the_long_names_in_their_own_case() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("Mixed.TXT"), b"x")?;
        assert_eq!(
            stored_name(&dir.path().join("mixed.txt"))?,
            Some(OsString::from("Mixed.TXT"))
        );
        assert_eq!(stored_name(&dir.path().join("missing"))?, None);
        assert_eq!(stored_name(&dir.path().join("no/such/dir"))?, None);
        assert_eq!(
            stored_name(&dir.path().join("a*")).map_err(|error| error.kind()),
            Err(io::ErrorKind::InvalidInput)
        );
        std::fs::write(dir.path().join("readme-long.md"), b"x")?;
        let short = dir.path().join("README~1.MD");
        if short.exists() {
            assert_eq!(stored_name(&short)?, Some(OsString::from("readme-long.md")));
        } else {
            eprintln!("8.3 names are off on this volume; short-name case not checked");
        }
        Ok(())
    }
}
