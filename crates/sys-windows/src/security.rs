//! Security descriptors, and directories and files private to the current user.
//!
//! Private means: owned by the current user, and an access list that grants
//! access only to that user, SYSTEM and Administrators. Every check goes
//! through a handle opened on the path itself, never a link's target, so the
//! path cannot be swapped for a junction between the check and a change.

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_NO_MORE_FILES, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, READ_CONTROL, WRITE_DAC,
};

use crate::token::{self, Sid};
use crate::util::{raw, wide, win32_error};

/// A self-relative security descriptor, freed on drop.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is heap memory owned by this value and never changed
// after creation.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; every use only reads it.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// Parses an SDDL string such as `O:S-1-5-...D:P(A;;GA;;;S-1-5-...)`.
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(OsStr::new(sddl))?;
        let mut descriptor = ptr::null_mut();
        // SAFETY: `text` is NUL-terminated; on success `descriptor` receives a
        // LocalAlloc'ed descriptor that `Drop` frees.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
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

    /// The descriptor's access list; valid while `self` lives. A missing or
    /// null list is an error, since applying it would grant everyone everything.
    fn dacl(&self) -> io::Result<*mut ACL> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = ptr::null_mut();
        // SAFETY: `self.0` is a valid descriptor; `dacl` receives a pointer into it.
        let ok = unsafe {
            GetSecurityDescriptorDacl(self.0, &raw mut present, &raw mut dacl, &raw mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
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
        // SAFETY: allocated with LocalAlloc by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW, freed once.
        unsafe { LocalFree(self.0) };
    }
}

/// `SECURITY_ATTRIBUTES` that own their descriptor. Handles created with them
/// are never inheritable.
pub struct SecurityAttributes {
    attributes: SECURITY_ATTRIBUTES,
    descriptor: SecurityDescriptor,
}

// SAFETY: the only pointer inside refers to the owned, immutable descriptor.
unsafe impl Send for SecurityAttributes {}
// SAFETY: as above.
unsafe impl Sync for SecurityAttributes {}

impl SecurityAttributes {
    /// Wraps `descriptor` for object creation calls.
    pub fn new(descriptor: SecurityDescriptor) -> Self {
        Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                // The descriptor is heap memory, so moving `Self` keeps this valid.
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            },
            descriptor,
        }
    }

    /// A pointer for calls that take `SECURITY_ATTRIBUTES*`; valid while `self`
    /// lives. The creation calls only read it, despite some signatures taking
    /// a mutable pointer.
    pub(crate) fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &raw const self.attributes
    }
}

/// Why a directory or file is not private to the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A file or device is where a directory was expected.
    NotADirectory,
    /// A directory or device is where a file was expected.
    NotAFile,
    /// A symlink, junction or other reparse point, which could point anywhere.
    ReparsePoint,
    /// Owned by this other account.
    Owner(Sid),
    /// These other accounts are granted access, such as `Authenticated Users`
    /// on directories created under a drive root. A null access list, which
    /// grants everyone everything, is reported as `Everyone`.
    Acl(Vec<Sid>),
    /// The file has this many names; another one may sit in a directory other
    /// accounts can reach.
    HardLinks(u32),
    /// The owner or access list cannot be read or judged: this process may not
    /// read them, the volume has no security (FAT), or the access list holds
    /// entry types these checks do not understand.
    Unreadable,
}

/// What [`ensure_private_dir`] found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It was missing and was created private.
    Created,
    /// It existed and was private already.
    Present,
    /// It existed empty and owned by this user, others had access, and it now
    /// has the private access list.
    Tightened,
}

/// Checks that `path` is a real directory private to the current user. A
/// missing directory is an error of kind `NotFound`.
pub fn check_private_dir(path: &Path) -> io::Result<Result<(), Problem>> {
    let user = token::current_user()?;
    let directory = match open_dir(path, false)? {
        Ok(directory) => directory,
        Err(problem) => return Ok(Err(problem)),
    };
    Ok(judge(
        &read_security(&directory.file, SE_FILE_OBJECT)?,
        &user,
    ))
}

