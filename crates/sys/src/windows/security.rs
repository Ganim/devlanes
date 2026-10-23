//! Security descriptors for objects only the current user may open, checks that
//! a directory or file is private to that user, and `icacls` commands that fix one.

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, GetFileInformationByHandle, READ_CONTROL,
    WRITE_DAC,
};

use super::token::{self, Sid};

/// `FILE_ATTRIBUTE_REPARSE_POINT`, set on symlinks and junctions.
const REPARSE_POINT: u32 = 0x400;
/// Accounts that may appear on a private directory besides its user: SYSTEM,
/// Administrators, and CREATOR OWNER (an inherited entry that only applies to
/// whoever creates a file inside).
const TRUSTED: [&str; 3] = ["S-1-5-18", "S-1-5-32-544", "S-1-3-0"];
/// Administrators, accepted as owner too: an elevated process owns what it
/// creates through this group, and an administrator can reach everything anyway.
const ADMINISTRATORS: &str = "S-1-5-32-544";

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

    /// The descriptor's DACL; valid while `self` lives.
    fn dacl(&self) -> io::Result<*mut ACL> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        // SAFETY: `self.0` is a valid descriptor; `dacl` points into it.
        let ok = unsafe {
            GetSecurityDescriptorDacl(self.0, &raw mut present, &raw mut dacl, &raw mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // Applying a missing or null DACL would grant everyone full access.
        if present == 0 || dacl.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "security descriptor has no access list",
            ));
        }
        Ok(dacl)
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
    /// as pipes and job objects. The medium mandatory label with no-read-up and
    /// no-write-up keeps lower-integrity processes of the same user (sandboxes,
    /// low-integrity browsers) out entirely.
    pub fn current_user_only() -> io::Result<Self> {
        let user = token::current_user()?.to_sddl()?;
        Ok(Self::new(SecurityDescriptor::from_sddl(&format!(
            "D:P(A;;GA;;;{user})S:(ML;;NWNR;;;ME)"
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
    /// Owned by another account.
    Owner,
    /// Other accounts are granted access, such as `Authenticated Users` on
    /// directories created under a drive root. Holds those accounts' SIDs; empty
    /// when access cannot be judged (a missing access list, an unknown entry type).
    Acl(Vec<String>),
}

/// Why a file is not private to the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileProblem {
    /// A directory or device is in the way.
    NotAFile,
    /// A symlink or other reparse point, which could point anywhere.
    ReparsePoint,
    /// The file has this many names; another one may sit in a directory other
    /// accounts can open, or be swapped for other contents there.
    HardLinks(u32),
    /// Owned by another account.
    Owner,
    /// Other accounts are granted access, such as an explicit `Everyone` grant.
    /// Holds those accounts' SIDs; empty when access cannot be judged.
    Acl(Vec<String>),
}

impl From<DirProblem> for FileProblem {
    /// The same finding about a file.
    fn from(problem: DirProblem) -> Self {
        match problem {
            DirProblem::NotADirectory => Self::NotAFile,
            DirProblem::ReparsePoint => Self::ReparsePoint,
            DirProblem::Owner => Self::Owner,
            DirProblem::Acl(others) => Self::Acl(others),
        }
    }
}

/// What [`ensure_private_dir`] found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateDir {
    /// It was missing and was created private.
    Created,
    /// It existed and was private already.
    Present,
    /// It existed empty, was this user's, and others had access; it now has the
    /// private access list.
    Tightened,
}

/// Checks that `path` is a real directory owned by the current user (or
/// Administrators), granting access only to that user, SYSTEM and Administrators.
/// A missing directory is an error of kind `NotFound`.
pub fn check_private_dir(path: &Path) -> io::Result<Result<(), DirProblem>> {
    let directory = match open_dir(path, false)? {
        Ok(directory) => directory,
        Err(problem) => return Ok(Err(problem)),
    };
    // SAFETY: the handle stays open for the call.
    unsafe { inspect(&directory, None) }
}

/// Checks that `path` is a regular file with a single name, owned by the current
/// user (or Administrators), granting access only to that user, SYSTEM and
/// Administrators: what a file created in a private directory inherits. The file
/// itself is opened, never a link's target. A missing file is an error of kind
/// `NotFound`.
///
/// Its directory needs [`check_private_dir`] as well: a private file in a
/// directory others can write to can be replaced.
pub fn check_private_file(path: &Path) -> io::Result<Result<(), FileProblem>> {
    let file = File::options()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        // Backup semantics lets a directory open too, so it is reported, not denied.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Ok(Err(FileProblem::ReparsePoint));
    }
    if !metadata.is_file() {
        return Ok(Err(FileProblem::NotAFile));
    }
    let links = link_count(&file)?;
    if links > 1 {
        return Ok(Err(FileProblem::HardLinks(links)));
    }
    // SAFETY: the handle stays open with READ_CONTROL for the call.
    Ok(unsafe { inspect(&file, None) }?.map_err(FileProblem::from))
}

/// How many names (hard links) the open file has.
fn link_count(file: &File) -> io::Result<u32> {
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the handle is owned by `file` and stays open for the call; `info`
    // points to writable memory of the structure's size.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled the whole structure.
    Ok(unsafe { info.assume_init() }.nNumberOfLinks)
}

/// `Everyone (S-1-1-0)` for the well-known groups that end up on directories
/// outside the user profile, the bare SID for any other account. The names are
/// English; Windows shows them translated per display language.
pub fn account_label(sid: &str) -> String {
    let name = match sid {
        "S-1-1-0" => "Everyone",
        "S-1-5-11" => "Authenticated Users",
        "S-1-5-32-545" => "Users",
        "S-1-5-32-546" => "Guests",
        "S-1-5-4" => "INTERACTIVE",
        "S-1-5-7" => "ANONYMOUS LOGON",
        _ => return sid.to_owned(),
    };
    format!("{name} ({sid})")
}

/// Two `icacls` commands that leave `path` and everything in it to this user,
/// SYSTEM and Administrators. The first makes the directory private: stops
/// inheritance, grants those three, and removes the explicit grants of `others`
/// (SIDs), which `/grant:r` would keep. The second resets every entry inside to
/// inherit from it; entries keep their old grants otherwise. (`/T` on the first
/// would copy directory-only inheritance flags onto files and lock them.)
///
/// SIDs instead of names, which Windows translates per display language. The
/// quotes work in cmd and PowerShell, but no separator works in both, so the
/// commands are joined by ", then ".
pub fn private_dir_fix(path: &Path, others: &[String]) -> io::Result<String> {
    let user = token::current_user()?.to_sddl()?;
    let path = path.display();
    let mut directory = format!(
        r#"icacls "{path}" /inheritance:r /grant:r "*{user}:(OI)(CI)F" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F""#
    );
    if !others.is_empty() {
        directory.push_str(" /remove:g");
        for sid in others {
            directory.push_str(&format!(r#" "*{sid}""#));
        }
    }
    Ok(format!(
        r#"{directory}, then icacls "{path}\*" /reset /T /C"#
    ))
}

/// An `icacls` command that drops a file's own grants and lets it inherit its
/// directory's access list again; enough once that directory is private.
pub fn private_file_fix(path: &Path) -> String {
    format!(r#"icacls "{}" /reset"#, path.display())
}

/// Creates `path` private when missing (its parent must exist) and checks it.
/// With `tighten`, an own empty directory that others can open gets the private
/// access list instead of being refused, the Windows counterpart of `chmod 700`.
///
/// A non-empty one is refused even with `tighten`. Changing a DACL through a
/// handle does not reach existing entries, which keep their old inherited grants
/// and stay reachable by full path (everyone may traverse); only a recursive
/// change by the user (`icacls ... /T`) fixes those.
///
/// The check and the change go through one handle opened without following links,
/// so the path cannot be swapped for a junction in between.
pub fn ensure_private_dir(
    path: &Path,
    tighten: bool,
) -> io::Result<Result<PrivateDir, DirProblem>> {
    let descriptor = private_descriptor()?;
    let mut attributes = SecurityAttributes::new(descriptor);
    let wide_path = wide(path.as_os_str());
    // SAFETY: `wide_path` is NUL-terminated and `attributes` outlives the call.
    let created = unsafe { CreateDirectoryW(wide_path.as_ptr(), attributes.as_mut_ptr()) } != 0;
    if !created {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    let directory = match open_dir(path, tighten)? {
        Ok(directory) => directory,
        Err(problem) => return Ok(Err(problem)),
    };
    let private = private_descriptor()?;
    let replace = (tighten && is_empty(path, &directory)?).then_some(&private);
    // SAFETY: the handle stays open for the call; `private` lives across it.
    let verdict = unsafe { inspect(&directory, replace) }?;
    Ok(match verdict {
        Ok(()) if created => Ok(PrivateDir::Created),
        Ok(()) => Ok(PrivateDir::Present),
        // Others could still add entries until the new list applied; anything that
        // appeared keeps its own owner and grants, so it is refused after all.
        Err(DirProblem::Acl(others)) if replace.is_some() => {
            if is_empty(path, &directory)? {
                Ok(PrivateDir::Tightened)
            } else {
                Err(DirProblem::Acl(others))
            }
        }
        Err(problem) => Err(problem),
    })
}

/// Whether the directory open as `directory` has no entries. The listing goes by
/// path, so it only counts when `path` still names the same directory afterwards.
fn is_empty(path: &Path, directory: &File) -> io::Result<bool> {
    let empty = std::fs::read_dir(path)?.next().is_none();
    Ok(empty && crate::path_identity(path)? == crate::file_identity(directory)?)
}

/// The protected access list of a private directory, inherited by everything
/// created inside, owned by the current user even when this process is elevated
/// (whose default owner would be Administrators).
fn private_descriptor() -> io::Result<SecurityDescriptor> {
    let user = token::current_user()?.to_sddl()?;
    SecurityDescriptor::from_sddl(&format!(
        "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))
}

/// Opens `path` itself (never a link's target) for reading its security, and for
/// changing its access list with `write_dac`. A link or a non-directory is a
/// problem, not an error.
fn open_dir(path: &Path, write_dac: bool) -> io::Result<Result<File, DirProblem>> {
    let access = READ_CONTROL | FILE_READ_ATTRIBUTES | if write_dac { WRITE_DAC } else { 0 };
    let file = File::options()
        .access_mode(access)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Ok(Err(DirProblem::ReparsePoint));
    }
    if !metadata.is_dir() {
        return Ok(Err(DirProblem::NotADirectory));
    }
    Ok(Ok(file))
}

/// Judges the owner and access list of `directory` (or a file). With `replace`, an
/// access-list problem on an acceptable owner is fixed by applying that
/// descriptor's DACL as a protected one, and still reported so the caller can
/// tell it happened.
///
/// # Safety
///
/// `directory` must be open with `READ_CONTROL`, plus `WRITE_DAC` with `replace`.
unsafe fn inspect(
    directory: &File,
    replace: Option<&SecurityDescriptor>,
) -> io::Result<Result<(), DirProblem>> {
    let handle = directory.as_raw_handle();
    // SAFETY: the caller opened the handle with READ_CONTROL.
    let security = unsafe { read_security(handle, SE_FILE_OBJECT) }?;
    let verdict = judge(&security)?;
    if let (Err(DirProblem::Acl(_)), Some(replace)) = (&verdict, replace) {
        let dacl = replace.dacl()?;
        // SAFETY: the handle is open with WRITE_DAC; `dacl` lives in `replace`.
        let status = unsafe {
            SetSecurityInfo(
                handle,
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
    }
    Ok(verdict)
}

/// Compares the owner and walks the access list.
fn judge(security: &Security) -> io::Result<Result<(), DirProblem>> {
    let user = token::current_user()?.to_sddl()?;
    // Volumes without security (FAT) report no owner.
    let Some(owner) = &security.owner else {
        return Ok(Err(DirProblem::Owner));
    };
    if *owner != user && owner != ADMINISTRATORS {
        return Ok(Err(DirProblem::Owner));
    }
    // A null access list grants everyone full access.
    let Some(dacl) = &security.dacl else {
        return Ok(Err(DirProblem::Acl(Vec::new())));
    };
    let mut others: Vec<String> = Vec::new();
    for entry in &dacl.allowed {
        if entry.sid != user
            && !TRUSTED.contains(&entry.sid.as_str())
            && !others.contains(&entry.sid)
        {
            others.push(entry.sid.clone());
        }
    }
    if others.is_empty() && !dacl.unjudged {
        Ok(Ok(()))
    } else {
        Ok(Err(DirProblem::Acl(others)))
    }
}

/// The owner and access list of an object, as SID strings.
struct Security {
    /// `None` on volumes without security (FAT).
    owner: Option<String>,
    /// `None` for a null access list, which grants everyone everything.
    dacl: Option<Dacl>,
}

/// The parts of an access list the checks judge.
struct Dacl {
    /// Allowed and allowed-callback entries.
    allowed: Vec<Allowed>,
    /// Whether an entry of another layout (object or compound grants) is present,
    /// which these checks cannot read and so must not pass.
    unjudged: bool,
}

/// One grant of an access list.
struct Allowed {
    sid: String,
    mask: u32,
    /// Inherit-only entries apply to children, not to the object itself.
    inherit_only: bool,
}

/// `INHERIT_ONLY_ACE`.
const INHERIT_ONLY: u8 = 0x08;

/// Reads the owner and access list of the object behind `handle`.
///
/// # Safety
///
/// `handle` must be open with `READ_CONTROL` and be an object of `kind`.
unsafe fn read_security(
    handle: std::os::windows::io::RawHandle,
    kind: windows_sys::Win32::Security::Authorization::SE_OBJECT_TYPE,
) -> io::Result<Security> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: the caller guarantees the handle and its access; on success `owner`
    // and `dacl` point into `descriptor`, which is freed below after their last use.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            kind,
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
    let security = unsafe { parse_security(owner, dacl) };
    // SAFETY: allocated by GetSecurityInfo.
    unsafe { LocalFree(descriptor) };
    security
}

/// Copies the owner and the readable entries of `dacl`.
///
/// # Safety
///
/// `owner` and `dacl` must be null or valid for the call.
unsafe fn parse_security(owner: PSID, dacl: *const ACL) -> io::Result<Security> {
    let owner = if owner.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees a non-null `owner` is valid.
        Some(unsafe { Sid::copy_from(owner) }?.to_sddl()?)
    };
    if dacl.is_null() {
        return Ok(Security { owner, dacl: None });
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
    let mut allowed = Vec::new();
    let mut unjudged = false;
    for index in 0..size.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `index` is below the ACE count; `ace` points into `dacl`.
        if unsafe { GetAce(dacl, index, &raw mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        match header.AceType {
            // Allowed and allowed-callback entries share the layout with the SID
            // at `SidStart`.
            0 | 9 => {
                // SAFETY: as stated for these two types.
                let (sid, mask) = unsafe {
                    let entry = ace.cast::<ACCESS_ALLOWED_ACE>();
                    (
                        Sid::copy_from((&raw mut (*entry).SidStart).cast())?,
                        (*entry).Mask,
                    )
                };
                allowed.push(Allowed {
                    sid: sid.to_sddl()?,
                    mask,
                    inherit_only: header.AceFlags & INHERIT_ONLY != 0,
                });
            }
            // Deny entries only take access away.
            1 | 6 | 10 | 12 => {}
            // Object and compound grants have other layouts; a private directory
            // never needs them, so their presence alone fails the check.
            _ => unjudged = true,
        }
    }
    Ok(Security {
        owner,
        dacl: Some(Dacl { allowed, unjudged }),
    })
}

/// `NT SERVICE\TrustedInstaller`, owner of Windows' own program directories.
const TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Accounts that may own and write a privileged program and its directory:
/// SYSTEM, Administrators and TrustedInstaller.
const ADMIN_WRITERS: [&str; 3] = ["S-1-5-18", ADMINISTRATORS, TRUSTED_INSTALLER];

/// Rights that change a file or directory, its entries, or who may access it:
/// write and append data (add file and add subdirectory on directories),
/// extended attributes, delete child, attributes, delete, change the access list
/// or owner, and the generic write and all rights.
const WRITE_RIGHTS: u32 =
    0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x4000_0000 | 0x1000_0000;

/// Why a privileged program or its directory could be changed by others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminOnlyProblem {
    /// A symlink or junction, which could point anywhere.
    ReparsePoint,
    /// A directory where a file was expected, or the other way round.
    WrongKind,
    /// Owned by this account, not SYSTEM, Administrators or TrustedInstaller.
    Owner(String),
    /// These accounts may change it; empty when the access list cannot be judged
    /// (missing, or with entries of an unknown layout).
    Writers(Vec<String>),
}

impl std::fmt::Display for AdminOnlyProblem {
    /// What is wrong, naming accounts by SID.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReparsePoint => f.write_str("is a symlink or junction"),
            Self::WrongKind => f.write_str("is not the expected kind of file system entry"),
            Self::Owner(owner) => write!(
                f,
                "is owned by {}, not SYSTEM, Administrators or TrustedInstaller",
                account_label(owner)
            ),
            Self::Writers(writers) if writers.is_empty() => {
                f.write_str("has an access list that cannot be checked")
            }
            Self::Writers(writers) => {
                let names: Vec<String> = writers.iter().map(|sid| account_label(sid)).collect();
                write!(f, "can be changed by {}", names.join(", "))
            }
        }
    }
}

/// Checks that `path` (a directory when `directory`, else a regular file) is
/// owned by SYSTEM, Administrators or TrustedInstaller and that nobody else may
/// change it: the Windows counterpart of "root-owned, not group or world
/// writable" for a program that runs as SYSTEM. The entry itself is opened,
/// never a link's target. Grants that only apply to children (inherit-only) are
/// not counted. A missing path is an error of kind `NotFound`.
pub fn check_admin_only(path: &Path, directory: bool) -> io::Result<Result<(), AdminOnlyProblem>> {
    let entry = File::options()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = entry.metadata()?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Ok(Err(AdminOnlyProblem::ReparsePoint));
    }
    let kind_matches = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if !kind_matches {
        return Ok(Err(AdminOnlyProblem::WrongKind));
    }
    // SAFETY: the handle is open with READ_CONTROL for the call.
    let security = unsafe { read_security(entry.as_raw_handle(), SE_FILE_OBJECT) }?;
    Ok(judge_admin_only(&security))
}

/// The verdict of [`check_admin_only`] on an object's security.
fn judge_admin_only(security: &Security) -> Result<(), AdminOnlyProblem> {
    match &security.owner {
        Some(owner) if ADMIN_WRITERS.contains(&owner.as_str()) => {}
        Some(owner) => return Err(AdminOnlyProblem::Owner(owner.clone())),
        None => return Err(AdminOnlyProblem::Owner("nobody".into())),
    }
    let Some(dacl) = &security.dacl else {
        return Err(AdminOnlyProblem::Writers(Vec::new()));
    };
    if dacl.unjudged {
        return Err(AdminOnlyProblem::Writers(Vec::new()));
    }
    let mut writers: Vec<String> = Vec::new();
    for entry in &dacl.allowed {
        if entry.inherit_only
            || entry.mask & WRITE_RIGHTS == 0
            || ADMIN_WRITERS.contains(&entry.sid.as_str())
            || writers.contains(&entry.sid)
        {
            continue;
        }
        writers.push(entry.sid.clone());
    }
    if writers.is_empty() {
        Ok(())
    } else {
        Err(AdminOnlyProblem::Writers(writers))
    }
}

/// The access list of a directory for programs that run as SYSTEM: owned by
/// Administrators, full control for SYSTEM and Administrators, read and execute
/// for Users, inherited by everything created inside.
pub const ADMIN_ONLY_DIR_SDDL: &str =
    "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

/// Creates `path` (its parent must exist) with [`ADMIN_ONLY_DIR_SDDL`]. Returns
/// `false` when it already existed, untouched; check it with
/// [`check_admin_only`]. Needs an elevated process, since only one can name
/// Administrators as owner.
pub fn create_admin_only_dir(path: &Path) -> io::Result<bool> {
    let mut attributes =
        SecurityAttributes::new(SecurityDescriptor::from_sddl(ADMIN_ONLY_DIR_SDDL)?);
    let wide_path = wide(path.as_os_str());
    // SAFETY: `wide_path` is NUL-terminated and `attributes` outlives the call.
    if unsafe { CreateDirectoryW(wide_path.as_ptr(), attributes.as_mut_ptr()) } != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// The access list of a program file that runs as SYSTEM: owned by
/// Administrators, full control for SYSTEM and Administrators, read and execute
/// for Users, and nothing inherited from its directory.
pub const ADMIN_ONLY_FILE_SDDL: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)";

/// Creates the file `path`, which must not exist (a link of that name counts),
/// with `descriptor` as its owner and access list from the start, and opens it
/// for reading and writing. Setting them after creation would leave a moment in
/// which the file has the default owner: under the "default owner: object
/// creator" policy that is the elevated user, who could change the access list.
///
/// Naming an owner other than this account (Administrators in
/// [`ADMIN_ONLY_FILE_SDDL`]) needs an elevated process; otherwise it fails with
/// `ERROR_INVALID_OWNER` (1307) and nothing is created.
pub fn create_new_with_descriptor(
    path: &Path,
    descriptor: &SecurityDescriptor,
) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide_path = wide(path.as_os_str());
    // SAFETY: `wide_path` is NUL-terminated and `attributes` (with the descriptor
    // it points to, borrowed from the caller) outlives the call; the template
    // handle is null as allowed. CREATE_NEW with OPEN_REPARSE_POINT fails on any
    // existing name instead of following it.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle was just created, is valid, and is owned by the File.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// The owner of the kernel object behind `handle` (a pipe, for instance), which
/// must be open with `READ_CONTROL`.
pub(crate) fn kernel_object_owner(
    handle: std::os::windows::io::RawHandle,
) -> io::Result<Option<String>> {
    use windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT;
    // SAFETY: the caller guarantees an open handle with READ_CONTROL.
    Ok(unsafe { read_security(handle, SE_KERNEL_OBJECT) }?.owner)
}

/// A NUL-terminated UTF-16 copy for Win32 string parameters.
pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `icacls` on `path` to set up access lists.
    fn icacls(path: &Path, args: &[&str]) -> io::Result<()> {
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()?;
        assert!(status.success(), "icacls {args:?}");
        Ok(())
    }

    /// A directory devlanes creates is private; one with an extra grant is not,
    /// names that grant, and is fixed only when tightening is asked for.
    #[test]
    fn created_directories_are_private() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let private = temp.path().join("private");
        assert_eq!(
            ensure_private_dir(&private, false)?,
            Ok(PrivateDir::Created)
        );
        assert_eq!(
            ensure_private_dir(&private, false)?,
            Ok(PrivateDir::Present)
        );
        let file = temp.path().join("file");
        std::fs::write(&file, "x")?;
        assert_eq!(check_private_dir(&file)?, Err(DirProblem::NotADirectory));
        let open = temp.path().join("open");
        std::fs::create_dir(&open)?;
        // Everyone (S-1-1-0), read access.
        icacls(&open, &["/grant", "*S-1-1-0:(OI)(CI)R"])?;
        let everyone = DirProblem::Acl(vec!["S-1-1-0".to_owned()]);
        assert_eq!(check_private_dir(&open)?, Err(everyone.clone()));
        assert_eq!(ensure_private_dir(&open, false)?, Err(everyone));
        assert_eq!(ensure_private_dir(&open, true)?, Ok(PrivateDir::Tightened));
        assert_eq!(check_private_dir(&open)?, Ok(()), "tightened");
        Ok(())
    }

    /// A non-empty directory is never tightened: its entries would keep the old
    /// grants.
    #[test]
    fn non_empty_directories_are_not_tightened() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let open = temp.path().join("open");
        std::fs::create_dir(&open)?;
        std::fs::write(open.join("state.db"), "x")?;
        icacls(&open, &["/grant", "*S-1-1-0:(OI)(CI)R"])?;
        let everyone = DirProblem::Acl(vec!["S-1-1-0".to_owned()]);
        assert_eq!(ensure_private_dir(&open, true)?, Err(everyone.clone()));
        assert_eq!(check_private_dir(&open)?, Err(everyone), "left unchanged");
        Ok(())
    }

    /// A file in a private directory is private; an explicit grant, a second
    /// name (hard link) or a directory in its place is not.
    #[test]
    fn private_files() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("private");
        ensure_private_dir(&directory, false)?.map_err(|p| io::Error::other(format!("{p:?}")))?;
        let key = directory.join("key.pem");
        std::fs::write(&key, "x")?;
        assert_eq!(check_private_file(&key)?, Ok(()));
        assert_eq!(check_private_file(&directory)?, Err(FileProblem::NotAFile));
        assert_eq!(
            check_private_file(&directory.join("missing"))
                .err()
                .map(|e| e.kind()),
            Some(io::ErrorKind::NotFound)
        );

        let linked = directory.join("linked.pem");
        std::fs::write(&linked, "x")?;
        std::fs::hard_link(&linked, temp.path().join("elsewhere.pem"))?;
        assert_eq!(check_private_file(&linked)?, Err(FileProblem::HardLinks(2)));

        // Everyone (S-1-1-0), read access, on the file alone.
        icacls(&key, &["/grant", "*S-1-1-0:R"])?;
        assert_eq!(
            check_private_file(&key)?,
            Err(FileProblem::Acl(vec!["S-1-1-0".to_owned()]))
        );
        icacls(&key, &["/reset"])?;
        assert_eq!(
            check_private_file(&key)?,
            Ok(()),
            "{}",
            private_file_fix(&key)
        );
        Ok(())
    }

    /// The fix names this user, SYSTEM and Administrators, removes the other
    /// accounts' grants and resets what is inside; labels name well-known groups.
    #[test]
    fn fixes_and_labels() -> io::Result<()> {
        let user = token::current_user()?.to_sddl()?;
        let fix = private_dir_fix(Path::new(r"D:\ca"), &["S-1-5-11".to_owned()])?;
        assert_eq!(
            fix,
            format!(
                r#"icacls "D:\ca" /inheritance:r /grant:r "*{user}:(OI)(CI)F" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" /remove:g "*S-1-5-11", then icacls "D:\ca\*" /reset /T /C"#
            )
        );
        assert_eq!(account_label("S-1-5-11"), "Authenticated Users (S-1-5-11)");
        assert_eq!(account_label("S-1-5-21-1-2-3-1001"), "S-1-5-21-1-2-3-1001");
        Ok(())
    }

    /// Windows' own program directories pass the admin-only check; what this
    /// user owns fails it on the owner, and an extra grant names the writer.
    #[test]
    fn admin_only_checks() -> io::Result<()> {
        let program_files = super::super::dirs::program_files()?;
        assert_eq!(check_admin_only(&program_files, true)?, Ok(()));
        let system = super::super::dirs::system_directory()?;
        assert_eq!(check_admin_only(&system.join("cmd.exe"), false)?, Ok(()));
        assert_eq!(
            check_admin_only(&system, false)?,
            Err(AdminOnlyProblem::WrongKind)
        );
        let user = token::current_user()?.to_sddl()?;
        let temp = tempfile::tempdir()?;
        // Owned by this user whether or not the process is elevated.
        let private = temp.path().join("private");
        ensure_private_dir(&private, false)?.map_err(|p| io::Error::other(format!("{p:?}")))?;
        assert_eq!(
            check_admin_only(&private, true)?,
            Err(AdminOnlyProblem::Owner(user.clone()))
        );
        // A plain new directory has the token's default owner: this user, or
        // Administrators when elevated (as on CI runners). Then the user's grant
        // inherited from %TEMP% is what makes it unsafe, since the same user
        // can write it unelevated.
        let expected = if token::elevated()? {
            AdminOnlyProblem::Writers(vec![user.clone()])
        } else {
            AdminOnlyProblem::Owner(user.clone())
        };
        assert_eq!(check_admin_only(temp.path(), true)?, Err(expected));
        // The verdict on an admin-owned object with an extra writer.
        let security = Security {
            owner: Some(ADMINISTRATORS.to_owned()),
            dacl: Some(Dacl {
                allowed: vec![
                    Allowed {
                        sid: "S-1-5-18".into(),
                        mask: 0x1f01ff,
                        inherit_only: false,
                    },
                    Allowed {
                        sid: "S-1-5-32-545".into(),
                        mask: 0x1200a9,
                        inherit_only: false,
                    },
                    // CREATOR OWNER full control applies to children only.
                    Allowed {
                        sid: "S-1-3-0".into(),
                        mask: 0x1000_0000,
                        inherit_only: true,
                    },
                    Allowed {
                        sid: user.clone(),
                        mask: 0x2,
                        inherit_only: false,
                    },
                ],
                unjudged: false,
            }),
        };
        assert_eq!(
            judge_admin_only(&security),
            Err(AdminOnlyProblem::Writers(vec![user]))
        );
        let unjudged = Security {
            owner: Some("S-1-5-18".into()),
            dacl: Some(Dacl {
                allowed: Vec::new(),
                unjudged: true,
            }),
        };
        assert_eq!(
            judge_admin_only(&unjudged),
            Err(AdminOnlyProblem::Writers(Vec::new()))
        );
        Ok(())
    }

    /// A file created with a descriptor has that owner and protected access list
    /// from the start; an existing name is refused. Naming Administrators as the
    /// owner needs elevation, and without it nothing is created.
    #[test]
    fn files_are_created_with_their_descriptor() -> io::Result<()> {
        let user = token::current_user()?.to_sddl()?;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("staged.exe");
        let own = SecurityDescriptor::from_sddl(&format!("O:{user}D:P(A;;FA;;;{user})"))?;
        let mut file = create_new_with_descriptor(&path, &own)?;
        std::io::Write::write_all(&mut file, b"MZ")?;
        drop(file);
        assert_eq!(std::fs::read(&path)?, b"MZ");
        assert_eq!(check_private_file(&path)?, Ok(()));
        let error = create_new_with_descriptor(&path, &own).expect_err("exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        if !token::elevated()? {
            let admin = SecurityDescriptor::from_sddl(ADMIN_ONLY_FILE_SDDL)?;
            let refused = temp.path().join("admin.exe");
            let error = create_new_with_descriptor(&refused, &admin).expect_err("not elevated");
            assert_eq!(error.raw_os_error(), Some(1307), "{error}");
            assert!(!refused.exists());
        }
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
        assert_eq!(
            check_admin_only(&link, true)?,
            Err(AdminOnlyProblem::ReparsePoint)
        );
        assert_eq!(
            ensure_private_dir(&link, true)?,
            Err(DirProblem::ReparsePoint)
        );
        Ok(())
    }
}
