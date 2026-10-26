//! The process table: snapshots, single lookups, held and verified handles,
//! listening sockets, established TCP connections and the boot id.
//!
//! A process's start time is its creation `FILETIME` as one `u64` (100 ns
//! ticks since 1601 UTC), recorded once at creation, so clock changes never
//! alter it. Windows reuses PIDs quickly, so every action on a recorded
//! process opens it and compares the start time on that same handle first.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::ptr;
use std::time::Duration;

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemTimeOfDayInformation,
};
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE,
    NO_ERROR, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP_STATE_ESTAB, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
    MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_CONNECTIONS,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, INFINITE, OpenProcess, PROCESS_ACCESS_RIGHTS,
    PROCESS_BASIC_INFORMATION, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject,
};

use crate::util::{owned, raw, until_nul, win32_error};

/// One process of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    /// The process ID.
    pub pid: u32,
    /// The parent's PID at creation; the parent may be gone and its PID reused.
    pub ppid: u32,
    /// The creation time, the identity token that tells PID reuse apart.
    pub start_time: u64,
    /// The executable's file name, such as `node.exe`.
    pub name: String,
    /// The process has exited but someone still holds a handle to it, the
    /// counterpart of an unreaped Unix child.
    pub exited: bool,
}

/// Every process this user can query. Protected processes and other users'
/// processes that refuse `PROCESS_QUERY_LIMITED_INFORMATION` are left out:
/// they have no start time to give.
pub fn processes() -> io::Result<Vec<ProcessEntry>> {
    // SAFETY: plain call; the result is checked before use.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = owned(snapshot)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = Vec::new();
    // SAFETY: `snapshot` is open and `entry.dwSize` is set as the API requires.
    let mut more = unsafe { Process32FirstW(raw(&snapshot), &raw mut entry) } != 0;
    while more {
        if let Some(handle) = open_query(entry.th32ProcessID)
            && let Ok(start_time) = creation_time(&handle)
        {
            found.push(ProcessEntry {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                start_time,
                name: String::from_utf16_lossy(until_nul(&entry.szExeFile)),
                exited: has_exited(&handle),
            });
        }
        // SAFETY: as above, continuing the same enumeration.
        more = unsafe { Process32NextW(raw(&snapshot), &raw mut entry) } != 0;
    }
    Ok(found)
}

/// One process, or `None` when no process has the PID or it refuses queries.
/// A process that exited while someone still holds a handle is reported with
/// `exited: true`.
pub fn process(pid: u32) -> io::Result<Option<ProcessEntry>> {
    process_where(pid, |_| true)
}

/// [`process`], but `None` also when `accept` refuses the process's open
/// handle; for checks that must name the same process as the entry.
pub(crate) fn process_where(
    pid: u32,
    accept: impl FnOnce(&OwnedHandle) -> bool,
) -> io::Result<Option<ProcessEntry>> {
    let Some(handle) = open_query(pid) else {
        return Ok(None);
    };
    if !accept(&handle) {
        return Ok(None);
    }
    Ok(Some(ProcessEntry {
        pid,
        ppid: parent_of(&handle).unwrap_or(0),
        start_time: creation_time(&handle)?,
        name: image_name(&handle).unwrap_or_default(),
        exited: has_exited(&handle),
    }))
}

/// The start time of `pid`, or `None` when no process has the PID or it
/// refuses queries.
pub fn start_time(pid: u32) -> io::Result<Option<u64>> {
    open_query(pid)
        .map(|handle| creation_time(&handle))
        .transpose()
}

/// Whether `pid` still names the running process that started at
/// `start_time`. Needs only `PROCESS_QUERY_LIMITED_INFORMATION`, so elevated
/// processes of the same user can be matched too.
pub fn matches(pid: u32, start_time: u64) -> io::Result<bool> {
    Ok(open_query_verified(pid, start_time)?.is_some())
}

/// The parent PID `pid` recorded at creation, or `None` when no process has
/// the PID or it refuses queries. The parent may be gone and its PID reused.
pub fn parent_pid(pid: u32) -> io::Result<Option<u32>> {
    Ok(open_query(pid).and_then(|handle| parent_of(&handle)))
}

/// A running process verified by start time, whose PID cannot be reused while
/// this is held: an open handle keeps the process object, and so its PID,
/// alive. Use it to act on a process by bare PID afterwards (attaching to its
/// console, matching its windows) without reaching a stranger.
#[derive(Debug)]
pub struct Held {
    handle: OwnedHandle,
    pid: u32,
    start_time: u64,
}