/// Creates `path` private when it is missing (its parent must exist), then
/// checks it. An existing directory owned by this user that only fails on its
/// access list is tightened when it is empty, the counterpart of `chmod 700`.
///
/// A non-empty directory is never tightened: a changed access list does not
/// reach the entries inside, which keep their old grants and stay reachable
/// by full path. Fixing those takes a recursive change by the user.
///
/// New directories get an explicit owner (this user, also when the process
/// is elevated and would otherwise give them to Administrators) and a
/// protected access list for this user, SYSTEM and Administrators that
/// everything created inside inherits.
pub fn ensure_private_dir(path: &Path) -> io::Result<Result<Outcome, Problem>> {
    let user = token::current_user()?;
    let attributes = SecurityAttributes::new(private_dir_descriptor(&user)?);
    let wide_path = wide(path.as_os_str())?;
    // SAFETY: `wide_path` is NUL-terminated and `attributes` outlives the call,
    // which only reads it.
    let created =
        unsafe { CreateDirectoryW(wide_path.as_ptr(), attributes.as_ptr().cast_mut()) } != 0;
    if !created {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS.cast_signed()) {
            return Err(error);
        }
    }
    let directory = match open_dir(path, true)? {
        Ok(directory) => directory,
        Err(problem) => return Ok(Err(problem)),
    };
    let problem = match judge(&read_security(&directory.file, SE_FILE_OBJECT)?, &user) {
        Ok(()) if created => return Ok(Ok(Outcome::Created)),
        Ok(()) => return Ok(Ok(Outcome::Present)),
        Err(problem) => problem,
    };
    if !matches!(problem, Problem::Acl(_)) || !directory.writable || !is_empty(&directory.file)? {
        return Ok(Err(problem));
    }
    let dacl = attributes.descriptor.dacl()?;
    // SAFETY: the handle is open with WRITE_DAC; `dacl` points into the
    // descriptor `attributes` owns, alive for the call.
    let status = unsafe {
        SetSecurityInfo(
            raw(&directory.file),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != 0 {
        return Err(win32_error(status));
    }
    // Others could add entries until the new list applied; those keep their
    // own grants, so the directory only counts as private if it is still empty.
    match judge(&read_security(&directory.file, SE_FILE_OBJECT)?, &user) {
        Ok(()) if is_empty(&directory.file)? => Ok(Ok(Outcome::Tightened)),
        Ok(()) => Ok(Err(problem)),
        Err(again) => Ok(Err(again)),
    }
}

/// Checks that `path` is a regular file with a single name, private to the
/// current user: what a file created in a private directory inherits. The
/// file itself is opened, never a link's target. A missing file is an error
/// of kind `NotFound`.
///
/// The file's directory needs [`check_private_dir`] too: a private file in a
/// directory others can write to can be replaced.
pub fn check_private_file(path: &Path) -> io::Result<Result<(), Problem>> {
    let user = token::current_user()?;
    let file = match open_entry(path, READ_CONTROL | FILE_READ_ATTRIBUTES) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(Err(Problem::Unreadable));
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(Err(Problem::ReparsePoint));
    }
    if !metadata.is_file() {
        return Ok(Err(Problem::NotAFile));
    }
    let links = link_count(&file)?;
    if links > 1 {
        return Ok(Err(Problem::HardLinks(links)));
    }
    Ok(judge(&read_security(&file, SE_FILE_OBJECT)?, &user))
}

/// The owner of the kernel object behind `handle` (a pipe, for instance),
/// which must be open with `READ_CONTROL`; `None` when it has none.
pub(crate) fn kernel_object_owner(
    handle: &impl std::os::windows::io::AsRawHandle,
) -> io::Result<Option<Sid>> {
    Ok(read_security(handle, SE_KERNEL_OBJECT)?.owner)
}

