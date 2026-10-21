//! Security identifiers of processes, threads and pipe peers.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, PSID, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// An owned copy of a security identifier. SIDs have one binary form, so byte
/// equality is identity.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sid(Vec<u8>);

impl Sid {
    /// Copies the SID at `sid`.
    ///
    /// # Safety
    ///
    /// `sid` must point to a valid SID that stays alive for the call.
    pub(crate) unsafe fn copy_from(sid: PSID) -> io::Result<Self> {
        // SAFETY: the caller guarantees `sid` points to a SID.
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid SID"));
        }
        // SAFETY: as above; a valid SID reports its own length.
        let len = unsafe { GetLengthSid(sid) } as usize;
        // SAFETY: a valid SID is `len` readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), len) };
        Ok(Self(bytes.to_vec()))
    }

    /// The SID in the form Win32 APIs take.
    pub(crate) fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }

    /// The string form, such as `S-1-5-21-...`, used in SDDL and for display.
    pub fn to_sddl(&self) -> io::Result<String> {
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: `as_psid` points into `self`, a SID copied by `copy_from`; on success
        // `text` receives a LocalAlloc'ed NUL-terminated string freed below.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &raw mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `text` is NUL-terminated per the API contract.
        let len = unsafe { (0..).take_while(|&i| *text.add(i) != 0).count() };
        // SAFETY: `len` units before the terminator are readable.
        let string = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc.
        unsafe { LocalFree(text.cast()) };
        Ok(string)
    }
}

impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_sddl() {
            Ok(text) => f.write_str(&text),
            Err(_) => f.write_str("<invalid SID>"),
        }
    }
}

/// The user this process runs as.
pub fn current_user() -> io::Result<Sid> {
    token_user(&current_process_token()?)
}

/// The user process `pid` runs as.
pub fn process_user(pid: u32) -> io::Result<Sid> {
    // SAFETY: plain call; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let process = owned(process)?;
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is an open handle with query rights; `token` receives a new handle.
    if unsafe { OpenProcessToken(raw(&process), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    token_user(&owned(token)?)
}

/// Whether this process runs elevated (as administrator under UAC).
pub fn elevated() -> io::Result<bool> {
    let token = current_process_token()?;
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0;
    // SAFETY: `token` has query rights and `elevation` is the structure TokenElevation fills.
    let ok = unsafe {
        GetTokenInformation(
            raw(&token),
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut returned,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

/// The user of an access token.
pub(crate) fn token_user(token: &OwnedHandle) -> io::Result<Sid> {
    let mut needed = 0;
    // SAFETY: a size query with no buffer; it fails with the needed size.
    unsafe { GetTokenInformation(raw(token), TokenUser, ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 elements keep the buffer aligned for TOKEN_USER's pointer field.
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds `needed` writable bytes, aligned for TOKEN_USER.
    let ok = unsafe {
        GetTokenInformation(
            raw(token),
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            needed,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call filled a TOKEN_USER at the start of the buffer; its SID points
    // into the same buffer, which outlives the copy.
    unsafe {
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        Sid::copy_from(user.User.Sid)
    }
}

fn current_process_token() -> io::Result<OwnedHandle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle needs no closing; `token` receives a new handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    owned(token)
}

/// Takes ownership of a handle returned by a Win32 call, or the last error when null.
pub(crate) fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the caller just received `handle` from the OS and owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// The raw handle for a Win32 call; ownership stays with `handle`.
pub(crate) fn raw(handle: &OwnedHandle) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    handle.as_raw_handle()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process and its own PID report the same user, in `S-1-` form.
    #[test]
    fn current_user_matches_own_process() -> io::Result<()> {
        let me = current_user()?;
        assert_eq!(process_user(std::process::id())?, me);
        assert!(me.to_sddl()?.starts_with("S-1-"));
        elevated()?;
        Ok(())
    }
}