impl Held {
    /// Opens `pid` for queries and waiting; `None` when it is gone, has
    /// exited, refuses, or its PID now names a process started at another
    /// time. An elevated process opened from an unelevated one may refuse the
    /// `SYNCHRONIZE` right this needs.
    pub fn open(pid: u32, start_time: u64) -> io::Result<Option<Self>> {
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        let handle = match open(pid, access) {
            Ok(Some(handle)) => handle,
            Ok(None) => return Ok(None),
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if creation_time(&handle)? != start_time || has_exited(&handle) {
            return Ok(None);
        }
        Ok(Some(Self {
            handle,
            pid,
            start_time,
        }))
    }

    /// The process ID.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The start time it was verified against.
    pub fn start_time(&self) -> u64 {
        self.start_time
    }

    /// Whether the process has exited since it was opened.
    pub fn exited(&self) -> bool {
        has_exited(&self.handle)
    }

    /// Waits up to `timeout` for the process to exit; `true` once it has.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        wait_handle(&self.handle, Some(timeout))
    }
}

impl AsHandle for Held {
    /// The process handle, open with query and synchronize rights.
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

/// [`Held::open`].
pub fn open_verified(pid: u32, start_time: u64) -> io::Result<Option<Held>> {
    Held::open(pid, start_time)
}

/// What [`terminate_verified`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminated {
    /// The process was told to end.
    Sent,
    /// No process has the PID, or it had already exited.
    Gone,
    /// The PID now names a process started at another time; nothing was done.
    Mismatch,
}

/// Ends `pid` with `exit_code` only if it is still the process that started at
/// `start_time`. The check and the termination use one handle, so the PID
/// cannot be reused in between. A live process that refuses is an error,
/// unless its PID belongs to a process started at another time.
pub fn terminate_verified(pid: u32, start_time: u64, exit_code: u32) -> io::Result<Terminated> {
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let handle = match open(pid, access) {
        Ok(Some(handle)) => handle,
        Ok(None) => return Ok(Terminated::Gone),
        Err(error) => {
            return match open_query(pid).map(|handle| creation_time(&handle)) {
                Some(Ok(other)) if other != start_time => Ok(Terminated::Mismatch),
                _ => Err(error),
            };
        }
    };
    if creation_time(&handle)? != start_time {
        return Ok(Terminated::Mismatch);
    }
    if has_exited(&handle) {
        return Ok(Terminated::Gone);
    }
    // SAFETY: `handle` is open with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(raw(&handle), exit_code) } == 0 {
        let error = io::Error::last_os_error();
        // Access denied while it exits: it is gone either way.
        if has_exited(&handle) {
            return Ok(Terminated::Gone);
        }
        return Err(error);
    }
    Ok(Terminated::Sent)
}

/// Waits up to `timeout` for `pid` to exit; `true` once no process has the
/// PID or it has exited. Only for waiting: a reused PID could make this wait
/// on another process, which costs time but never acts on it.
pub fn wait_exit(pid: u32, timeout: Duration) -> io::Result<bool> {
    let Some(handle) = open(pid, PROCESS_SYNCHRONIZE)? else {
        return Ok(true);
    };
    wait_handle(&handle, Some(timeout))
}

/// An opaque token for the current boot: the kernel's boot time minus its
/// bias, which stays fixed when the clock is set (both move together) and
/// changes with the next boot.
pub fn boot_id() -> io::Result<String> {
    // SYSTEM_TIMEOFDAY_INFORMATION is 48 bytes: BootTime, CurrentTime,
    // TimeZoneBias, TimeZoneId plus padding, BootTimeBias, SleepTimeBias.
    let mut info = [0_i64; 6];
    // SAFETY: `info` is 48 writable, 8-aligned bytes, the structure's size.
    let status = unsafe {
        NtQuerySystemInformation(
            SystemTimeOfDayInformation,
            info.as_mut_ptr().cast(),
            size_of_val(&info) as u32,
            ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!("NTSTATUS {status:#x}")));
    }
    Ok(info[0].wrapping_sub(info[4]).to_string())
}

/// TCP listeners (IPv4 and IPv6) owned by `pids`, as `(pid, address)`.
pub fn listening_tcp(pids: &[u32]) -> io::Result<Vec<(u32, SocketAddr)>> {
    let mut sockets = Vec::new();
    for row in v4_rows(TCP_TABLE_OWNER_PID_LISTENER)? {
        if pids.contains(&row.dwOwningPid) {
            sockets.push((row.dwOwningPid, v4_addr(row.dwLocalAddr, row.dwLocalPort)));
        }
    }
    for row in v6_rows(TCP_TABLE_OWNER_PID_LISTENER)? {
        if pids.contains(&row.dwOwningPid) {
            let addr = v6_addr(row.ucLocalAddr, row.dwLocalPort, row.dwLocalScopeId);
            sockets.push((row.dwOwningPid, addr));
        }
    }
    Ok(sockets)
}

