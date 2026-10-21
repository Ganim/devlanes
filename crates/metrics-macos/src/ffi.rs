//! Adapters over the safe macOS wrappers in `devlanes-sys`. Every function returns
//! owned data; `None` means the process exited or is not inspectable by this user.

pub(crate) use devlanes_sys::macos::{
    RusageFlavor, RusageInfo, ShortInfo, Timebase, list_pids, mach_now, short_info,
};

/// Effective uid of the daemon; other users' processes are never ours.
pub(crate) fn current_uid() -> u32 {
    devlanes_sys::current_uid()
}

/// Returns the birth token `devlanes-sys` records in `ProcessIdentity`.
pub(crate) fn start_token(pid: u32) -> Option<u64> {
    devlanes_sys::start_time(pid).ok().flatten()
}

/// Session id of `pid`, or `None` once it exited.
pub(crate) fn session(pid: u32) -> Option<u32> {
    devlanes_sys::session(pid)
}

/// Reads resource usage; `Err(())` only when the flavor itself is unsupported.
pub(crate) fn rusage(pid: u32, flavor: RusageFlavor) -> Result<Option<RusageInfo>, ()> {
    devlanes_sys::macos::rusage(pid, flavor).map_err(|_| ())
}
