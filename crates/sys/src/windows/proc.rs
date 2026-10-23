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
use windows_sys::Wdk::System::Threading::{
    NtQueryInformationProcess, PROCESSINFOCLASS, ProcessBasicInformation,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, NO_ERROR,
    WAIT_OBJECT_0,
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
/// identity. Units always run as this user, so none of theirs is lost. Exited
/// processes are not listed even while a handle keeps them, so `zombie` is only
/// set for one that exits during the walk.
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
        if let Some(handle) = open_query(entry.th32ProcessID)
            && let Ok(start_time) = creation_time(&handle)
        {
            found.push(ProcessInfo {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                pgid: None,
                sid: None,
                uid: None,
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
    let Some(handle) = open_query(pid) else {
        return Ok(None);
    };
    let start_time = creation_time(&handle)?;
    Ok(Some(ProcessInfo {
        pid,
        ppid: parent_pid(&handle).unwrap_or(0),
        pgid: None,
        sid: None,
        uid: None,
        start_time,
        zombie: exited(&handle),
        name: image_name(&handle).unwrap_or_default(),
    }))
}

/// The kernel's boot time, in 100 ns ticks since 1601. It stays the same for the
/// whole boot and changes with the next one. A Fast Startup resume keeps the
/// hibernated kernel's value, which is harmless: every user process is gone then
/// anyway, and the recorded start times no longer match.
///
/// Setting the clock moves `BootTime` and `BootTimeBias` by the same amount, so
/// their difference is the value that stays fixed for the boot.
pub(crate) fn boot_id() -> Result<String, Error> {
    // SYSTEM_TIMEOFDAY_INFORMATION is 48 bytes: BootTime, CurrentTime,
    // TimeZoneBias, TimeZoneId plus padding, BootTimeBias, SleepTimeBias.
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
    Ok(format!("boot-{}", info[0].wrapping_sub(info[4])))
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
    // Only a PID that names no process is gone; a live one that refuses is an error,
    // so the caller keeps its record instead of forgetting a running process,
    // unless the PID now belongs to a process started at another time.
    let handle = match open(identity.pid, access) {
        Ok(Some(handle)) => handle,
        Ok(None) => return Ok(SignalOutcome::Gone),
        Err(error) => {
            return match open_query(identity.pid).map(|handle| creation_time(&handle)) {
                Some(Ok(start_time)) if start_time != identity.start_time => {
                    Ok(SignalOutcome::Mismatch)
                }
                _ => Err(error.into()),
            };
        }
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

/// A running process, verified by identity, whose PID cannot be reused while
/// this is held: an open handle keeps the process object, and so its PID, alive.
/// For acting on a process by bare PID afterwards (a console helper attaching
/// to it) without reaching a stranger that got the PID in between.
#[derive(Debug)]
pub struct Held {
    _handle: OwnedHandle,
}

impl Held {
    /// Opens `identity`; `None` when it is gone, exited, or its PID now names
    /// another process.
    pub fn open(identity: &ProcessIdentity) -> Result<Option<Self>, Error> {
        let handle = open_verified(identity, PROCESS_SYNCHRONIZE)?;
        Ok(handle.map(|handle| Self { _handle: handle }))
    }
}

/// The console a process is attached to, known by its console host
/// (`conhost.exe`, or `OpenConsole.exe` in a Windows Terminal tab). The host is held
/// open, so its PID cannot name another process while this lives, even after
/// the host exited with its last client.
///
/// Built on an undocumented query (see [`console_host`]); the tests below pin
/// how it behaves.
#[derive(Debug)]
pub struct Console {
    host: u32,
    _handle: OwnedHandle,
}

/// A process's console, as [`console_host`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleHost {
    /// Attached to the console the host with this PID serves.
    Host(u32),
    /// Attached to none (`DETACHED_PROCESS`, `FreeConsole`).
    Detached,
    /// No console set up yet: the process is starting, or never gets one (a
    /// GUI program, which stays so for its whole life; [`age`] tells the two
    /// apart). A console host reports this about itself too.
    Pending,
}

/// Where a process stands with a [`Console`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConsole {
    /// Attached to it now.
    Yes,
    /// On another console or none, or gone.
    No,
    /// Its console is not set up yet (see [`ConsoleHost::Pending`]).
    NotYet,
}

/// Why the console an exited process was on is not known.
#[derive(Debug, thiserror::Error)]
pub enum LostConsole {
    #[error("it never had a console (a GUI program, or detached)")]
    NeverHad,
    #[error("its console host {0} has exited")]
    HostGone(u32),
    #[error("process {0} is not a console host it started")]
    NotItsHost(u32),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Console {
    /// The console `process` is attached to now; `None` while it has none, as
    /// before its console is set up (it is created while the process starts).
    pub(crate) fn of(process: &OwnedHandle) -> io::Result<Option<Self>> {
        let ConsoleHost::Host(host) = console_host(process)? else {
            return Ok(None);
        };
        let Some(handle) = open(
            host,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        )?
        else {
            return Ok(None);
        };
        // Opened by bare PID: it is the host only if `process` is still running
        // on it, which keeps the host alive and its PID from being reused.
        if exited(process) || console_host(process)? != ConsoleHost::Host(host) {
            return Ok(None);
        }
        Ok(Some(Self {
            host,
            _handle: handle,
        }))
    }

    /// The console `process` (PID `pid`, started at `start_time`) was last
    /// attached to, also after it exited: the query keeps its last value. The
    /// host it names is accepted only while it runs, was started by `process`
    /// (its parent PID) and not before it. `process` is held open, so no other
    /// process has its PID, and a host it started is the one it set up.
    pub(crate) fn last_of(
        process: &OwnedHandle,
        pid: u32,
        start_time: u64,
    ) -> Result<Self, LostConsole> {
        let ConsoleHost::Host(host) = console_host(process)? else {
            return Err(LostConsole::NeverHad);
        };
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        let Some(handle) = open(host, access)?.filter(|handle| !exited(handle)) else {
            return Err(LostConsole::HostGone(host));
        };
        if parent_pid(&handle) != Some(pid) || creation_time(&handle)? < start_time {
            return Err(LostConsole::NotItsHost(host));
        }
        Ok(Self {
            host,
            _handle: handle,
        })
    }

    /// Whether `identity` is attached to this console now; [`OnConsole::No`]
    /// once it is gone, exited or its PID names another process.
    pub fn attached(&self, identity: &ProcessIdentity) -> io::Result<OnConsole> {
        Ok(match console_host_of(identity)? {
            Some(ConsoleHost::Host(host)) if host == self.host => OnConsole::Yes,
            Some(ConsoleHost::Pending) => OnConsole::NotYet,
            _ => OnConsole::No,
        })
    }
}

/// The console `identity` is attached to now; `None` when it is gone, exited
/// or its PID names another process.
pub fn console_host_of(identity: &ProcessIdentity) -> io::Result<Option<ConsoleHost>> {
    // `SYNCHRONIZE`, so an exited process is told from a running one.
    let Some(handle) = open_verified(identity, PROCESS_SYNCHRONIZE)? else {
        return Ok(None);
    };
    console_host(&handle).map(Some)
}

/// The console host of the console an open process is attached to.
///
/// Undocumented: `NtQueryInformationProcess` class 49, `ProcessConsoleHostProcess`
/// in System Informer's phnt headers (windows-sys names it
/// `ProcessOwnerInformation`), needs only `PROCESS_QUERY_LIMITED_INFORMATION`
/// and gives a `ULONG_PTR` with two flag bits. Bit 0 set: the rest is the host's
/// PID, 0 without a console; the value follows `AttachConsole` and
/// `FreeConsole`. Bit 1 set: the rest is another process (the creator of a
/// process whose console is not set up yet, which stays so for a GUI program,
/// or the client a console host serves). It keeps its last value after the
/// process exited.
fn console_host(process: &OwnedHandle) -> io::Result<ConsoleHost> {
    const PROCESS_CONSOLE_HOST_PROCESS: PROCESSINFOCLASS = 49;
    let mut value = 0_usize;
    // SAFETY: `process` is open with query rights; `value` is the writable
    // ULONG_PTR the class fills.
    let status = unsafe {
        NtQueryInformationProcess(
            raw(process),
            PROCESS_CONSOLE_HOST_PROCESS,
            (&raw mut value).cast::<c_void>(),
            size_of::<usize>() as u32,
            ptr::null_mut(),
        )
    };
    // NT_SUCCESS: success and informational statuses are not negative.
    if status < 0 {
        return Err(io::Error::other(format!(
            "NtQueryInformationProcess(ProcessConsoleHostProcess) failed: {status:#x}"
        )));
    }
    let pid = u32::try_from(value & !3).unwrap_or(0);
    Ok(if value & 2 != 0 {
        ConsoleHost::Pending
    } else if value & 1 != 0 && pid != 0 {
        ConsoleHost::Host(pid)
    } else {
        ConsoleHost::Detached
    })
}

/// How long ago a process with this start time was created, by the system
/// clock; zero for a start time ahead of it (the clock was set back).
pub fn age(start_time: u64) -> std::time::Duration {
    use std::time::{Duration, SystemTime};
    /// 1970-01-01 in 100 ns ticks since 1601-01-01, the start time's epoch.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let now = u64::try_from(since_unix.as_nanos() / 100)
        .map_or(u64::MAX, |ticks| ticks.saturating_add(UNIX_EPOCH_TICKS));
    Duration::from_nanos(now.saturating_sub(start_time).saturating_mul(100))
}

/// Opens `identity` with `access` plus `PROCESS_QUERY_LIMITED_INFORMATION`;
/// `None` when it is gone, exited, or its PID now names a process started at
/// another time.
pub(crate) fn open_verified(
    identity: &ProcessIdentity,
    access: PROCESS_ACCESS_RIGHTS,
) -> io::Result<Option<OwnedHandle>> {
    let Some(handle) = open(identity.pid, access | PROCESS_QUERY_LIMITED_INFORMATION)? else {
        return Ok(None);
    };
    if creation_time(&handle)? != identity.start_time || exited(&handle) {
        return Ok(None);
    }
    Ok(Some(handle))
}

/// Opens `pid` with `access`; `None` when no process has that PID.
pub(crate) fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> io::Result<Option<OwnedHandle>> {
    if pid == 0 {
        return Ok(None);
    }
    // SAFETY: plain call; a null result is checked.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    match owned(handle) {
        Ok(handle) => Ok(Some(handle)),
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Opens `pid` for queries, or `None` when it is gone or refuses: a protected or
/// another user's process has no identity to give.
///
/// `SYNCHRONIZE` is asked for too, because only a waitable handle tells an exited
/// process from a running one; processes that refuse it are opened without.
fn open_query(pid: u32) -> Option<OwnedHandle> {
    match open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(handle) => handle,
        // Only a refusal of `SYNCHRONIZE` is worth a second try; protected
        // processes refuse both, and every scan meets about a hundred of them.
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok().flatten()
        }
        Err(_) => None,
    }
}

/// The creation time of an open process as one 64-bit value.
pub(crate) fn creation_time(handle: &OwnedHandle) -> io::Result<u64> {
    Ok(times(handle)?.created)
}

/// `GetProcessTimes` of an open process, in 100 ns ticks.
pub(crate) struct Times {
    /// The creation time, since 1601 UTC.
    pub(crate) created: u64,
    /// CPU time in kernel mode.
    pub(crate) kernel: u64,
    /// CPU time in user mode.
    pub(crate) user: u64,
}

/// Reads the creation and CPU times of an open process.
pub(crate) fn times(handle: &OwnedHandle) -> io::Result<Times> {
    let mut times = [0_u64; 4];
    let [created, exited, kernel, user] = times.each_mut().map(ptr::from_mut);
    // SAFETY: `handle` is open with query rights. A FILETIME is two u32 halves,
    // low half first, so on little-endian Windows each writable u64 has its
    // layout, with stricter alignment.
    let ok = unsafe {
        GetProcessTimes(
            raw(handle),
            created.cast(),
            exited.cast(),
            kernel.cast(),
            user.cast(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let [created, _, kernel, user] = times;
    Ok(Times {
        created,
        kernel,
        user,
    })
}

/// Whether an open process has exited. Waiting is preferred over the exit code,
/// because a running process can legitimately report 259 (`STILL_ACTIVE`); the
/// code is the fallback for a handle without `SYNCHRONIZE`.
pub(crate) fn exited(handle: &OwnedHandle) -> bool {
    use windows_sys::Win32::Foundation::{STILL_ACTIVE, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    // SAFETY: `handle` is open; a zero timeout only polls.
    match unsafe { WaitForSingleObject(raw(handle), 0) } {
        WAIT_OBJECT_0 => true,
        WAIT_TIMEOUT => false,
        _ => {
            let mut code = 0;
            // SAFETY: `handle` is open with query rights; `code` is writable.
            let ok = unsafe { GetExitCodeProcess(raw(handle), &raw mut code) } != 0;
            ok && code != STILL_ACTIVE as u32
        }
    }
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
    // NT_SUCCESS, as in `console_host`.
    (status >= 0).then(|| u32::try_from(info.InheritedFromUniqueProcessId).ok())?
}

/// The executable's file name, like the snapshot's `szExeFile`.
fn image_name(handle: &OwnedHandle) -> Option<String> {
    // The longest path Windows supports, so long image paths still have a name.
    let mut buffer = vec![0_u16; 32_768];
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

    /// A live identity is held; another start time or a gone PID is not.
    #[test]
    fn holds_only_the_recorded_process() -> Result<(), Error> {
        let me = crate::identity(std::process::id())?.ok_or(Error::Tool {
            tool: "identity",
            detail: "no identity for this process".into(),
        })?;
        assert!(Held::open(&me)?.is_some());
        let other = ProcessIdentity {
            start_time: me.start_time + 1,
            ..me
        };
        assert!(Held::open(&other)?.is_none());
        let gone = ProcessIdentity { pid: 0, ..me };
        assert!(Held::open(&gone)?.is_none());
        Ok(())
    }

    /// A job whose members are ended when the test is done, pass or fail.
    struct Ending(crate::windows::job::Job);

    impl Drop for Ending {
        fn drop(&mut self) {
            let _ = self.0.terminate(1);
        }
    }

    /// A child started without a window has a console of its own. A grandchild
    /// that inherited it is attached to it, still after the child exited, while
    /// processes with a console of their own or none are not, and one whose
    /// console is not set up yet is neither. The exited child's console stays
    /// known.
    #[test]
    fn consoles_tell_who_shares_them() -> Result<(), Error> {
        use std::os::windows::process::CommandExt;
        use std::time::{Duration, Instant};

        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        use crate::windows::process::{CREATE_NO_WINDOW, DETACHED_PROCESS};
        use crate::windows::spawn::{Spawn, spawn};

        let job = Ending(crate::windows::job::Job::create()?);
        let cmd = std::path::PathBuf::from(
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into()),
        );
        let child = spawn(&Spawn {
            program: &cmd,
            command_line: std::ffi::OsStr::new(
                r#"cmd.exe /d /c "start /b ping -n 30 127.0.0.1 >NUL& ping -n 3 127.0.0.1 >NUL""#,
            ),
            environment: None,
            cwd: &std::env::temp_dir(),
            stdout: crate::windows::spawn::Stdio::Null,
            stderr: crate::windows::spawn::Stdio::Null,
            job: Some(&job.0),
            console: None,
        })?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let (console, pings) = loop {
            let console = child.console()?;
            let pings: Vec<ProcessInfo> = processes()?
                .into_iter()
                .filter(|p| p.ppid == child.pid() && p.name.eq_ignore_ascii_case("PING.EXE"))
                .collect();
            if let Some(console) = console
                && pings.len() == 2
            {
                break (console, pings);
            }
            assert!(Instant::now() < deadline, "no console or pings: {pings:?}");
            std::thread::sleep(Duration::from_millis(20));
        };
        let leader = ProcessIdentity {
            pid: child.pid(),
            sid: child.pid(),
            start_time: child.start_time(),
        };
        assert_eq!(console.attached(&leader)?, OnConsole::Yes);
        child.wait()?;
        // The background ping outlives the child on the same console.
        let attached: Vec<OnConsole> = pings
            .iter()
            .map(|ping| console.attached(&ping.identity()))
            .collect::<io::Result<_>>()?;
        assert!(attached.contains(&OnConsole::Yes), "{pings:?} {attached:?}");
        assert_eq!(
            console.attached(&leader)?,
            OnConsole::No,
            "an exited process is on no console"
        );
        // The query keeps its value after the exit, so the console is still
        // known from the exited child, whose handle `child` holds.
        let exited_child = open(child.pid(), PROCESS_QUERY_LIMITED_INFORMATION)?
            .ok_or(Error::Unsupported("held child missing"))?;
        assert_eq!(
            console_host(&exited_child)?,
            ConsoleHost::Host(console.host)
        );
        let last = child.last_console().map_err(io::Error::other)?;
        assert_eq!(last.host, console.host);
        // A console host reports its client, not a console of its own.
        let host = crate::identity(console.host)?.ok_or(Error::Unsupported("host missing"))?;
        assert_eq!(console_host_of(&host)?, Some(ConsoleHost::Pending));
        for (flags, expected) in [
            (CREATE_NO_WINDOW, OnConsole::No),
            (DETACHED_PROCESS, OnConsole::No),
            // Not running yet, so its console is not set up.
            (CREATE_SUSPENDED, OnConsole::NotYet),
        ] {
            let mut other = Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .creation_flags(flags)
                .spawn()?;
            // Time to set up its own console, which takes milliseconds.
            std::thread::sleep(Duration::from_millis(200));
            let identity = crate::identity(other.id())?;
            let result = identity.map(|identity| {
                Ok::<_, io::Error>((console.attached(&identity)?, console_host_of(&identity)?))
            });
            let _ = other.kill();
            let _ = other.wait();
            let (on, host) = result
                .transpose()?
                .ok_or(Error::Unsupported("ping missing"))?;
            assert_eq!(on, expected, "flags {flags:#x}");
            if flags == DETACHED_PROCESS {
                assert_eq!(host, Some(ConsoleHost::Detached));
            }
        }
        Ok(())
    }

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
        // This process started before the test and well within a day of it.
        let own = age(looked_up.start_time);
        assert!(
            own > std::time::Duration::ZERO && own < std::time::Duration::from_secs(86_400),
            "{own:?}"
        );
        assert_eq!(age(u64::MAX), std::time::Duration::ZERO);
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

    /// An exited process that someone still holds a handle to is a zombie, and
    /// disappears once the last handle closes.
    #[test]
    fn exited_but_held_is_a_zombie() -> Result<(), Error> {
        let mut child = Command::new("cmd")
            .args(["/d", "/c", "exit 0"])
            .stdout(Stdio::null())
            .spawn()?;
        let pid = child.id();
        // `wait` keeps the handle open until `child` is dropped.
        child.wait()?;
        let held = process(pid)?.ok_or(Error::Unsupported("held process missing"))?;
        assert!(held.zombie, "{held:?}");
        assert!(!crate::matches(&held.identity())?);
        // Snapshots leave exited processes out, held or not, as macOS does.
        assert!(!processes()?.iter().any(|p| p.pid == pid && !p.zombie));
        drop(child);
        assert!(process(pid)?.is_none_or(|p| p.start_time != held.start_time));
        Ok(())
    }
}