/// Every established TCP connection on the machine (IPv4 and IPv6), as
/// `(local, remote)`, both ends of loopback connections included. A
/// connection the kernel completed but no process has accepted yet is listed
/// too, so connections to a suspended server show up.
pub fn established_tcp() -> io::Result<Vec<(SocketAddr, SocketAddr)>> {
    let established = MIB_TCP_STATE_ESTAB.cast_unsigned();
    let mut connections = Vec::new();
    // The connections table leaves listeners out, so it stays small.
    for row in v4_rows(TCP_TABLE_OWNER_PID_CONNECTIONS)? {
        if row.dwState == established {
            connections.push((
                v4_addr(row.dwLocalAddr, row.dwLocalPort),
                v4_addr(row.dwRemoteAddr, row.dwRemotePort),
            ));
        }
    }
    for row in v6_rows(TCP_TABLE_OWNER_PID_CONNECTIONS)? {
        if row.dwState == established {
            connections.push((
                v6_addr(row.ucLocalAddr, row.dwLocalPort, row.dwLocalScopeId),
                v6_addr(row.ucRemoteAddr, row.dwRemotePort, row.dwRemoteScopeId),
            ));
        }
    }
    Ok(connections)
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
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed()) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Opens `pid` for queries, or `None` when it is gone or refuses.
///
/// `SYNCHRONIZE` is asked for too, because only a waitable handle tells an
/// exited process from a running one reliably; processes that refuse it are
/// opened without.
fn open_query(pid: u32) -> Option<OwnedHandle> {
    match open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(handle) => handle,
        // Protected processes refuse both, so only access denied gets a retry.
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => {
            open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok().flatten()
        }
        Err(_) => None,
    }
}

/// Opens `pid` for queries as [`open_query`] does; `None` also when it has
/// exited or its PID names a process started at another time.
pub(crate) fn open_query_verified(pid: u32, start_time: u64) -> io::Result<Option<OwnedHandle>> {
    let Some(handle) = open_query(pid) else {
        return Ok(None);
    };
    if creation_time(&handle)? != start_time || has_exited(&handle) {
        return Ok(None);
    }
    Ok(Some(handle))
}

/// The creation time of an open process as one 64-bit value.
pub(crate) fn creation_time(handle: &impl AsRawHandle) -> io::Result<u64> {
    Ok(process_times(handle)?[0])
}

/// Kernel plus user CPU time of an open process, in 100 ns units.
pub(crate) fn cpu_time(handle: &impl AsRawHandle) -> io::Result<u64> {
    let [_, _, kernel, user] = process_times(handle)?;
    Ok(kernel.saturating_add(user))
}

/// `GetProcessTimes`: creation, exit, kernel and user times as 64-bit values.
fn process_times(handle: &impl AsRawHandle) -> io::Result<[u64; 4]> {
    let mut times = [0_u64; 4];
    let [created, exited, kernel, user] = times.each_mut().map(ptr::from_mut);
    // SAFETY: the handle has query rights. A FILETIME is two u32 halves, low
    // half first, so on little-endian Windows each u64 has its layout, with
    // stricter alignment.
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
    Ok(times)
}

/// Whether an open process has exited. Waiting is preferred over the exit
/// code, because a running process can legitimately report 259
/// (`STILL_ACTIVE`); the code is the fallback for a handle without
/// `SYNCHRONIZE`.
pub(crate) fn has_exited(handle: &impl AsRawHandle) -> bool {
    // SAFETY: the handle is open; a zero timeout only polls.
    match unsafe { WaitForSingleObject(raw(handle), 0) } {
        WAIT_OBJECT_0 => true,
        WAIT_TIMEOUT => false,
        _ => exit_code_of(handle).is_ok_and(|code| code != STILL_ACTIVE as u32),
    }
}

