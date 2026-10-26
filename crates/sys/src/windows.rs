//! Windows backend over `devlanes-sys-windows`. A start time is the creation
//! `FILETIME` as one `u64`; a zombie is a process that exited while some
//! process still holds a handle to it. Windows has no sessions in the Unix
//! sense, so `sid` is always `None`.

use devlanes_sys_windows::power::{self, Resumed};
use devlanes_sys_windows::proc::{self, ProcessEntry, Terminated};

use crate::{Error, ListeningSocket, ProcessIdentity, ProcessInfo, SignalOutcome, TcpConnection};

/// Every process this user can query; protected processes are left out.
pub(crate) fn processes() -> Result<Vec<ProcessInfo>, Error> {
    Ok(proc::processes()?.into_iter().map(info).collect())
}

/// One process, or `None` when no process has the PID or it refuses queries.
pub(crate) fn process(pid: u32) -> Result<Option<ProcessInfo>, Error> {
    Ok(proc::process(pid)?.map(info))
}

/// The kernel's boot time minus its bias, which changes only with a reboot.
pub(crate) fn boot_id() -> Result<String, Error> {
    Ok(proc::boot_id()?)
}

/// TCP listeners of `pids` from the system's TCP tables.
pub(crate) fn listening_tcp(pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    Ok(proc::listening_tcp(pids)?
        .into_iter()
        .map(|(pid, addr)| ListeningSocket { pid, addr })
        .collect())
}

/// Established TCP connections from the system's TCP tables, both families.
pub(crate) fn tcp_connections() -> Result<Vec<TcpConnection>, Error> {
    Ok(proc::established_tcp()?
        .into_iter()
        .map(|(local, remote)| TcpConnection { local, remote })
        .collect())
}

/// Terminates the recorded process, checking its start time on the same handle.
pub(crate) fn terminate_verified(
    identity: &ProcessIdentity,
    exit_code: u32,
) -> Result<SignalOutcome, Error> {
    Ok(
        match proc::terminate_verified(identity.pid, identity.start_time, exit_code)? {
            Terminated::Sent => SignalOutcome::Sent,
            Terminated::Gone => SignalOutcome::Gone,
            Terminated::Mismatch => SignalOutcome::Mismatch,
        },
    )
}

/// Takes one suspension off the recorded process, checking its start time on
/// the same handle.
pub(crate) fn resume_verified(identity: &ProcessIdentity) -> Result<SignalOutcome, Error> {
    Ok(
        match power::resume_verified(identity.pid, identity.start_time)? {
            Resumed::Sent => SignalOutcome::Sent,
            Resumed::Gone => SignalOutcome::Gone,
            Resumed::Mismatch => SignalOutcome::Mismatch,
        },
    )
}

/// Converts a table entry; Windows reports no session.
fn info(entry: ProcessEntry) -> ProcessInfo {
    ProcessInfo {
        pid: entry.pid,
        ppid: entry.ppid,
        sid: None,
        start_time: entry.start_time,
        zombie: entry.exited,
        name: entry.name,
    }
}
