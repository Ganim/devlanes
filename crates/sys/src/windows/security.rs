//! Security descriptors for objects only the current user may open, and checks that
//! a directory is private to that user.

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, OWNER_SECURITY_INFORMATION, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

use super::token::{self, Sid};

/// `ACCESS_ALLOWED_ACE_TYPE`.
const ACCESS_ALLOWED: u8 = 0;
/// `FILE_ATTRIBUTE_REPARSE_POINT`, set on symlinks and junctions.
const REPARSE_POINT: u32 = 0x400;
/// Accounts that may appear on a private directory besides its user: SYSTEM,
/// Administrators, and CREATOR OWNER (inherited entries that apply to the creator).
const TRUSTED: [&str; 3] = ["S-1-5-18", "S-1-5-32-544", "S-1-3-0"];

/// A self-relative security descriptor allocated by the system, freed on drop.
pub struct SecurityDescriptor(*mut c_void);

// SAFETY: the descriptor is immutable heap memory owned by this value.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; nothing mutates it after creation.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// Parses an SDDL string such as `D:P(A;;GA;;;S-1-5-...)`.
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = wide(OsStr::new(sddl));
        let mut descriptor = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; on success `descriptor` receives a
        // LocalAlloc'ed descriptor that `Drop` frees.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// `SECURITY_ATTRIBUTES` that keep their descriptor alive and are never inherited.
pub struct SecurityAttributes {
    attributes: SECURITY_ATTRIBUTES,
    _descriptor: SecurityDescriptor,
}

// SAFETY: the raw pointer inside points at the owned, immutable descriptor.
unsafe impl Send for SecurityAttributes {}
// SAFETY: as above.
unsafe impl Sync for SecurityAttributes {}

impl SecurityAttributes {
    /// Wraps `descriptor` for an object creation call.
    pub fn new(descriptor: SecurityDescriptor) -> Self {
        Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            },
            _descriptor: descriptor,
        }
    }

    /// Full access for the current user and nobody else, for kernel objects such
    /// as pipes and job objects. Lower-integrity processes of the same user are
    /// kept out by the default mandatory label.
    pub fn current_user_only() -> io::Result<Self> {
        let user = token::current_user()?.to_sddl()?;
        Ok(Self::new(SecurityDescriptor::from_sddl(&format!(
            "D:P(A;;GA;;;{user})"
        ))?))
    }

    /// A pointer for APIs that take `SECURITY_ATTRIBUTES*`; valid while `self` lives.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut SECURITY_ATTRIBUTES {
        &raw mut self.attributes
    }
}

/// Why a directory is not private to the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirProblem {
    /// A file or other non-directory is in the way.
    NotADirectory,
    /// A symlink or junction, which could point anywhere.
    ReparsePoint,
    /// Owned by another account (for example Administrators after an elevated run).
    Owner,
    /// Other accounts are granted access, such as `Authenticated Users` on
    /// directories created under a drive root.
    Acl,
}

/// Checks that `path` is a real directory owned by the current user, granting
/// access only to that user, SYSTEM and Administrators.
pub fn check_private_dir(path: &Path) -> io::Result<Result<(), DirProblem>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Ok(Err(DirProblem::ReparsePoint));
    }
    if !metadata.is_dir() {
        return Ok(Err(DirProblem::NotADirectory));
    }
    let user = token::current_user()?;
    let wide = wide(path.as_os_str());
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; on success `owner` and `dacl` point into
    // `descriptor`, which is freed below after their last use.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            &raw mut dacl,
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: `owner` and `dacl` point into `descriptor`, alive until freed below.
    let verdict = unsafe { judge(&user, owner, dacl) };
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(descriptor) };
    verdict
}

