//! Thin libproc/mach wrappers. Every function returns owned data; `None` means the
//! process exited or is not inspectable by this user.

use std::{ffi::c_void, io, mem::MaybeUninit};

/// `RUSAGE_INFO_V6` (macOS 14+). libc only defines up to v4.
const RUSAGE_INFO_V6: i32 = 6;
/// v4 is a layout prefix of v6, so it can fill the same buffer on older systems.
pub(crate) const RUSAGE_INFO_V4: i32 = libc::RUSAGE_INFO_V4;

/// `struct rusage_info_v6` from `<sys/resource.h>`. Time fields are mach ticks.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RusageInfoV6 {
    pub uuid: [u8; 16],
    pub user_time: u64,
    pub system_time: u64,
    pub pkg_idle_wkups: u64,
    pub interrupt_wkups: u64,
    pub pageins: u64,
    pub wired_size: u64,
    pub resident_size: u64,
    pub phys_footprint: u64,
    pub proc_start_abstime: u64,
    pub proc_exit_abstime: u64,
    pub child_user_time: u64,
    pub child_system_time: u64,
    pub child_pkg_idle_wkups: u64,
    pub child_interrupt_wkups: u64,
    pub child_pageins: u64,
    pub child_elapsed_abstime: u64,
    pub diskio_bytesread: u64,
    pub diskio_byteswritten: u64,
    pub cpu_time_qos: [u64; 7],
    pub billed_system_time: u64,
    pub serviced_system_time: u64,
    pub logical_writes: u64,
    pub lifetime_max_phys_footprint: u64,
    pub instructions: u64,
    pub cycles: u64,
    pub billed_energy: u64,
    pub serviced_energy: u64,
    pub interval_max_phys_footprint: u64,
    pub runnable_time: u64,
    pub flags: u64,
    pub user_ptime: u64,
    pub system_ptime: u64,
    pub pinstructions: u64,
    pub pcycles: u64,
    pub energy_nj: u64,
    pub penergy_nj: u64,
    pub secure_time_in_system: u64,
    pub secure_ptime_in_system: u64,
    pub neural_footprint: u64,
    pub lifetime_max_neural_footprint: u64,
    pub interval_max_neural_footprint: u64,
    pub reserved: [u64; 9],
}

// `sizeof(struct rusage_info_v6)` in the macOS 26 SDK: 16 uuid bytes + 56 u64 fields.
const _: () = assert!(size_of::<RusageInfoV6>() == 464);

/// `mach_timebase_info_data_t`, defined here because libc deprecates its fields.
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    /// `<mach/mach_time.h>`; fills the tick-to-nanosecond ratio.
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    /// `<mach/mach_time.h>`; ticks since boot, excluding sleep.
    fn mach_absolute_time() -> u64;
}

/// Fields of `proc_bsdshortinfo` the collector uses.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShortInfo {
    pub ppid: u32,
    pub uid: u32,
    pub zombie: bool,
}

/// Mach tick to nanosecond ratio.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    /// Reads the ratio once; it is fixed for the life of the system (125/3 on Apple
    /// Silicon, 1/1 on Intel).
    pub fn read() -> io::Result<Self> {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: `info` matches the C layout and is writable for the call.
        let status = unsafe { mach_timebase_info(&raw mut info) };
        if status != 0 || info.denom == 0 {
            return Err(io::Error::other(format!(
                "mach_timebase_info failed with status {status}"
            )));
        }
        Ok(Self {
            numer: info.numer,
            denom: info.denom,
        })
    }

    /// Converts mach ticks to nanoseconds without intermediate overflow.
    pub fn nanos(self, ticks: u64) -> u64 {
        let nanos = u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom);
        u64::try_from(nanos).unwrap_or(u64::MAX)
    }
}

/// Current mach absolute time, the clock `ri_proc_start_abstime` uses.
pub(crate) fn mach_now() -> u64 {
    // SAFETY: `mach_absolute_time` has no preconditions.
    unsafe { mach_absolute_time() }
}

