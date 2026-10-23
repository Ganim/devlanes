//! Resource readings for `devlanes-metrics-windows`: CPU time and memory of one
//! process, and the machine's CPU times, memory load and low-memory state.
//!
//! Processes are opened with `PROCESS_QUERY_LIMITED_INFORMATION` only, the
//! least access these calls take, so every process of the daemon's user can be
//! read, including elevated ones started by a unit.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::System::Memory::{
    CreateMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

use super::proc::{open_verified, times};
use super::token::{owned, raw};
use crate::ProcessIdentity;

/// The low-memory notification object, created on first use and kept for the
/// process's life: the object is shared system state, so one handle is enough.
static LOW_MEMORY: OnceLock<OwnedHandle> = OnceLock::new();

/// CPU time and memory of one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUsage {
    /// Kernel plus user CPU time over the process's life, in 100 ns units.
    pub cpu_time: u64,
    /// Working set: resident pages, shared ones (DLLs, shared memory) included.
    pub working_set: u64,
    /// Private working set: resident pages no other process shares, what Task
    /// Manager's Memory column shows.
    pub private_working_set: u64,
}

/// Machine-wide CPU times since boot, in 100 ns units summed over all CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTimes {
    pub idle: u64,
    /// Kernel time, idle time included.
    pub kernel: u64,
    pub user: u64,
}

/// Physical memory use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryStatus {
    pub total_physical: u64,
    /// Physical memory available without paging anything out: free, zeroed and
    /// standby (cache) pages.
    pub available_physical: u64,
}

