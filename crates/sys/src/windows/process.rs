//! Waiting on processes, the job this process runs in, and standard handles.

use std::fs::File;
use std::io;
use std::os::windows::io::IntoRawHandle;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE, SetStdHandle};
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use super::token;

/// `CreateProcess` flag: no console at all, so the terminal's Ctrl-C, close and
/// hangup events never reach the process.
pub const DETACHED_PROCESS: u32 = windows_sys::Win32::System::Threading::DETACHED_PROCESS;
/// `CreateProcess` flag: leave the caller's job object, so a terminal that kills
/// its job on close does not take the process with it.
pub const CREATE_BREAKAWAY_FROM_JOB: u32 =
    windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
/// `CreateProcess` flag: a console without a window, for console programs started
/// by a process that has none, so no window flashes.
pub const CREATE_NO_WINDOW: u32 = windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

/// Whether this process is attached to a console. Children of a process without
/// one get a console of their own, with a window unless [`CREATE_NO_WINDOW`] is set.
pub fn has_console() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleCP;
    // SAFETY: plain query; it fails (returns 0) when no console is attached.
    unsafe { GetConsoleCP() != 0 }
}

/// Clears the inherited "ignore Ctrl-C" flag, so processes this one starts receive
/// Ctrl-C again.
///
/// `CREATE_NEW_PROCESS_GROUP` (used by Git Bash, among others) sets the flag and
/// every descendant inherits it; Ctrl-C then reaches no unit, while sending it
/// still reports success. It works without a console.
pub fn accept_ctrl_c() -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: a null handler with FALSE only clears the process's ignore flag.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets the inherited "ignore Ctrl-C" flag that [`accept_ctrl_c`] clears, so
/// processes started afterwards begin as under Git Bash. For tests of programs
/// that must clear it themselves.
pub fn ignore_ctrl_c() -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: a null handler with TRUE only sets the process's ignore flag.
    if unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Runs `spawn` while this process's standard handles are not inheritable, then
/// restores them.
///
/// std starts Windows children with handle inheritance on, so a child also gets
/// every inheritable handle of this process, not only the stdio it was given. A
/// long-lived child (the daemon) would then hold the caller's stdout pipe, and a
/// shell reading it (`$(devlanes status)`) would wait for end of file forever.
/// Children given `Stdio::inherit`, here or on other threads, are unaffected:
/// std passes them inheritable duplicates of the standard handles.
pub fn without_inherited_std_handles<T>(spawn: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_INPUT_HANDLE};
    let mut cleared = Vec::new();
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: reads the standard handle without taking ownership.
        let handle = unsafe { GetStdHandle(which) };
        if handle.is_null() || handle as isize == -1 {
            continue;
        }
        let mut flags = 0;
        // SAFETY: `handle` is this process's standard handle; `flags` is writable.
        if unsafe { GetHandleInformation(handle, &raw mut flags) } != 0
            && flags & HANDLE_FLAG_INHERIT != 0
            // SAFETY: changes only the inherit flag of a handle this process owns.
            && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0
        {
            cleared.push(handle);
        }
    }
    let result = spawn();
    for handle in cleared {
        // SAFETY: restores the flag cleared above on the same, still-open handle.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    result
}

/// Waits up to `timeout` for process `pid` to exit; `true` once it is gone.
///
/// Only for waiting: a recycled PID could make this wait on an unrelated process,
/// which costs time but never acts on it.
pub fn wait_exit(pid: u32, timeout: Duration) -> io::Result<bool> {
    let Some(process) = super::proc::open(pid, PROCESS_SYNCHRONIZE)? else {
        return Ok(true);
    };
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: `process` is an open handle with SYNCHRONIZE rights.
    match unsafe { WaitForSingleObject(token::raw(&process), millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Limits of the job object this process belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobInfo {
    /// Closing the job's last handle kills every process in it, which is how some
    /// terminals (VS Code's, CI runners) clean up when they close.
    pub kill_on_close: bool,
    /// Children may leave the job with `CREATE_BREAKAWAY_FROM_JOB`.
    pub breakaway_ok: bool,
}

/// The job this process runs in, or `None` outside any job.
pub fn job_of_self() -> io::Result<Option<JobInfo>> {
    let mut in_job = 0;
    // SAFETY: the pseudo handle is always valid; a null job asks about any job.
    if unsafe { IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &raw mut in_job) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if in_job == 0 {
        return Ok(None);
    }
    // SAFETY: an all-zero bit pattern is a valid value for this plain C structure.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a null job handle queries the caller's job; `info` matches the class.
    let ok = unsafe {
        QueryInformationJobObject(
            ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = info.BasicLimitInformation.LimitFlags;
    Ok(Some(JobInfo {
        kill_on_close: flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
        breakaway_ok: flags
            & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)
            != 0,
    }))
}

/// Points standard output at `NUL` and closes the original handle, so the reader
/// of an inherited pipe sees end of file. std looks up the standard handle on each
/// write, so later writes go to `NUL`.
///
/// A parent may pass one handle as several standard streams; every slot holding
/// the original is swapped too, or closing it would leave that slot naming a
/// handle value the next open file could reuse.
pub fn release_stdout() -> io::Result<()> {
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_INPUT_HANDLE};
    let null = File::options().read(true).write(true).open("NUL")?;
    // SAFETY: reads the current standard handle without taking ownership.
    let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let null = null.into_raw_handle();
    // SAFETY: `null` is an open handle whose ownership moves to the process's
    // standard handle table.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, null) } == 0 {
        let error = io::Error::last_os_error();
        // SAFETY: the swap failed, so `null` is still ours to close.
        unsafe { CloseHandle(null) };
        return Err(error);
    }
    if original.is_null() || original as isize == -1 {
        return Ok(());
    }
    for which in [STD_INPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: reads a standard handle without taking ownership.
        if unsafe { GetStdHandle(which) } == original
            // SAFETY: `null` stays open in the standard handle table.
            && unsafe { SetStdHandle(which, null) } == 0
        {
            // Closing would leave this slot dangling; keeping the pipe open only
            // delays the client's end of file until the daemon exits.
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: no standard slot refers to the original handle any more; closing it
    // releases the pipe end the spawning client waits on.
    unsafe { CloseHandle(original) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing PID counts as exited; this process does not exit within the wait.
    #[test]
    fn waits_on_processes() -> io::Result<()> {
        assert!(wait_exit(u32::MAX - 3, Duration::ZERO)?);
        assert!(!wait_exit(std::process::id(), Duration::from_millis(10))?);
        job_of_self()?;
        Ok(())
    }
}
