//! Service settings the `windows-service` crate does not cover: the privileges
//! the service manager leaves in a service's token.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceConfig2W,
    SC_HANDLE, SC_MANAGER_CONNECT, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
    SERVICE_QUERY_CONFIG, SERVICE_REQUIRED_PRIVILEGES_INFOW,
};

use super::security::wide;

/// A service manager or service handle, closed on drop.
struct Handle(SC_HANDLE);

impl Handle {
    /// Takes a handle returned by the service manager, or the last error when null.
    fn new(handle: SC_HANDLE) -> io::Result<Self> {
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW or OpenServiceW and is
        // closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Opens the service `name` with `access`.
fn open(name: &str, access: u32) -> io::Result<(Handle, Handle)> {
    // SAFETY: null machine and database names mean the local active database.
    let manager =
        Handle::new(unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) })?;
    let wide_name = wide(OsStr::new(name));
    // SAFETY: `manager` is open and `wide_name` is NUL-terminated.
    let service = Handle::new(unsafe { OpenServiceW(manager.0, wide_name.as_ptr(), access) })?;
    Ok((manager, service))
}

/// `privileges` as a `REG_MULTI_SZ` style list: each NUL-terminated, then one
/// more NUL.
fn multi_string(privileges: &[&str]) -> Vec<u16> {
    let mut list: Vec<u16> = privileges
        .iter()
        .flat_map(|privilege| wide(OsStr::new(privilege)))
        .collect();
    list.push(0);
    list
}

/// Splits a `REG_MULTI_SZ` style list at its NULs, stopping at the empty entry.
fn split_multi_string(list: &[u16]) -> Vec<String> {
    list.split(|&unit| unit == 0)
        .take_while(|entry| !entry.is_empty())
        .map(|entry| {
            std::ffi::OsString::from_wide(entry)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Sets the privileges the service `name` keeps in its token
/// (`SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO`); the service manager removes
/// every other one when it starts the process. Needs an elevated process.
pub fn set_required_privileges(name: &str, privileges: &[&str]) -> io::Result<()> {
    let (_manager, service) = open(name, SERVICE_CHANGE_CONFIG)?;
    let mut list = multi_string(privileges);
    let info = SERVICE_REQUIRED_PRIVILEGES_INFOW {
        pmszRequiredPrivileges: list.as_mut_ptr(),
    };
    // SAFETY: `service` is open with SERVICE_CHANGE_CONFIG; `info` points to a
    // double-NUL-terminated list that outlives the call.
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
            (&raw const info).cast(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The privileges the service `name` keeps in its token; empty when none are
/// set (it then keeps all of its account's). Needs only query access.
pub fn required_privileges(name: &str) -> io::Result<Vec<String>> {
    let (_manager, service) = open(name, SERVICE_QUERY_CONFIG)?;
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer; it fails with the needed size.
    unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
            ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 elements keep the buffer aligned for the structure's pointer.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds `needed` writable, aligned bytes.
    if unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call filled a SERVICE_REQUIRED_PRIVILEGES_INFOW at the start
    // of the buffer.
    let list = unsafe {
        (*buffer.as_ptr().cast::<SERVICE_REQUIRED_PRIVILEGES_INFOW>()).pmszRequiredPrivileges
    };
    if list.is_null() {
        return Ok(Vec::new());
    }
    // The list lies inside `buffer`, after the structure; read up to its end.
    let start = list as usize;
    let end = buffer.as_ptr() as usize + buffer.len() * 8;
    if start < buffer.as_ptr() as usize || start >= end {
        return Err(io::Error::other("privilege list outside the buffer"));
    }
    let units = (end - start) / 2;
    // SAFETY: `list` points into `buffer`, which holds `units` more u16 values.
    let slice = unsafe { std::slice::from_raw_parts(list, units) };
    Ok(split_multi_string(slice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_lists_round_trip() {
        let list = multi_string(&["SeChangeNotifyPrivilege", "SeImpersonatePrivilege"]);
        assert_eq!(list.iter().filter(|&&unit| unit == 0).count(), 3);
        assert_eq!(list.last(), Some(&0));
        assert_eq!(
            split_multi_string(&list),
            ["SeChangeNotifyPrivilege", "SeImpersonatePrivilege"]
        );
        assert!(split_multi_string(&multi_string(&[])).is_empty());
    }

    /// Read-only: the DNS Client service lists the privileges it keeps.
    #[test]
    fn a_system_service_lists_its_privileges() -> io::Result<()> {
        let privileges = required_privileges("Dnscache")?;
        assert!(!privileges.is_empty(), "{privileges:?}");
        assert!(
            privileges
                .iter()
                .all(|privilege| privilege.starts_with("Se")),
            "{privileges:?}"
        );
        let missing = required_privileges("devlanes-no-such-service").expect_err("missing");
        assert_eq!(missing.raw_os_error(), Some(1060));
        Ok(())
    }
}