/// The protected access list of a private directory, inherited by everything
/// created inside, with `user` as the explicit owner.
fn private_dir_descriptor(user: &Sid) -> io::Result<SecurityDescriptor> {
    SecurityDescriptor::from_sddl(&format!(
        "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))
}

/// A directory opened on the path itself.
struct OpenDir {
    file: File,
    /// Opened with `WRITE_DAC` and `FILE_LIST_DIRECTORY`, so it can be tightened.
    writable: bool,
}

/// Opens the directory `path` itself for reading its security and, with
/// `tighten`, for listing it and changing its access list when this process
/// may. A link, a non-directory or a refused open is a problem, not an error.
fn open_dir(path: &Path, tighten: bool) -> io::Result<Result<OpenDir, Problem>> {
    let read = READ_CONTROL | FILE_READ_ATTRIBUTES;
    let mut opened = None;
    if tighten {
        match open_entry(path, read | FILE_LIST_DIRECTORY | WRITE_DAC) {
            Ok(file) => opened = Some((file, true)),
            // Not this user's directory, or one that denies them; the read-only
            // open below still lets the check say which.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    let (file, writable) = match opened {
        Some(opened) => opened,
        None => match open_entry(path, read) {
            Ok(file) => (file, false),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Ok(Err(Problem::Unreadable));
            }
            Err(error) => return Err(error),
        },
    };
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(Err(Problem::ReparsePoint));
    }
    if !metadata.is_dir() {
        return Ok(Err(Problem::NotADirectory));
    }
    Ok(Ok(OpenDir { file, writable }))
}

/// Opens `path` itself with `access`, never following a link; backup
/// semantics let directories open too.
fn open_entry(path: &Path, access: u32) -> io::Result<File> {
    File::options()
        .access_mode(access)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Whether the directory open as `directory` (with `FILE_LIST_DIRECTORY`) has
/// no entries besides `.` and `..`. Lists through the handle, so it is the
/// same directory the security calls see.
fn is_empty(directory: &File) -> io::Result<bool> {
    const BYTES: usize = 4096;
    let mut buffer = vec![0_u64; BYTES / 8];
    let mut class = FileFullDirectoryRestartInfo;
    let header = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
    loop {
        // SAFETY: `buffer` holds BYTES writable, 8-aligned bytes and the
        // handle stays open for the call.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                raw(directory),
                class,
                buffer.as_mut_ptr().cast(),
                BYTES as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
                return Ok(true);
            }
            return Err(error);
        }
        class = FileFullDirectoryInfo;
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0_usize;
        loop {
            if offset + header > BYTES {
                return Err(io::Error::other("directory listing overran its buffer"));
            }
            // SAFETY: `offset + header` is within the buffer, and entries the
            // call wrote are 8-aligned (NextEntryOffset keeps that alignment).
            let entry = unsafe { &*base.add(offset).cast::<FILE_FULL_DIR_INFO>() };
            let units = entry.FileNameLength as usize / 2;
            if offset + header + units * 2 > BYTES {
                return Err(io::Error::other("directory listing overran its buffer"));
            }
            // SAFETY: the name's `units` UTF-16 units follow the header inside
            // the buffer, as checked above.
            let name = unsafe {
                std::slice::from_raw_parts(base.add(offset + header).cast::<u16>(), units)
            };
            if name != [u16::from(b'.')] && name != [u16::from(b'.'), u16::from(b'.')] {
                return Ok(false);
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
}

/// How many names (hard links) the open file has.
fn link_count(file: &File) -> io::Result<u32> {
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the handle stays open for the call; `info` is writable memory of
    // the structure's size.
    if unsafe { GetFileInformationByHandle(raw(file), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled the structure.
    Ok(unsafe { info.assume_init() }.nNumberOfLinks)
}

/// The owner and access list of an object.
struct Security {
    /// `None` on volumes without security (FAT).
    owner: Option<Sid>,
    /// `None` for a null access list, which grants everyone everything.
    dacl: Option<Dacl>,
}

/// The parts of an access list the checks judge.
struct Dacl {
    /// Accounts of the allowed and allowed-callback entries.
    allowed: Vec<Sid>,
    /// An entry of another layout (object or compound grants) is present,
    /// which these checks cannot read and so must not pass.
    unjudged: bool,
}

/// Whether `owner` may own a directory or file that only `user` should
/// control: the user, or an account that already controls every object on
/// the machine (Administrators, SYSTEM). Not for the daemon's pipe, whose
/// server always sets the user as owner, so clients require exactly that.
pub fn trusted_owner(owner: &Sid, user: &Sid) -> bool {
    owner == user || *owner == Sid::administrators() || *owner == Sid::local_system()
}

/// The verdict on an object's security for `user`.
fn judge(security: &Security, user: &Sid) -> Result<(), Problem> {
    let Some(owner) = &security.owner else {
        return Err(Problem::Unreadable);
    };
    // Administrators and SYSTEM can rewrite any access list anyway, and an
    // elevated process creates objects owned by Administrators by default, so
    // their ownership grants nobody untrusted a way in.
    if !trusted_owner(owner, user) {
        return Err(Problem::Owner(owner.clone()));
    }
    let Some(dacl) = &security.dacl else {
        return Err(Problem::Acl(vec![Sid::everyone()]));
    };
    if dacl.unjudged {
        return Err(Problem::Unreadable);
    }
    // CREATOR OWNER only stands for the owner of each new entry inside, who
    // already has to be one of the accounts allowed to create entries here.
    let trusted = [
        user.clone(),
        Sid::local_system(),
        Sid::administrators(),
        Sid::creator_owner(),
    ];
    let mut others: Vec<Sid> = Vec::new();
    for sid in &dacl.allowed {
        if !trusted.contains(sid) && !others.contains(sid) {
            others.push(sid.clone());
        }
    }
    if others.is_empty() {
        Ok(())
    } else {
        Err(Problem::Acl(others))
    }
}

/// Reads the owner and access list of the object behind `handle`, which must
/// be open with `READ_CONTROL` and be an object of `kind`.
fn read_security(
    handle: &impl std::os::windows::io::AsRawHandle,
    kind: SE_OBJECT_TYPE,
) -> io::Result<Security> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the handle is borrowed for the call; on success `owner` and
    // `dacl` point into `descriptor`, freed below after their last use.
    let status = unsafe {
        GetSecurityInfo(
            raw(handle),
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
        return Err(win32_error(status));
    }
    // SAFETY: `owner` and `dacl` are null or point into `descriptor`, alive
    // until freed below.
    let security = unsafe { parse_security(owner, dacl) };
    // SAFETY: allocated with LocalAlloc by GetSecurityInfo, freed once.
    unsafe { LocalFree(descriptor) };
    security
}

/// Copies the owner and the readable entries of `dacl`.
///
/// # Safety
///
/// `owner` and `dacl` must each be null or valid for the call.
unsafe fn parse_security(owner: PSID, dacl: *const ACL) -> io::Result<Security> {
    let owner = if owner.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees a non-null `owner` is valid.
        Some(unsafe { Sid::copy_from(owner) }?)
    };
    if dacl.is_null() {
        return Ok(Security { owner, dacl: None });
    }
    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` is valid and `size` is the structure AclSizeInformation
    // fills, with its size passed.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast::<c_void>(),
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
        // SAFETY: `index` is below the entry count; `ace` receives a pointer
        // into `dacl`.
        if unsafe { GetAce(dacl, index, &raw mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every entry starts with an ACE_HEADER; entries are only
        // 4-aligned, hence the unaligned read.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        match header.AceType {
            // ACCESS_ALLOWED and ACCESS_ALLOWED_CALLBACK share a layout with
            // the SID at `SidStart`.
            0 | 9 => {
                let entry = ace.cast::<ACCESS_ALLOWED_ACE>();
                // SAFETY: as stated for these two types; the SID lies inside
                // the entry, which lies inside `dacl`.
                let sid = unsafe { Sid::copy_from((&raw mut (*entry).SidStart).cast()) }?;
                allowed.push(sid);
            }
            // Deny entries only take access away.
            1 | 6 | 10 | 12 => {}
            // Object and compound grants have other layouts; a private
            // directory never needs them.
            _ => unjudged = true,
        }
    }
    Ok(Security {
        owner,
        dacl: Some(Dacl { allowed, unjudged }),
    })
}
