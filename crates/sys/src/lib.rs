//! Operating-system process queries for supervision, reconciliation and diagnostics.
//!
//! Platform code is selected at compile time: procfs on Linux, libproc and `lsof`
//! on macOS. Every query is blocking; async callers should use `spawn_blocking`.
//!
//! ```
//! let me = devlanes_sys::identity(std::process::id())?.expect("own process is visible");
//! assert!(devlanes_sys::matches(&me)?);
//! # Ok::<(), devlanes_sys::Error>(())
//! ```

// Unsafe code is limited to the macOS FFI module; each block documents its invariants.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::net::SocketAddr;

pub use devlanes_core::ProcessIdentity;
pub use rustix::process::Signal;

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

pub use rlimit::{NofileLimit, child_nofile_limit, raise_nofile_limit};
pub use tree::{DescendantTracker, session_members};

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
}

impl From<Error> for devlanes_core::Error {
    /// Keeps I/O failures typed and reports other process-table failures as integration errors.
    fn from(error: Error) -> Self {
        match error {
            Error::Io(error) => Self::Io(error),
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

/// Converts a kernel PID; 0 and values beyond `pid_t` are not addressable processes.
fn raw_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}
