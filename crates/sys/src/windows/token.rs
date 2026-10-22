//! Security identifiers of processes, threads and pipe peers.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
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

    /// Parses the string form, such as `S-1-5-18`. Anything that is not a plain
    /// SID is refused, so the result's [`Sid::to_sddl`] is safe to paste into SDDL
    /// even when `text` came from a file or the registry. SDDL aliases such as
    /// `SY` are refused too.
    pub fn parse(text: &str) -> io::Result<Self> {
        let plain = text.len() <= 256
            && text.strip_prefix("S-1-").is_some_and(|rest| {
                rest.split('-')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            });
        if !plain {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{text:?} is not a security identifier"),
            ));
        }
        let wide = super::security::wide(std::ffi::OsStr::new(text));
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; on success `sid` receives a LocalAlloc'ed
        // SID that is copied and freed below.
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &raw mut sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{text:?} is not a security identifier: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        // SAFETY: the call succeeded, so `sid` is a valid SID until freed.
        let copied = unsafe { Self::copy_from(sid) };
        // SAFETY: allocated by ConvertStringSidToSidW with LocalAlloc.
        unsafe { LocalFree(sid) };
        copied
    }

    /// `NT AUTHORITY\SYSTEM` (`S-1-5-18`), the account services run as by default.
    pub fn local_system() -> Self {
        // S-1-5-18: revision 1, one sub-authority, authority 5 (NT), RID 18.
        Self(vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0])
    }

    /// `BUILTIN\Administrators` (`S-1-5-32-544`).
    pub fn administrators() -> Self {
        // Revision 1, two sub-authorities, authority 5, RIDs 32 and 544.
        Self(vec![1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 0x02, 0, 0])
    }

    /// Whether the SID has the shape of a person's account rather than a
    /// built-in account or a well-known group: a local, Microsoft or domain
    /// account (`S-1-5-21-` and four numbers) or a Microsoft Entra ID account
    /// (`S-1-12-1-` and four numbers). Domain groups share the first shape;
    /// [`Sid::names_a_user`] tells them apart when the account database answers.
    pub fn has_account_shape(&self) -> bool {
        let Ok(text) = self.to_sddl() else {
            return false;
        };
        ["S-1-5-21-", "S-1-12-1-"].iter().any(|prefix| {
            text.strip_prefix(prefix)
                .is_some_and(|rest| rest.split('-').count() == 4)
        })
    }

    /// Whether the account database (`LookupAccountSidW`) says the SID is a
    /// user, not a group, alias or well-known SID. Needs the account's domain to
    /// answer, which is the case for the logged-on user; an unknown SID is an
    /// error.
    pub fn names_a_user(&self) -> io::Result<bool> {
        use windows_sys::Win32::Security::{LookupAccountSidW, SID_NAME_USE, SidTypeUser};
        let mut name = vec![0u16; 256];
        let mut domain = vec![0u16; 256];
        for _ in 0..2 {
            let mut name_len = name.len() as u32;
            let mut domain_len = domain.len() as u32;
            let mut kind: SID_NAME_USE = 0;
            // SAFETY: the SID points into `self`; both buffers hold the lengths
            // passed, which the call updates; `kind` is writable.
            let ok = unsafe {
                LookupAccountSidW(
                    ptr::null(),
                    self.as_psid(),
                    name.as_mut_ptr(),
                    &raw mut name_len,
                    domain.as_mut_ptr(),
                    &raw mut domain_len,
                    &raw mut kind,
                )
            };
            if ok != 0 {
                return Ok(kind == SidTypeUser);
            }
            let error = io::Error::last_os_error();
            // ERROR_INSUFFICIENT_BUFFER: the lengths now hold what is needed.
            if error.raw_os_error() != Some(122) {
                return Err(error);
            }
            name.resize(name_len as usize, 0);
            domain.resize(domain_len as usize, 0);
        }
        Err(io::Error::other("account name kept growing"))
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
    token_user(&process_token(pid)?)
}

/// Who a token speaks for, and how far it is trusted. Windows can run code of
/// the same user at a lower trust (low integrity, AppContainer, restricted
/// tokens), so the user alone does not say a peer is as trusted as this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenFacts {
    pub user: Sid,
    /// The mandatory integrity level RID: 0x2000 medium, 0x3000 high, 0x1000 low.
    pub integrity: u32,
    pub app_container: bool,
    pub restricted: bool,
}

/// The medium integrity level, what a normal unelevated process runs at.
pub const MEDIUM_INTEGRITY: u32 = 0x2000;

impl TokenFacts {
    /// Whether a peer with these facts is trusted as much as `own`: the same user,
    /// not sandboxed, and at least the same integrity level. The daemon's rule for
    /// its clients.
    pub fn as_trusted_as(&self, own: &Self) -> bool {
        self.user == own.user
            && !self.app_container
            && !self.restricted
            && self.integrity >= own.integrity
    }

    /// Whether a daemon with these facts may serve a client with `own`: the same
    /// user, not sandboxed, at least medium integrity. An elevated client may use
    /// an unelevated daemon, which only takes away rights it did not need.
    pub fn may_serve(&self, own: &Self) -> bool {
        self.user == own.user
            && !self.app_container
            && !self.restricted
            && self.integrity >= MEDIUM_INTEGRITY
    }
}

/// This process's token facts.
pub fn current_facts() -> io::Result<TokenFacts> {
    token_facts(&current_process_token()?)
}