/// Effective uid of the daemon; other users' processes are never ours.
pub(crate) fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Lists every PID. The table can grow between the sizing and filling calls, so a
/// full buffer is retried with more room.
pub(crate) fn list_pids() -> io::Result<Vec<u32>> {
    // SAFETY: a null buffer with size 0 asks only for the current PID count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut capacity = usize::try_from(count).unwrap_or(0) + 64;
    loop {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let bytes = i32::try_from(capacity * size_of::<libc::pid_t>())
            .map_err(|_| io::Error::other("PID buffer exceeds i32"))?;
        // SAFETY: `pids` owns `bytes` writable bytes of `pid_t` slots.
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
        if filled < 0 {
            return Err(io::Error::last_os_error());
        }
        let filled = usize::try_from(filled).unwrap_or(0);
        if filled < capacity {
            pids.truncate(filled);
            return Ok(pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok())
                .filter(|&pid| pid != 0)
                .collect());
        }
        capacity *= 2;
    }
}

/// Converts a PID for libproc; values beyond `pid_t` cannot name a process.
fn raw(pid: u32) -> Option<libc::c_int> {
    libc::c_int::try_from(pid).ok().filter(|&pid| pid > 0)
}

/// Reads parent, owner and zombie state with the cheap `PROC_PIDT_SHORTBSDINFO`.
pub(crate) fn short_info(pid: u32) -> Option<ShortInfo> {
    let pid = raw(pid)?;
    let size = i32::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    // SAFETY: the buffer is `size` writable bytes for one `proc_bsdshortinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the kernel filled the whole struct (`written == size`), and every
    // field is plain integer data, so the zeroed-then-written value is initialized.
    let info = unsafe { info.assume_init() };
    Some(ShortInfo {
        ppid: info.pbsi_ppid,
        uid: info.pbsi_uid,
        zombie: info.pbsi_status == libc::SZOMB,
    })
}

/// Returns the birth token `devlanes-sys` records in `ProcessIdentity`:
/// microseconds since the Unix epoch from `PROC_PIDTBSDINFO`.
pub(crate) fn start_token(pid: u32) -> Option<u64> {
    let pid = raw(pid)?;
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: the buffer is `size` writable bytes for one `proc_bsdinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: fully written by the kernel as checked above; integer/char fields only.
    let info = unsafe { info.assume_init() };
    info.pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)
}

/// Session id of `pid`, or `None` once it exited.
pub(crate) fn session(pid: u32) -> Option<u32> {
    let pid = raw(pid)?;
    // SAFETY: `getsid` takes a plain PID and reports failure through its result.
    let sid = unsafe { libc::getsid(pid) };
    u32::try_from(sid).ok()
}

/// Reads resource usage with `flavor` (v6, or v4 on systems that reject v6).
/// Returns `Err(())` only when the flavor itself is unsupported.
pub(crate) fn rusage(pid: u32, flavor: i32) -> Result<Option<RusageInfoV6>, ()> {
    let Some(raw_pid) = raw(pid) else {
        return Ok(None);
    };
    let mut info = RusageInfoV6::default();
    // SAFETY: `info` is a writable `rusage_info_v6`, large enough for v4 and v6;
    // libproc's `rusage_info_t *` parameter is really a pointer to that buffer.
    let status = unsafe {
        libc::proc_pid_rusage(
            raw_pid,
            flavor,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    if status == 0 {
        return Ok(Some(info));
    }
    let error = io::Error::last_os_error();
    if flavor == RUSAGE_INFO_V6 && error.raw_os_error() == Some(libc::EINVAL) {
        return Err(());
    }
    Ok(None)
}

/// Preferred rusage flavor; [`rusage`] tells callers when to fall back to v4.
pub(crate) const PREFERRED_RUSAGE: i32 = RUSAGE_INFO_V6;
