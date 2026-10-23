//! Resource readings for the metrics collector and the start gate: CPU time
//! and memory of one process, and the machine's CPU times, memory and
//! low-memory state.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::System::Memory::{
    CreateMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

use crate::proc::{cpu_time, open_query_verified};
use crate::util::{owned, raw};

/// CPU time and memory of one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUsage {
    /// Kernel plus user CPU time over the process's life, in 100 ns units.
    pub cpu_time: u64,
    /// Working set: resident pages, shared ones (DLLs) included.
    pub working_set: u64,
    /// Private working set: resident pages no other process shares, what Task
    /// Manager's Memory column shows.
    pub private_working_set: u64,
}

/// Machine-wide CPU times since boot, in 100 ns units summed over all CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTimes {
    /// Idle time.
    pub idle: u64,
    /// Kernel time, idle time included.
    pub kernel: u64,
    /// User time.
    pub user: u64,
}

/// Physical memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryStatus {
    /// Installed memory usable by Windows.
    pub total_physical: u64,
    /// Memory available without paging anything out: free, zeroed and
    /// standby (cache) pages.
    pub available_physical: u64,
}

/// The usage of `pid`, or `None` when it is gone, has exited, refuses
/// queries, or its PID names a process started at another time than
/// `start_time`. Needs only `PROCESS_QUERY_LIMITED_INFORMATION`, so elevated
/// processes of the same user can be read too.
pub fn process_usage(pid: u32, start_time: u64) -> io::Result<Option<ProcessUsage>> {
    let Some(handle) = open_query_verified(pid, start_time)? else {
        return Ok(None);
    };
    let (working_set, private_working_set) = memory_counters(&handle)?;
    Ok(Some(ProcessUsage {
        cpu_time: cpu_time(&handle)?,
        working_set,
        private_working_set,
    }))
}

/// Idle, kernel and user time of all CPUs since boot.
pub fn system_times() -> io::Result<SystemTimes> {
    let mut times = [0_u64; 3];
    let [idle, kernel, user] = times.each_mut().map(ptr::from_mut);
    // SAFETY: a FILETIME is two u32 halves, low half first, so on
    // little-endian Windows each writable u64 has its layout, with stricter
    // alignment.
    if unsafe { GetSystemTimes(idle.cast(), kernel.cast(), user.cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let [idle, kernel, user] = times;
    Ok(SystemTimes { idle, kernel, user })
}

/// Total and available physical memory.
pub fn memory_status() -> io::Result<MemoryStatus> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is writable and `dwLength` is set as the API requires.
    if unsafe { GlobalMemoryStatusEx(&raw mut status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(MemoryStatus {
        total_physical: status.ullTotalPhys,
        available_physical: status.ullAvailPhys,
    })
}

/// Whether Windows reports low physical memory. The system sets the state
/// when available memory falls below its own threshold, about 32 MB per 4 GB
/// of RAM and at most 64 MB, so it means memory is nearly exhausted.
pub fn low_memory() -> io::Result<bool> {
    // The notification object is shared system state, so one handle per
    // process is enough; it lives as long as the process.
    static LOW_MEMORY: OnceLock<OwnedHandle> = OnceLock::new();
    let handle = match LOW_MEMORY.get() {
        Some(handle) => handle,
        None => {
            // SAFETY: plain call with a valid notification type; a null
            // result is checked by `owned`.
            let created =
                owned(unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) })?;
            // A racing caller may have stored its handle first; either works.
            LOW_MEMORY.get_or_init(|| created)
        }
    };
    let mut state = 0;
    // SAFETY: `handle` is a live notification object; `state` is writable.
    if unsafe { QueryMemoryResourceNotification(raw(handle), &raw mut state) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(state != 0)
}

/// Working set and private working set of an open process, from
/// `PROCESS_MEMORY_COUNTERS_EX2`.
fn memory_counters(handle: &OwnedHandle) -> io::Result<(u64, u64)> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    // SAFETY: the handle has query rights; `counters` is writable and `cb` is
    // the size passed. EX2 starts with the base layout, so the cast is the
    // documented way to pass it.
    let ok = unsafe {
        GetProcessMemoryInfo(
            raw(handle),
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        counters.WorkingSetSize as u64,
        counters.PrivateWorkingSetSize as u64,
    ))
}