/// Compares the owner and walks the DACL.
///
/// # Safety
///
/// `owner` and `dacl` must be valid for the call (`dacl` may be null).
unsafe fn judge(user: &Sid, owner: PSID, dacl: *const ACL) -> io::Result<Result<(), DirProblem>> {
    // SAFETY: the caller guarantees `owner` is valid.
    if unsafe { Sid::copy_from(owner) }? != *user {
        return Ok(Err(DirProblem::Owner));
    }
    // A null DACL grants everyone full access.
    if dacl.is_null() {
        return Ok(Err(DirProblem::Acl));
    }
    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` is valid and `size` is the structure AclSizeInformation fills.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = user.to_sddl()?;
    for index in 0..size.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `index` is below the ACE count; `ace` points into `dacl`.
        if unsafe { GetAce(dacl, index, &raw mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // Deny entries only take access away, so only grants can make it too open.
        if header.AceType != ACCESS_ALLOWED {
            continue;
        }
        // SAFETY: an access-allowed ACE has the SID starting at `SidStart`.
        let sid = unsafe {
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            Sid::copy_from((&raw mut (*allowed).SidStart).cast())
        }?;
        let sid = sid.to_sddl()?;
        if sid != user && !TRUSTED.contains(&sid.as_str()) {
            return Ok(Err(DirProblem::Acl));
        }
    }
    Ok(Ok(()))
}

/// The protected DACL of a private directory: full access for the current user,
/// SYSTEM and Administrators, inherited by everything created inside.
fn private_descriptor() -> io::Result<SecurityDescriptor> {
    let user = token::current_user()?.to_sddl()?;
    SecurityDescriptor::from_sddl(&format!(
        "D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))
}

/// Creates `path` with the private DACL when missing (its parent must exist), then
/// checks it. An existing directory is only checked here; [`tighten_private_dir`]
/// repairs an own directory that is too open.
pub fn ensure_private_dir(path: &Path) -> io::Result<Result<(), DirProblem>> {
    let mut attributes = SecurityAttributes::new(private_descriptor()?);
    let wide = wide(path.as_os_str());
    // SAFETY: `wide` is NUL-terminated and `attributes` outlives the call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), attributes.as_mut_ptr()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    check_private_dir(path)
}

/// Replaces the DACL of `path` with the private one and stops inheritance from its
/// parent, the Windows counterpart of `chmod 700`. Only call it for a directory
/// this user owns: the check comes first, so another account's directory is never
/// changed.
pub fn tighten_private_dir(path: &Path) -> io::Result<()> {
    set_dacl(path, &private_descriptor()?)
}

/// Applies the DACL of `descriptor` to `path` as a protected DACL.
fn set_dacl(path: &Path, descriptor: &SecurityDescriptor) -> io::Result<()> {
    use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: `descriptor` is valid; `dacl` points into it while it lives.
    let ok = unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let wide = wide(path.as_os_str());
    // SAFETY: `wide` is NUL-terminated and `dacl` is valid for the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// A NUL-terminated UTF-16 copy for Win32 string parameters.
pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory devlanes creates is private; one with an extra grant is not.
    #[test]
    fn created_directories_are_private() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let private = temp.path().join("private");
        assert_eq!(ensure_private_dir(&private)?, Ok(()));
        assert_eq!(
            ensure_private_dir(&private)?,
            Ok(()),
            "existing is checked again"
        );
        let file = temp.path().join("file");
        std::fs::write(&file, "x")?;
        assert_eq!(check_private_dir(&file)?, Err(DirProblem::NotADirectory));
        let open = temp.path().join("open");
        std::fs::create_dir(&open)?;
        // Everyone (WD) read access, the classic "too open" grant.
        let user = token::current_user()?.to_sddl()?;
        let descriptor =
            SecurityDescriptor::from_sddl(&format!("D:P(A;OICI;FA;;;{user})(A;OICI;FR;;;WD)"))?;
        set_dacl(&open, &descriptor)?;
        assert_eq!(check_private_dir(&open)?, Err(DirProblem::Acl));
        tighten_private_dir(&open)?;
        assert_eq!(check_private_dir(&open)?, Ok(()), "tightened");
        Ok(())
    }

    /// A junction is refused, never followed. Junctions need no privilege to create.
    #[test]
    fn junctions_are_refused() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("target");
        std::fs::create_dir(&target)?;
        let link = temp.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()?;
        assert!(status.success(), "mklink /J failed");
        assert_eq!(check_private_dir(&link)?, Err(DirProblem::ReparsePoint));
        assert_eq!(ensure_private_dir(&link)?, Err(DirProblem::ReparsePoint));
        Ok(())
    }
}
