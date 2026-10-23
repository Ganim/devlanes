//! Security identifiers and facts about this process's access token.

use std::fmt;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use std::str::FromStr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{
    GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    IsTokenRestricted, IsValidSid, PSID, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenIntegrityLevel,
    TokenIsAppContainer, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::util::{owned, raw};

/// A security identifier in its binary form. SIDs have exactly one binary
/// form, so byte equality is identity.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sid(Vec<u8>);

/// A string that is not a plain SID such as `S-1-5-18`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a security identifier")]
pub struct InvalidSid;

/// The most sub-authorities a SID can have (`SID_MAX_SUB_AUTHORITIES`).
const MAX_SUB_AUTHORITIES: usize = 15;

impl Sid {
    /// Parses the string form, such as `S-1-5-18`. Only canonical text is
    /// accepted: revision 1, decimal numbers without leading zeros, one to 15
    /// sub-authorities, and nothing around them. SDDL aliases such as `SY`
    /// are refused, so the result's text is safe to paste into SDDL even when
    /// the input came from a file or the registry.
    pub fn parse(text: &str) -> Result<Self, InvalidSid> {
        let rest = text.strip_prefix("S-1-").ok_or(InvalidSid)?;
        let mut parts = rest.split('-');
        let authority = parts.next().ok_or(InvalidSid)?;
        let authority = match authority.strip_prefix("0x") {
            // Windows prints authorities of 2^32 and above as 12 hex digits.
            Some(hex) if hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                let value = u64::from_str_radix(hex, 16).map_err(|_| InvalidSid)?;
                if value < 1 << 32 || hex.bytes().any(|b| b.is_ascii_lowercase()) {
                    return Err(InvalidSid);
                }
                value
            }
            Some(_) => return Err(InvalidSid),
            None => u64::from(number(authority)?),
        };
        let subs = parts.map(number).collect::<Result<Vec<u32>, _>>()?;
        if subs.is_empty() || subs.len() > MAX_SUB_AUTHORITIES {
            return Err(InvalidSid);
        }
        let mut bytes = Vec::with_capacity(8 + 4 * subs.len());
        bytes.push(1);
        bytes.push(subs.len() as u8);
        bytes.extend_from_slice(&authority.to_be_bytes()[2..]);
        for sub in subs {
            bytes.extend_from_slice(&sub.to_le_bytes());
        }
        Ok(Self(bytes))
    }

    /// `NT AUTHORITY\SYSTEM` (`S-1-5-18`).
    pub fn local_system() -> Self {
        Self(vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0])
    }

    /// `BUILTIN\Administrators` (`S-1-5-32-544`).
    pub fn administrators() -> Self {
        Self(vec![1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 0x02, 0, 0])
    }

    /// `Everyone` (`S-1-1-0`).
    pub fn everyone() -> Self {
        Self(vec![1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0])
    }

    /// `CREATOR OWNER` (`S-1-3-0`), a placeholder that inheritance replaces
    /// with the owner of each new child.
    pub fn creator_owner() -> Self {
        Self(vec![1, 1, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0])
    }

    /// The binary form.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

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
}

/// A decimal `u32` without sign or leading zeros.
fn number(text: &str) -> Result<u32, InvalidSid> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if !canonical {
        return Err(InvalidSid);
    }
    text.parse().map_err(|_| InvalidSid)
}

impl FromStr for Sid {
    type Err = InvalidSid;

    /// Same as [`Sid::parse`].
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for Sid {
    /// The string form Windows uses, such as `S-1-5-21-...`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut authority = [0_u8; 8];
        authority[2..].copy_from_slice(&self.0[2..8]);
        let authority = u64::from_be_bytes(authority);
        if authority < 1 << 32 {
            write!(f, "S-{}-{authority}", self.0[0])?;
        } else {
            write!(f, "S-{}-0x{authority:012X}", self.0[0])?;
        }
        for sub in self.0[8..].as_chunks::<4>().0 {
            write!(f, "-{}", u32::from_le_bytes(*sub))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sid {
    /// Same as `Display`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// The user this process runs as. Cached: the process token's user never
/// changes (impersonation only changes thread tokens).
pub fn current_user() -> io::Result<Sid> {
    static USER: OnceLock<Sid> = OnceLock::new();
    if let Some(user) = USER.get() {
        return Ok(user.clone());
    }
    let user = token_user(&current_process_token()?)?;
    Ok(USER.get_or_init(|| user).clone())
}

/// The integrity level of a normal, unelevated process.
pub const MEDIUM_INTEGRITY: u32 = 0x2000;
/// The integrity level of an elevated process.
pub const HIGH_INTEGRITY: u32 = 0x3000;

/// How far this process's token is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenFacts {
    /// Running as administrator under UAC.
    pub elevated: bool,
    /// The mandatory integrity level RID: 0x1000 low, [`MEDIUM_INTEGRITY`],
    /// [`HIGH_INTEGRITY`], 0x4000 system.
    pub integrity: u32,
    /// A restricted token (restricting SIDs present).
    pub restricted: bool,
    /// Running in an AppContainer sandbox.
    pub app_container: bool,
}

/// The facts of this process's token.
pub fn current_facts() -> io::Result<TokenFacts> {
    let token = current_process_token()?;
    let label = information(&token, TokenIntegrityLevel)?;
    // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL whose SID points into
    // the same buffer, alive for these calls.
    let integrity = unsafe {
        let label = &*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = label.Label.Sid;
        if IsValidSid(sid) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid integrity label",
            ));
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "integrity label without a level",
            ));
        }
        *GetSidSubAuthority(sid, u32::from(count) - 1)
    };
    let container = information(&token, TokenIsAppContainer)?;
    // SAFETY: the token is open with TOKEN_QUERY.
    let restricted = unsafe { IsTokenRestricted(raw(&token)) } != 0;
    Ok(TokenFacts {
        elevated: is_elevated(&token)?,
        integrity,
        restricted,
        app_container: container
            .first()
            .is_some_and(|&value| value & 0xffff_ffff != 0),
    })
}

/// Whether this process runs elevated (as administrator under UAC).
pub fn elevated() -> io::Result<bool> {
    is_elevated(&current_process_token()?)
}

/// Reads `TokenElevation`.
fn is_elevated(token: &OwnedHandle) -> io::Result<bool> {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0;
    // SAFETY: `token` has TOKEN_QUERY and `elevation` is the structure
    // TokenElevation fills, with its size passed.
    let ok = unsafe {
        GetTokenInformation(
            raw(token),
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
fn token_user(token: &OwnedHandle) -> io::Result<Sid> {
    let buffer = information(token, TokenUser)?;
    // SAFETY: the call filled a TOKEN_USER at the start of the buffer; its SID
    // points into the same buffer, which outlives the copy.
    unsafe {
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        Sid::copy_from(user.User.Sid)
    }
}

/// A token information class read into a buffer aligned for any of the
/// structures it may hold.
fn information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut needed = 0;
    // SAFETY: a size query with no buffer; it fails and stores the size needed.
    unsafe { GetTokenInformation(raw(token), class, ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds at least `needed` writable, 8-aligned bytes.
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

/// This process's access token, for queries.
fn current_process_token() -> io::Result<OwnedHandle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle needs no closing; `token` receives a new handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    owned(token)
}
