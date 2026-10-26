//! Suspending, resuming and throttling verified processes, for idle lanes.
//!
//! Suspension goes through `NtSuspendProcess` and `NtResumeProcess`, which add
//! one to and take one from the suspend count of every thread of a process; a
//! thread runs again only once its count is back to zero. A [`PowerHandle`]
//! suspends at most once and takes back only its own suspension. A resume
//! that fails leaves the suspension on the handle, so the caller can try
//! again through the same handle.
//!
//! Throttling is Task Manager's efficiency mode: `IDLE_PRIORITY_CLASS` plus
//! EcoQoS (`ProcessPowerThrottling` with `PROCESS_POWER_THROTTLING_EXECUTION_SPEED`).

use std::io;
use std::os::windows::io::{AsHandle, BorrowedHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE, NTSTATUS};
use windows_sys::Win32::System::Threading::{
    GetPriorityClass, GetProcessInformation, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE, ProcessPowerThrottling,
    SetPriorityClass, SetProcessInformation,
};

use crate::proc::{creation_time, has_exited, open};
use crate::util::raw;

// windows-sys has no metadata for these ntdll exports. Linked the way
// windows-link links the rest, so no import library is needed.
#[cfg_attr(
    target_arch = "x86",
    link(
        name = "ntdll.dll",
        kind = "raw-dylib",
        modifiers = "+verbatim",
        import_name_type = "undecorated"
    )
)]
#[cfg_attr(
    not(target_arch = "x86"),
    link(name = "ntdll.dll", kind = "raw-dylib", modifiers = "+verbatim")
)]
unsafe extern "system" {
    /// Adds one to the suspend count of every thread of the process. Needs
    /// `PROCESS_SUSPEND_RESUME`.
    fn NtSuspendProcess(process: HANDLE) -> NTSTATUS;
    /// Takes one from the suspend count of every thread of the process. Needs
    /// `PROCESS_SUSPEND_RESUME`.
    fn NtResumeProcess(process: HANDLE) -> NTSTATUS;
}

/// The priority class of a process, as `GetPriorityClass` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityClass(u32);

impl PriorityClass {
    /// `IDLE_PRIORITY_CLASS`: threads run only when nothing else wants the
    /// CPU. Children of such a process start with it too.
    pub const IDLE: Self = Self(IDLE_PRIORITY_CLASS);
    /// `NORMAL_PRIORITY_CLASS`, what a process gets unless its parent or its
    /// creator chose otherwise.
    pub const NORMAL: Self = Self(NORMAL_PRIORITY_CLASS);

    /// The `*_PRIORITY_CLASS` value.
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// The power throttling a process asked for (`PROCESS_POWER_THROTTLING_STATE`
/// without its version). Mechanisms outside `control_mask` are left to
/// Windows; for those inside, `state_mask` says whether each is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PowerThrottling {
    /// The `PROCESS_POWER_THROTTLING_*` mechanisms the process controls.
    pub control_mask: u32,
    /// Which of the controlled mechanisms are on.
    pub state_mask: u32,
}

impl PowerThrottling {
    /// Every mechanism left to Windows, the state of a process that never
    /// chose.
    pub const DEFAULT: Self = Self {
        control_mask: 0,
        state_mask: 0,
    };

    /// Whether EcoQoS (execution speed throttling) is explicitly on.
    pub fn eco_qos(self) -> bool {
        let speed = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        self.control_mask & speed != 0 && self.state_mask & speed != 0
    }

    /// The same choices with EcoQoS explicitly on.
    pub fn with_eco_qos(self) -> Self {
        let speed = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        Self {
            control_mask: self.control_mask | speed,
            state_mask: self.state_mask | speed,
        }
    }
}