/// Reads the usage of `identity`, or `None` when the process is gone, exited,
/// refuses access (protected or another user's) or its PID now names a process
/// started at another time.
pub fn process_usage(identity: &ProcessIdentity) -> io::Result<Option<ProcessUsage>> {
    let handle = match open_verified(identity, 0) {
        Ok(Some(handle)) => handle,
        Ok(None) => return Ok(None),
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let times = times(&handle)?;
    let memory = memory_counters(&handle)?;
    Ok(Some(ProcessUsage {
        cpu_time: times.kernel.saturating_add(times.user),
        ..memory
    }))
}

/// Idle, kernel and user time of all CPUs since boot (`GetSystemTimes`).
pub fn system_times() -> io::Result<SystemTimes> {
    let mut times = [0_u64; 3];
    let [idle, kernel, user] = times.each_mut().map(ptr::from_mut);
    // SAFETY: a FILETIME is two u32 halves, low half first, so on little-endian
    // Windows each writable u64 has its layout, with stricter alignment.
    if unsafe { GetSystemTimes(idle.cast(), kernel.cast(), user.cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let [idle, kernel, user] = times;
    Ok(SystemTimes { idle, kernel, user })
}

/// Physical memory load and availability (`GlobalMemoryStatusEx`).
pub fn memory_status() -> io::Result<MemoryStatus> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        // SAFETY: an all-zero value is valid for the rest of this plain C struct.
        ..unsafe { std::mem::zeroed() }
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

/// Whether Windows reports low physical memory
/// (`QueryMemoryResourceNotification` on a `LowMemoryResourceNotification`
/// object). The system sets it when available memory falls below its own
/// threshold, about 32 MB per 4 GB of RAM and at most 64 MB, so it means memory
/// is nearly exhausted.
pub fn low_memory() -> io::Result<bool> {
    let handle = match LOW_MEMORY.get() {
        Some(handle) => handle,
        None => {
            // SAFETY: plain call with a valid notification type; the result is
            // checked for null and owned below.
            let created =
                owned(unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) })?;
            // A racing caller may have stored its handle first; either one works and
            // the loser's is closed on drop.
            let _ = LOW_MEMORY.set(created);
            LOW_MEMORY
                .get()
                .ok_or_else(|| io::Error::other("low-memory notification missing"))?
        }
    };
    let mut state = 0;
    // SAFETY: `handle` is a live notification object; `state` is writable.
    if unsafe { QueryMemoryResourceNotification(raw(handle), &raw mut state) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(state != 0)
}

/// Memory counters of an open process (`PROCESS_MEMORY_COUNTERS_EX2`, which
/// Windows 11 24H2 fills in), CPU time left at 0.
fn memory_counters(handle: &OwnedHandle) -> io::Result<ProcessUsage> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        // SAFETY: an all-zero value is valid for the rest of this plain C struct.
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: `handle` is open with query rights; `counters` is writable and
    // `cb` is the size passed. EX2 starts with the base layout, so the cast is
    // the documented way to pass it.
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
    Ok(ProcessUsage {
        cpu_time: 0,
        working_set: counters.WorkingSetSize as u64,
        private_working_set: counters.PrivateWorkingSetSize as u64,
    })
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::Error;

    /// Kills and reaps a test child even when an assertion fails first.
    struct Reaped(Child);

    impl Drop for Reaped {
        /// Best-effort cleanup; the child may already have exited.
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A `cmd` spinning one core until killed.
    fn busy_child() -> Result<Reaped, Error> {
        let child = Command::new("cmd")
            .raw_arg("/d /c \"for /l %i in (0,0,1) do @rem\"")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Reaped(child))
    }

    /// The identity of a live child.
    fn identity_of(child: &Reaped) -> Result<ProcessIdentity, Error> {
        crate::identity(child.0.id())?.ok_or(Error::Unsupported("child identity missing"))
    }

    /// A spinning child accumulates CPU time and has resident private memory.
    #[test]
    fn busy_child_uses_cpu_and_memory() -> Result<(), Error> {
        let child = busy_child()?;
        let identity = identity_of(&child)?;
        let first = process_usage(&identity)?.ok_or(Error::Unsupported("child unread"))?;
        assert!(first.working_set > 0, "{first:?}");
        let private = first.private_working_set;
        assert!(private > 0 && private <= first.working_set, "{first:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let later = process_usage(&identity)?.ok_or(Error::Unsupported("child unread"))?;
            // 100 ms of spinning is a million 100 ns ticks; half proves it ran.
            if later.cpu_time >= first.cpu_time + 500_000 {
                return Ok(());
            }
            assert!(Instant::now() < deadline, "no CPU time in 10 s: {later:?}");
        }
    }

    /// Another start time, PID 0 and an exited child (still held) are skipped.
    #[test]
    fn stale_gone_and_exited_are_none() -> Result<(), Error> {
        let mut child = busy_child()?;
        let identity = identity_of(&child)?;
        let stale = ProcessIdentity {
            start_time: identity.start_time + 1,
            ..identity
        };
        assert_eq!(process_usage(&stale)?, None);
        assert_eq!(
            process_usage(&ProcessIdentity { pid: 0, ..identity })?,
            None
        );
        child.0.kill()?;
        // `wait` reaps but `child` keeps its handle, so the process object stays.
        child.0.wait()?;
        assert_eq!(process_usage(&identity)?, None);
        Ok(())
    }

    /// System CPU counters move forward and kernel time includes idle time.
    #[test]
    fn system_times_advance() -> io::Result<()> {
        let first = system_times()?;
        assert!(first.kernel >= first.idle, "{first:?}");
        std::thread::sleep(Duration::from_millis(50));
        let later = system_times()?;
        assert!(
            later.idle >= first.idle && later.kernel >= first.kernel,
            "{later:?}"
        );
        assert!(later.user >= first.user, "{later:?}");
        Ok(())
    }

    /// Availability fits in the total.
    #[test]
    fn memory_status_is_consistent() -> io::Result<()> {
        let status = memory_status()?;
        assert!(status.total_physical > 0, "{status:?}");
        assert!(
            status.available_physical <= status.total_physical,
            "{status:?}"
        );
        Ok(())
    }

    /// The low-memory query answers repeatedly through the one shared object.
    #[test]
    fn low_memory_answers() -> io::Result<()> {
        low_memory()?;
        low_memory()?;
        assert!(LOW_MEMORY.get().is_some());
        Ok(())
    }
}
