//! The process table: snapshots, single lookups, identities, listening sockets and
//! verified termination.
//!
//! A process's start time is its creation `FILETIME` (100 ns ticks since 1601 UTC),
//! recorded once at creation, so clock changes never alter it. Windows reuses PIDs
//! quickly, which is why every action compares it first.

use std::ffi::c_void;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::windows::io::OwnedHandle;
use std::ptr;

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemTimeOfDayInformation,
};
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, FILETIME, NO_ERROR, WAIT_OBJECT_0,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_BASIC_INFORMATION,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

use super::token::{owned, raw};
use crate::{Error, ListeningSocket, ProcessIdentity, ProcessInfo, SignalOutcome};

/// Every process this user can query. Protected and other users' processes that
/// refuse `PROCESS_QUERY_LIMITED_INFORMATION` are skipped: no start time, no
/// identity. Units always run as this user, so none of theirs is lost.
pub(crate) fn processes() -> Result<Vec<ProcessInfo>, Error> {
    // SAFETY: plain call; the result is checked before use.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot as isize == -1 {
        return Err(io::Error::last_os_error().into());
    }
    let snapshot = owned(snapshot)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        // SAFETY: an all-zero value is valid for the rest of this plain C struct.
        ..unsafe { std::mem::zeroed() }
    };
    let mut found = Vec::new();
    // SAFETY: `snapshot` is open and `entry.dwSize` is set as the API requires.
    let mut more = unsafe { Process32FirstW(raw(&snapshot), &raw mut entry) } != 0;
    while more {
        if let Some(handle) = open(entry.th32ProcessID, PROCESS_QUERY_LIMITED_INFORMATION)
            && let Ok(start_time) = creation_time(&handle)
        {
            found.push(ProcessInfo {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                pgid: 0,
                sid: None,
                uid: 0,
                start_time,
                zombie: exited(&handle),
                name: wide_name(&entry.szExeFile),
            });
        }
        // SAFETY: as above, continuing the same enumeration.
        more = unsafe { Process32NextW(raw(&snapshot), &raw mut entry) } != 0;
    }
    Ok(found)
}

/// One process, or `None` once it is gone or cannot be queried. A process that
/// exited while someone still holds a handle to it (the runner does until the
/// final report) is reported with `zombie: true`, like an unreaped Unix child.
pub(crate) fn process(pid: u32) -> Result<Option<ProcessInfo>, Error> {
    let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return Ok(None);
    };
    let start_time = creation_time(&handle)?;
    Ok(Some(ProcessInfo {
        pid,
        ppid: parent_pid(&handle).unwrap_or(0),
        pgid: 0,
        sid: None,
        uid: 0,
        start_time,
        zombie: exited(&handle),
        name: image_name(&handle).unwrap_or_default(),
    }))
}

/// The kernel's boot time, in 100 ns ticks since 1601. It stays the same for the
/// whole boot and changes with the next one. A Fast Startup resume keeps the
/// hibernated kernel's value, which is harmless: every user process is gone then
/// anyway, and the recorded start times no longer match.
pub(crate) fn boot_id() -> Result<String, Error> {
    // SYSTEM_TIMEOFDAY_INFORMATION is 48 bytes; BootTime is its first 64-bit field.
    let mut info = [0_i64; 6];
    // SAFETY: `info` is 48 writable, 8-aligned bytes, the size of the structure.
    let status = unsafe {
        NtQuerySystemInformation(
            SystemTimeOfDayInformation,
            info.as_mut_ptr().cast(),
            size_of_val(&info) as u32,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(Error::Tool {
            tool: "NtQuerySystemInformation",
            detail: format!("status {status:#x}"),
        });
    }
    Ok(format!("boot-{}", info[0]))
}

/// TCP listeners (IPv4 and IPv6) owned by `pids`.
pub(crate) fn listening_tcp(pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    let mut sockets = Vec::new();
    let v4 = tcp_table(u32::from(AF_INET))?;
    for row in rows::<MIB_TCPROW_OWNER_PID>(&v4) {
        if pids.contains(&row.dwOwningPid) {
            sockets.push(ListeningSocket {
                pid: row.dwOwningPid,
                // The table stores the address and port in network byte order.
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(row.dwLocalAddr))),
                    port(row.dwLocalPort),
                ),
            });
        }
    }
    let v6 = tcp_table(u32::from(AF_INET6))?;
    for row in rows::<MIB_TCP6ROW_OWNER_PID>(&v6) {
        if pids.contains(&row.dwOwningPid) {
            sockets.push(ListeningSocket {
                pid: row.dwOwningPid,
                addr: SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                    port(row.dwLocalPort),
                ),
            });
        }
    }
    Ok(sockets)
}

