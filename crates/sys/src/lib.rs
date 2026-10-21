//! Operating-system process queries for supervision, reconciliation and diagnostics.
//!
//! Platform code is selected at compile time: procfs on Linux, libproc and `lsof`
//! on macOS, Win32 on Windows. Every query is blocking; async callers should use
//! `spawn_blocking`.
//!
//! ```
//! # #[cfg(unix)] {
//! let me = devlanes_sys::identity(std::process::id())?.expect("own process is visible");
//! assert!(devlanes_sys::matches(&me)?);
//! # }
//! # Ok::<(), devlanes_sys::Error>(())
//! ```

// Unsafe code is limited to the macOS and Windows FFI modules; each block documents
// its invariants.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::net::SocketAddr;

pub use devlanes_core::ProcessIdentity;
#[cfg(unix)]
pub use rustix::process::Signal;

mod file;
mod rlimit;
mod tree;

#[cfg(any(target_os = "linux", test))]
mod procfs;

#[cfg(any(target_os = "macos", test))]
mod lsof;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
#[path = "macos.rs"]
mod os;

/// Safe wrappers over Win32 security, pipe, process and job calls, for the
/// endpoint, daemon and runner crates, so those need no `unsafe`.
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod windows;
#[cfg(windows)]
use windows as os;

/// Safe wrappers over macOS libproc, mach and sysctl calls for the metrics and
/// pressure collectors, so those crates need no `unsafe`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
#[path = "darwin.rs"]
pub mod macos;

pub use file::{FileIdentity, file_identity, path_identity};
pub use rlimit::{NofileLimit, child_nofile_limit, raise_nofile_limit};
pub use tree::{DescendantTracker, session_members, session_members_known};

/// Failures reading process tables or invoking diagnostic tools.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A system call or procfs read failed for a reason other than process exit.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A kernel table or tool output did not have the expected shape.
    #[error("malformed {source_name}: {detail}")]
    Malformed {
        source_name: &'static str,
        detail: String,
    },
    /// A diagnostic helper command was unavailable or failed.
    #[error("{tool} failed: {detail}")]
    Tool { tool: &'static str, detail: String },
    /// The query has no implementation on this platform yet.
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
}

impl From<Error> for devlanes_core::Error {
    /// Keeps I/O and unsupported failures typed and reports other process-table
    /// failures as integration errors.
    fn from(error: Error) -> Self {
        match error {
            Error::Io(error) => Self::Io(error),
            Error::Unsupported(what) => Self::Unsupported(what.to_owned()),
            other => Self::Integration(other.to_string()),
        }
    }
}

/// One process-table entry. `start_time` uses the same units as [`ProcessIdentity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    /// Absent when the platform refused to report the session.
    pub sid: Option<u32>,
    pub uid: u32,
    pub start_time: u64,
    /// Exited but not yet reaped by its parent; signals have no effect.
    pub zombie: bool,
    pub name: String,
}

impl ProcessInfo {
    /// Returns the identity used for PID-reuse-safe signalling; an unknown session is 0.
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            sid: self.sid.unwrap_or(0),
            start_time: self.start_time,
        }
    }
}

/// A TCP socket in the LISTEN state owned by one of the queried processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListeningSocket {
    pub pid: u32,
    pub addr: SocketAddr,
}

/// Result of a start-time-verified signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    Sent,
    /// The process exited or is a zombie.
    Gone,
    /// The PID now belongs to a different process.
    Mismatch,
}

/// Lists every visible process. Entries that exit during enumeration are skipped.
pub fn processes() -> Result<Vec<ProcessInfo>, Error> {
    os::processes()
}

/// Looks up one process, returning `None` once it is fully gone.
///
/// Unreaped zombies are still reported, so a parent can record a child's identity
/// even if it exited immediately.
pub fn process(pid: u32) -> Result<Option<ProcessInfo>, Error> {
    if pid == 0 {
        return Ok(None);
    }
    os::process(pid)
}

/// Returns the platform birth token: clock ticks since boot on Linux, microseconds
/// since the epoch on macOS. Compare tokens only on the machine that produced them.
pub fn start_time(pid: u32) -> Result<Option<u64>, Error> {
    Ok(process(pid)?.map(|info| info.start_time))
}