/// The exit code `GetExitCodeProcess` reports (259 while running).
pub(crate) fn exit_code_of(handle: &impl AsRawHandle) -> io::Result<u32> {
    let mut code = 0;
    // SAFETY: the handle has query rights; `code` is writable.
    if unsafe { GetExitCodeProcess(raw(handle), &raw mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code)
}

/// Waits on a process handle with `SYNCHRONIZE` for up to `timeout`, or
/// forever with `None`; `true` once it has exited.
pub(crate) fn wait_handle(
    handle: &impl AsRawHandle,
    timeout: Option<Duration>,
) -> io::Result<bool> {
    // INFINITE is u32::MAX, so a finite wait stops just below it.
    let millis = timeout.map_or(INFINITE, |timeout| {
        u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1)
    });
    // SAFETY: the handle is open with SYNCHRONIZE.
    match unsafe { WaitForSingleObject(raw(handle), millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// The parent PID an open process recorded at creation.
fn parent_of(handle: &OwnedHandle) -> Option<u32> {
    let mut info = PROCESS_BASIC_INFORMATION {
        ExitStatus: 0,
        PebBaseAddress: ptr::null_mut(),
        AffinityMask: 0,
        BasePriority: 0,
        UniqueProcessId: 0,
        InheritedFromUniqueProcessId: 0,
    };
    // SAFETY: the handle has query rights; `info` matches the class and size.
    let status = unsafe {
        NtQueryInformationProcess(
            raw(handle),
            ProcessBasicInformation,
            (&raw mut info).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    // NT_SUCCESS: success and informational statuses are not negative.
    if status < 0 {
        return None;
    }
    u32::try_from(info.InheritedFromUniqueProcessId).ok()
}

/// The executable's file name, like the snapshot's `szExeFile`. Most paths
/// fit `MAX_PATH`; longer ones get one retry at the longest Windows allows.
fn image_name(handle: &OwnedHandle) -> Option<String> {
    for capacity in [260_usize, 32_768] {
        let mut buffer = vec![0_u16; capacity];
        let mut len = capacity as u32;
        // SAFETY: `buffer` holds `len` writable units; the call stores the
        // length used.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                raw(handle),
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &raw mut len,
            )
        };
        if ok != 0 {
            let path = String::from_utf16_lossy(&buffer[..len as usize]);
            return Some(path.rsplit('\\').next().unwrap_or(&path).to_owned());
        }
        if io::Error::last_os_error().raw_os_error()
            != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed())
        {
            return None;
        }
    }
    None
}

/// The TCP port in the low 16 bits of a table entry, in network byte order.
fn port(raw_port: u32) -> u16 {
    u16::from_be(raw_port as u16)
}

/// An IPv4 table entry's address and port, both in network byte order.
fn v4_addr(raw_ip: u32, raw_port: u32) -> SocketAddr {
    let ip = Ipv4Addr::from(raw_ip.to_ne_bytes());
    SocketAddr::V4(SocketAddrV4::new(ip, port(raw_port)))
}

/// An IPv6 table entry's address, port in network byte order, and scope.
fn v6_addr(raw_ip: [u8; 16], raw_port: u32, scope: u32) -> SocketAddr {
    let ip = Ipv6Addr::from(raw_ip);
    SocketAddr::V6(SocketAddrV6::new(ip, port(raw_port), 0, scope))
}

/// The IPv4 rows of the `class` table, one of the `TCP_TABLE_OWNER_PID_*`.
fn v4_rows(class: TCP_TABLE_CLASS) -> io::Result<Vec<MIB_TCPROW_OWNER_PID>> {
    let table = tcp_table(AF_INET, class)?;
    Ok(rows(
        &table,
        std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table),
    ))
}

/// The IPv6 rows of the `class` table, one of the `TCP_TABLE_OWNER_PID_*`.
fn v6_rows(class: TCP_TABLE_CLASS) -> io::Result<Vec<MIB_TCP6ROW_OWNER_PID>> {
    let table = tcp_table(AF_INET6, class)?;
    Ok(rows(
        &table,
        std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
    ))
}

/// The `class` table of one address family, as 8-aligned bytes holding a
/// `MIB_TCP*TABLE_OWNER_PID`: a row count followed by rows.
fn tcp_table(family: u16, class: TCP_TABLE_CLASS) -> io::Result<Vec<u64>> {
    let mut bytes = 0_u32;
    for _ in 0..5 {
        // A quarter more than the size asked for, so connections opened
        // between the size query and the read still fit.
        let wanted = bytes as usize + bytes as usize / 4;
        let mut buffer = vec![0_u64; wanted.div_ceil(8).max(1)];
        bytes = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: `buffer` holds `bytes` writable, aligned bytes.
        let status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &raw mut bytes,
                0,
                u32::from(family),
                class,
                0,
            )
        };
        match status {
            NO_ERROR => return Ok(buffer),
            // The table grew between the size query and the read; ask again.
            ERROR_INSUFFICIENT_BUFFER => {}
            other => return Err(win32_error(other)),
        }
    }
    Err(io::Error::other(
        "the TCP table kept growing while being read",
    ))
}

/// The rows of a table from [`tcp_table`] whose rows start at `offset`.
fn rows<Row: Copy>(table: &[u64], offset: usize) -> Vec<Row> {
    let count = (table[0] & 0xffff_ffff) as usize;
    let available = (table.len() * 8).saturating_sub(offset) / size_of::<Row>();
    let base = table.as_ptr().cast::<u8>();
    (0..count.min(available))
        .map(|index| {
            // SAFETY: the row lies within the buffer, as `available` bounds it;
            // `read_unaligned` makes no alignment assumption.
            unsafe {
                base.add(offset + index * size_of::<Row>())
                    .cast::<Row>()
                    .read_unaligned()
            }
        })
        .collect()
}