/// Ends the process with `exit_code` only if its PID still belongs to the recorded
/// process (same creation time). The check and the termination use one handle, so
/// unlike Unix there is no window in which the PID could be reused.
pub fn terminate_verified(
    identity: &ProcessIdentity,
    exit_code: u32,
) -> Result<SignalOutcome, Error> {
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let Some(handle) = open(identity.pid, access) else {
        return Ok(SignalOutcome::Gone);
    };
    if creation_time(&handle)? != identity.start_time {
        return Ok(SignalOutcome::Mismatch);
    }
    if exited(&handle) {
        return Ok(SignalOutcome::Gone);
    }
    // SAFETY: `handle` is open with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(raw(&handle), exit_code) } == 0 {
        let error = io::Error::last_os_error();
        // Access denied right as it exits: it is gone either way.
        if exited(&handle) {
            return Ok(SignalOutcome::Gone);
        }
        return Err(error.into());
    }
    Ok(SignalOutcome::Sent)
}

/// Opens `pid` with `access`, or `None` when it is gone or refuses.
pub(crate) fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<OwnedHandle> {
    if pid == 0 {
        return None;
    }
    // SAFETY: plain call; a null result is mapped to `None`.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    match owned(handle) {
        Ok(handle) => Some(handle),
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => None,
        // Access denied: a protected or other user's process, no identity to give.
        Err(_) => None,
    }
}