/// The token facts of process `pid`.
pub fn process_facts(pid: u32) -> io::Result<TokenFacts> {
    token_facts(&process_token(pid)?)
}

/// Reads user, integrity level, AppContainer and restriction from `token`.
pub(crate) fn token_facts(token: &OwnedHandle) -> io::Result<TokenFacts> {
    use windows_sys::Win32::Security::{
        IsTokenRestricted, TOKEN_MANDATORY_LABEL, TokenIntegrityLevel, TokenIsAppContainer,
    };
    let label = token_information(token, TokenIntegrityLevel)?;
    // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL whose SID points into it.
    let label = unsafe {
        let label = &*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        Sid::copy_from(label.Label.Sid)
    }?;
    // The level is the label SID's last sub-authority, as in `S-1-16-8192`. A
    // level that cannot be read is an error: reading it as 0 for this process
    // would make every peer look trusted.
    let label = label.to_sddl()?;
    let integrity = label
        .strip_prefix("S-1-16-")
        .and_then(|rid| rid.parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected integrity label {label}")))?;
    let container = token_information(token, TokenIsAppContainer)?;
    // SAFETY: the token is open with query rights.
    let restricted = unsafe { IsTokenRestricted(raw(token)) } != 0;
    Ok(TokenFacts {
        user: token_user(token)?,
        integrity,
        app_container: container.first().is_some_and(|&value| value != 0),
        restricted,
    })
}

/// A token information class read into an aligned buffer.
fn token_information(
    token: &OwnedHandle,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> io::Result<Vec<u64>> {
    let mut needed = 0;
    // SAFETY: a size query with no buffer; it fails with the needed size.
    unsafe { GetTokenInformation(raw(token), class, ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds `needed` writable, aligned bytes.
    let ok = unsafe {
        GetTokenInformation(
            raw(token),
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

/// The access token of process `pid`, for queries.
fn process_token(pid: u32) -> io::Result<OwnedHandle> {
    // SAFETY: plain call; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let process = owned(process)?;
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is an open handle with query rights; `token` receives a new handle.
    if unsafe { OpenProcessToken(raw(&process), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    owned(token)
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

/// Why the daemon does not start from an elevated process, as a startup reason
/// (`CODE: message`) that clients turn into the `daemon_elevated` reason.
pub const ELEVATED_REASON: &str = "DAEMON_ELEVATED: the daemon refuses to run elevated; run devlanes from a terminal that is not running as administrator";

/// Whether the daemon must not start from this process: it is elevated, so every
/// unit would inherit administrator rights, and `DEVLANES_ALLOW_ELEVATED=1` (for
/// CI runners that only run elevated) is not set.
pub fn elevation_refused() -> io::Result<bool> {
    if std::env::var_os("DEVLANES_ALLOW_ELEVATED").is_some_and(|value| value == "1") {
        return Ok(false);
    }
    elevated()
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

/// This process's access token, for queries.
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
        let facts = current_facts()?;
        assert_eq!(facts, process_facts(std::process::id())?);
        assert!(facts.integrity >= 0x2000, "{facts:?}");
        assert!(facts.as_trusted_as(&facts));
        let lower = TokenFacts {
            integrity: 0x1000,
            ..facts.clone()
        };
        assert!(!lower.as_trusted_as(&facts));
        Ok(())
    }

    /// SIDs parse from their string form; aliases and anything that could carry
    /// SDDL syntax are refused.
    #[test]
    fn sids_parse_strictly() -> io::Result<()> {
        let me = current_user()?;
        assert_eq!(Sid::parse(&me.to_sddl()?)?, me);
        assert_eq!(Sid::parse("S-1-5-18")?, Sid::local_system());
        assert_eq!(Sid::local_system().to_sddl()?, "S-1-5-18");
        assert_eq!(Sid::administrators().to_sddl()?, "S-1-5-32-544");
        for bad in [
            "",
            "SY",
            "BA",
            "S-1-5-21)(A;;GA;;;WD",
            "S-1-5-18 ",
            "s-1-5-18",
            "S-1-",
            "S-1-5-18-",
        ] {
            assert_eq!(
                Sid::parse(bad).err().map(|error| error.kind()),
                Some(io::ErrorKind::InvalidInput),
                "{bad:?}"
            );
        }
        Ok(())
    }

    /// The current user is an account; SYSTEM, Administrators, Everyone and
    /// Authenticated Users are not, by shape or by the account database.
    #[test]
    fn accounts_are_told_from_groups_and_services() -> io::Result<()> {
        let me = current_user()?;
        assert!(me.has_account_shape(), "{me:?}");
        assert!(me.names_a_user()?, "{me:?}");
        for other in ["S-1-5-18", "S-1-5-32-544", "S-1-1-0", "S-1-5-11"] {
            let sid = Sid::parse(other)?;
            assert!(!sid.has_account_shape(), "{other}");
            assert!(!sid.names_a_user()?, "{other}");
        }
        assert!(Sid::parse("S-1-5-21-1-2-3-1001")?.has_account_shape());
        assert!(Sid::parse("S-1-12-1-1-2-3-4")?.has_account_shape());
        assert!(!Sid::parse("S-1-5-21-1-2-3")?.has_account_shape());
        assert!(!Sid::parse("S-1-5-21-1-2-3-4-5")?.has_account_shape());
        Ok(())
    }
}