/// What a [`PowerHandle`] is opened for. Each asks only for the rights its
/// calls need, so a process whose security allows one is not refused for the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAccess {
    /// [`PowerHandle::suspend`] and [`PowerHandle::resume`]
    /// (`PROCESS_SUSPEND_RESUME`).
    Suspend,
    /// The priority class and power throttling, read and set
    /// (`PROCESS_SET_INFORMATION`, and `PROCESS_QUERY_INFORMATION`, which
    /// `GetProcessInformation` needs).
    Throttle,
}

/// A process opened to suspend and resume it, or to throttle it, verified by
/// start time when opened. The open handle keeps its PID from being reused
/// while this is held, so later calls always reach the same process. Calls
/// that need rights the handle was not opened with fail with access denied.
///
/// Dropping a handle that still holds a suspension tries once to resume the
/// process; if that fails too, the process stays suspended, so callers that
/// care resume explicitly and keep the handle on failure.
#[derive(Debug)]
pub struct PowerHandle {
    handle: OwnedHandle,
    pid: u32,
    start_time: u64,
    suspended: bool,
}

impl PowerHandle {
    /// Opens `pid` for `access`. `None` when it is gone, has exited, or its
    /// PID now names a process started at another time. A live process that
    /// refuses those rights is an error.
    pub fn open(pid: u32, start_time: u64, access: PowerAccess) -> io::Result<Option<Self>> {
        let rights = PROCESS_QUERY_LIMITED_INFORMATION
            | match access {
                PowerAccess::Suspend => PROCESS_SUSPEND_RESUME,
                PowerAccess::Throttle => PROCESS_SET_INFORMATION | PROCESS_QUERY_INFORMATION,
            };
        // `SYNCHRONIZE` only makes exit checks reliable; a process that
        // refuses it is opened without, as `proc` does for queries.
        let opened = match open(pid, rights | PROCESS_SYNCHRONIZE) {
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => {
                open(pid, rights)
            }
            other => other,
        };
        let handle = match opened {
            Ok(Some(handle)) => handle,
            Ok(None) => return Ok(None),
            // A refusal only counts when it came from the recorded process.
            Err(error) => {
                return match crate::proc::start_time(pid) {
                    Ok(None) => Ok(None),
                    Ok(Some(other)) if other != start_time => Ok(None),
                    _ => Err(error),
                };
            }
        };
        if creation_time(&handle)? != start_time || has_exited(&handle) {
            return Ok(None);
        }
        Ok(Some(Self {
            handle,
            pid,
            start_time,
            suspended: false,
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

    /// Whether this handle holds a suspension of the process.
    pub fn suspended(&self) -> bool {
        self.suspended
    }

    /// Suspends every thread the process has now. A suspended process cannot
    /// start threads or processes itself, but another process can still
    /// create a thread in it. Does nothing when this handle already suspended
    /// it. Needs [`PowerAccess::Suspend`].
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.suspended {
            return Ok(());
        }
        // SAFETY: the handle is open with PROCESS_SUSPEND_RESUME; the call
        // takes no other argument.
        nt_result(unsafe { NtSuspendProcess(raw(&self.handle)) })?;
        self.suspended = true;
        Ok(())
    }

    /// Takes back this handle's suspension. Does nothing when it holds none.
    /// Threads someone else suspended stay suspended. A process that exited
    /// meanwhile counts as resumed. On failure the handle still holds the
    /// suspension, so a later call tries again. Needs [`PowerAccess::Suspend`].
    pub fn resume(&mut self) -> io::Result<()> {
        if !self.suspended {
            return Ok(());
        }
        // SAFETY: as in `suspend`.
        match nt_result(unsafe { NtResumeProcess(raw(&self.handle)) }) {
            Err(error) if !has_exited(&self.handle) => Err(error),
            Ok(()) | Err(_) => {
                self.suspended = false;
                Ok(())
            }
        }
    }

    /// The process's priority class. Works with either [`PowerAccess`].
    pub fn priority_class(&self) -> io::Result<PriorityClass> {
        // SAFETY: every handle has PROCESS_QUERY_LIMITED_INFORMATION.
        match unsafe { GetPriorityClass(raw(&self.handle)) } {
            0 => Err(io::Error::last_os_error()),
            class => Ok(PriorityClass(class)),
        }
    }

    /// Sets the process's priority class. Children it starts afterwards
    /// inherit [`PriorityClass::IDLE`], but not `NORMAL` or higher. Needs
    /// [`PowerAccess::Throttle`].
    pub fn set_priority_class(&self, class: PriorityClass) -> io::Result<()> {
        // SAFETY: the handle has PROCESS_SET_INFORMATION; `class` came from
        // GetPriorityClass or one of the constants.
        if unsafe { SetPriorityClass(raw(&self.handle), class.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The power throttling the process asked for, EcoQoS among it. Needs
    /// [`PowerAccess::Throttle`].
    pub fn power_throttling(&self) -> io::Result<PowerThrottling> {
        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: 0,
            StateMask: 0,
        };
        // SAFETY: the handle has PROCESS_QUERY_INFORMATION; `state` is
        // writable and matches the class and size.
        let ok = unsafe {
            GetProcessInformation(
                raw(&self.handle),
                ProcessPowerThrottling,
                (&raw mut state).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PowerThrottling {
            control_mask: state.ControlMask,
            state_mask: state.StateMask,
        })
    }

    /// Replaces the process's power throttling choices with `throttling`;
    /// mechanisms outside its control mask go back to Windows. Needs
    /// [`PowerAccess::Throttle`].
    pub fn set_power_throttling(&self, throttling: PowerThrottling) -> io::Result<()> {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: throttling.control_mask,
            StateMask: throttling.state_mask,
        };
        // SAFETY: the handle has PROCESS_SET_INFORMATION; `state` matches the
        // class and size and is only read.
        let ok = unsafe {
            SetProcessInformation(
                raw(&self.handle),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for PowerHandle {
    /// Tries once to resume the process if this handle still holds a
    /// suspension.
    fn drop(&mut self) {
        let _ = self.resume();
    }
}

impl AsHandle for PowerHandle {
    /// The process handle, open with the rights of its [`PowerAccess`].
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

/// What [`resume_verified`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resumed {
    /// One suspension was taken off every thread of the process.
    Sent,
    /// No process has the PID, or it had already exited.
    Gone,
    /// The PID now names a process started at another time; nothing was done.
    Mismatch,
}

/// Takes one suspension off every thread of `pid` only if it is still the
/// process that started at `start_time`, checked on the handle that resumes.
/// For processes a previous daemon suspended and never resumed; threads that
/// are not suspended are unaffected. A live process that refuses is an error,
/// unless its PID belongs to a process started at another time.
pub fn resume_verified(pid: u32, start_time: u64) -> io::Result<Resumed> {
    let access = PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let handle = match open(pid, access) {
        Ok(Some(handle)) => handle,
        Ok(None) => return Ok(Resumed::Gone),
        Err(error) => {
            return match crate::proc::start_time(pid) {
                Ok(None) => Ok(Resumed::Gone),
                Ok(Some(other)) if other != start_time => Ok(Resumed::Mismatch),
                _ => Err(error),
            };
        }
    };
    if creation_time(&handle)? != start_time {
        return Ok(Resumed::Mismatch);
    }
    if has_exited(&handle) {
        return Ok(Resumed::Gone);
    }
    // SAFETY: the handle is open with PROCESS_SUSPEND_RESUME.
    match nt_result(unsafe { NtResumeProcess(raw(&handle)) }) {
        Ok(()) => Ok(Resumed::Sent),
        Err(_) if has_exited(&handle) => Ok(Resumed::Gone),
        Err(error) => Err(error),
    }
}

/// An error for a failed `NTSTATUS`; success and informational statuses are
/// not negative.
fn nt_result(status: NTSTATUS) -> io::Result<()> {
    if status < 0 {
        return Err(io::Error::other(format!("NTSTATUS {status:#x}")));
    }
    Ok(())
}