/// The creation time of an open process as one 64-bit value.
pub(crate) fn creation_time(handle: &OwnedHandle) -> io::Result<u64> {
    let mut times = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: `handle` is open with query rights; the four FILETIMEs are writable.
    if unsafe { GetProcessTimes(raw(handle), created, exited, kernel, user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Whether an open process has exited. Waiting is used rather than the exit code,
/// because a running process can legitimately report 259 (`STILL_ACTIVE`).
pub(crate) fn exited(handle: &OwnedHandle) -> bool {
    // SAFETY: `handle` is open; a zero timeout only polls.
    unsafe { WaitForSingleObject(raw(handle), 0) == WAIT_OBJECT_0 }
}

/// The parent PID recorded at creation; the parent may be gone or its PID reused.
fn parent_pid(handle: &OwnedHandle) -> Option<u32> {
    // SAFETY: an all-zero value is valid for this plain C struct.
    let mut info: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is open with query rights; `info` matches the class and size.
    let status = unsafe {
        NtQueryInformationProcess(
            raw(handle),
            ProcessBasicInformation,
            (&raw mut info).cast::<c_void>(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    (status == 0).then(|| u32::try_from(info.InheritedFromUniqueProcessId).ok())?
}

/// The executable's file name, like the snapshot's `szExeFile`.
fn image_name(handle: &OwnedHandle) -> Option<String> {
    let mut buffer = vec![0_u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: `buffer` holds `len` writable units; the call stores the length used.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            raw(handle),
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &raw mut len,
        )
    };
    if ok == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    Some(path.rsplit('\\').next().unwrap_or(&path).to_owned())
}

/// A NUL-terminated UTF-16 name from a fixed-size field.
fn wide_name(field: &[u16]) -> String {
    let len = field
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..len])
}

/// The TCP port in the low 16 bits of a table entry, in network byte order.
fn port(raw_port: u32) -> u16 {
    u16::from_be(raw_port as u16)
}

/// The listener table of one address family, as raw bytes holding a
/// `MIB_TCP*TABLE_OWNER_PID`: a row count followed by rows.
fn tcp_table(family: u32) -> Result<Vec<u64>, Error> {
    let mut size = 0_u32;
    for _ in 0..4 {
        // u64 elements keep the rows 8-aligned.
        let mut buffer = vec![0_u64; (size as usize).div_ceil(8).max(1)];
        let mut bytes = (buffer.len() * 8) as u32;
        // SAFETY: `buffer` holds `bytes` writable, aligned bytes.
        let status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &raw mut bytes,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match status {
            NO_ERROR => return Ok(buffer),
            // The table grew between the size query and the read; ask again.
            ERROR_INSUFFICIENT_BUFFER => size = bytes,
            other => return Err(io::Error::from_raw_os_error(other as i32).into()),
        }
    }
    Err(Error::Malformed {
        source_name: "TCP listener table",
        detail: "kept growing while being read".into(),
    })
}

/// The rows of a table returned by [`tcp_table`].
fn rows<Row: Copy>(table: &[u64]) -> Vec<Row> {
    let count = (table[0] & 0xffff_ffff) as usize;
    // Rows start after the 32-bit count, padded to the row's alignment.
    let offset = align_of::<Row>().max(4);
    let available = (table.len() * 8).saturating_sub(offset) / size_of::<Row>();
    let base = table.as_ptr().cast::<u8>();
    (0..count.min(available))
        .map(|index| {
            // SAFETY: the index is within the bytes the call filled, and
            // `read_unaligned` makes no alignment assumption.
            unsafe {
                base.add(offset + index * size_of::<Row>())
                    .cast::<Row>()
                    .read_unaligned()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::process::{Command, Stdio};

    use super::*;

    /// This process is in the snapshot with the same identity a lookup gives.
    #[test]
    fn snapshot_and_lookup_agree() -> Result<(), Error> {
        let me = std::process::id();
        let listed = processes()?
            .into_iter()
            .find(|process| process.pid == me)
            .ok_or(Error::Unsupported("own process missing"))?;
        let looked_up = process(me)?.ok_or(Error::Unsupported("own lookup missing"))?;
        assert_eq!(listed.start_time, looked_up.start_time);
        assert_eq!(listed.ppid, looked_up.ppid);
        assert!(!looked_up.zombie);
        assert!(looked_up.name.to_ascii_lowercase().ends_with(".exe"));
        assert!(!boot_id()?.is_empty());
        assert_eq!(boot_id()?, boot_id()?);
        Ok(())
    }

    /// Listeners on both families are reported with their port.
    #[test]
    fn listeners_of_this_process() -> Result<(), Error> {
        let v4 = TcpListener::bind("127.0.0.1:0")?;
        let v6 = TcpListener::bind("[::1]:0")?;
        let found = listening_tcp(&[std::process::id()])?;
        for listener in [&v4, &v6] {
            let addr = listener.local_addr()?;
            assert!(
                found.iter().any(|socket| socket.addr == addr),
                "{addr} in {found:?}"
            );
        }
        Ok(())
    }

    /// A child is terminated only through its recorded identity; a stale identity
    /// is a mismatch and an exited child is gone.
    #[test]
    fn verified_termination() -> Result<(), Error> {
        let mut child = Command::new("cmd")
            .args(["/d", "/c", "ping -n 30 127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()?;
        let info = process(child.id())?.ok_or(Error::Unsupported("child missing"))?;
        let identity = ProcessIdentity {
            pid: info.pid,
            sid: info.pid,
            start_time: info.start_time,
        };
        let stale = ProcessIdentity {
            start_time: info.start_time - 1,
            ..identity
        };
        assert_eq!(terminate_verified(&stale, 1)?, SignalOutcome::Mismatch);
        assert_eq!(terminate_verified(&identity, 1)?, SignalOutcome::Sent);
        let status = child.wait()?;
        assert_eq!(status.code(), Some(1));
        assert_eq!(terminate_verified(&identity, 1)?, SignalOutcome::Gone);
        Ok(())
    }
}