/// Identifies the current boot: `/proc/sys/kernel/random/boot_id` on Linux,
/// `kern.bootsessionuuid` on macOS. PIDs and start times recorded under another
/// boot name unrelated processes. Compare values for equality only. The first
/// successful read is cached, so later calls do not touch the kernel.
pub fn boot_id() -> Result<String, Error> {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(id) = BOOT.get() {
        return Ok(id.clone());
    }
    let id = os::boot_id()?;
    Ok(BOOT.get_or_init(|| id).clone())
}

/// Captures a process identity for later verification.
pub fn identity(pid: u32) -> Result<Option<ProcessIdentity>, Error> {
    Ok(process(pid)?.map(|info| info.identity()))
}

/// Reports whether the identity still names a live, non-zombie process.
///
/// Only PID and start time are compared: a tracked process may legitimately call
/// `setsid` after the identity was recorded.
pub fn matches(identity: &ProcessIdentity) -> Result<bool, Error> {
    Ok(process(identity.pid)?
        .is_some_and(|info| info.start_time == identity.start_time && !info.zombie))
}

/// Sends `signal` only if the PID still belongs to the recorded process.
///
/// Reconciliation uses this so a recycled PID is never killed. The check and the
/// signal are not atomic, but reuse within that window requires PID wraparound.
#[cfg(unix)]
pub fn signal_verified(identity: &ProcessIdentity, signal: Signal) -> Result<SignalOutcome, Error> {
    let Some(info) = process(identity.pid)? else {
        return Ok(SignalOutcome::Gone);
    };
    if info.start_time != identity.start_time {
        return Ok(SignalOutcome::Mismatch);
    }
    if info.zombie {
        return Ok(SignalOutcome::Gone);
    }
    let Some(pid) = raw_pid(identity.pid) else {
        return Ok(SignalOutcome::Gone);
    };
    match rustix::process::kill_process(pid, signal) {
        Ok(()) => Ok(SignalOutcome::Sent),
        Err(rustix::io::Errno::SRCH) => Ok(SignalOutcome::Gone),
        Err(error) => Err(Error::Io(error.into())),
    }
}

/// Lists TCP listeners owned by `pids`, sorted and deduplicated.
///
/// Diagnostics only (ready-check timeouts); never used to assign ports. On macOS this
/// runs `/usr/sbin/lsof`.
pub fn listening_tcp(pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sockets = os::listening_tcp(pids)?;
    sockets.sort_unstable();
    sockets.dedup();
    Ok(sockets)
}

/// Session ID of `pid`, or `None` once it exited or the kernel refused to say.
#[cfg(unix)]
pub fn session(pid: u32) -> Option<u32> {
    let sid = rustix::process::getsid(Some(raw_pid(pid)?)).ok()?;
    u32::try_from(sid.as_raw_nonzero().get()).ok()
}

/// Real user ID of this process; other users' processes are never a unit's.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// An open file to look for in other processes' descriptor tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFile {
    /// A character device such as a PTY slave, by device number.
    Device(u64),
    /// A pipe: its inode on Linux, the kernel handle of the end passed to
    /// [`open_file`] on macOS. Holders of either end match.
    Pipe(u64),
}

/// Identifies what `fd` refers to, for a later [`holders`] lookup. Only character
/// devices and pipes are supported.
#[cfg(unix)]
pub fn open_file(fd: std::os::fd::BorrowedFd<'_>) -> Result<OpenFile, Error> {
    os::open_file(fd)
}

/// Returns the PIDs among `pids` that hold `target` open.
///
/// Finds processes that keep a unit's PTY or output pipe open after the unit ended,
/// typically daemonized grandchildren that were never seen in a snapshot. Processes
/// that exit or cannot be inspected during the scan are skipped. Blocking; the cost
/// grows with the descriptors of every candidate, so pass a filtered set.
#[cfg(unix)]
pub fn holders(target: OpenFile, pids: &[u32]) -> Result<Vec<u32>, Error> {
    let mut found = Vec::new();
    for &pid in pids {
        if os::holds(pid, target)? {
            found.push(pid);
        }
    }
    Ok(found)
}

/// Converts a kernel PID; 0 and values beyond `pid_t` are not addressable processes.
#[cfg(unix)]
fn raw_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}
