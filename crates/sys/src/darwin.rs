//! macOS readings for the metrics and pressure collectors: mach time, libproc
//! process info and rusage, host CPU ticks and integer sysctls. Every function
//! returns owned data; `None` means the process exited or is not inspectable.

use std::{
    ffi::{CStr, c_void},
    io,
    mem::MaybeUninit,
};

/// `struct rusage_info_v6` from `<sys/resource.h>`; libc only defines up to v4.
/// Time fields are mach ticks; convert them with [`Timebase::nanos`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RusageInfo {
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
const _: () = assert!(size_of::<RusageInfo>() == 464);

/// Which `proc_pid_rusage` flavor to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RusageFlavor {
    /// macOS 14 and later.
    V6,
    /// A layout prefix of v6 with every field up to `phys_footprint`; older systems.
    V4,
}

impl RusageFlavor {
    /// The `RUSAGE_INFO_*` constant.
    fn raw(self) -> i32 {
        match self {
            Self::V6 => 6,
            Self::V4 => libc::RUSAGE_INFO_V4,
        }
    }
}

/// The running system rejects the requested rusage flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlavorUnsupported;

/// `mach_timebase_info_data_t`, declared here because libc deprecates its fields.
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
    /// `<mach/mach_init.h>`; the host port. libc deprecates its binding.
    fn mach_host_self() -> libc::mach_port_t;
    /// `<mach/mach_init.h>`; this task's port, what the `mach_task_self()` macro
    /// reads. Set before `main` and never changed.
    static mach_task_self_: libc::mach_port_t;
}

/// Mach tick to nanosecond ratio.
#[derive(Debug, Clone, Copy)]
pub struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    /// Reads the ratio; it is fixed for the life of the system (125/3 on Apple
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

/// Current mach absolute time, the clock `RusageInfo::proc_start_abstime` uses.
pub fn mach_now() -> u64 {
    // SAFETY: `mach_absolute_time` has no preconditions.
    unsafe { mach_absolute_time() }
}

/// Lists every PID except the kernel's.
pub fn list_pids() -> io::Result<Vec<u32>> {
    Ok(crate::os::all_pids()?
        .into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .filter(|&pid| pid != 0)
        .collect())
}

/// Fields of `proc_bsdshortinfo`, the cheapest per-process read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortInfo {
    pub ppid: u32,
    pub uid: u32,
    pub zombie: bool,
}

/// Converts a PID for libproc; values beyond `pid_t` cannot name a process.
fn raw(pid: u32) -> Option<libc::c_int> {
    libc::c_int::try_from(pid).ok().filter(|&pid| pid > 0)
}

/// Reads parent, owner and zombie state with `PROC_PIDT_SHORTBSDINFO`.
pub fn short_info(pid: u32) -> Option<ShortInfo> {
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
    // SAFETY: the buffer started zeroed and the kernel filled it; every field is
    // plain integer data, valid for any bit pattern.
    let info = unsafe { info.assume_init() };
    Some(ShortInfo {
        ppid: info.pbsi_ppid,
        uid: info.pbsi_uid,
        zombie: info.pbsi_status == libc::SZOMB,
    })
}

/// Reads resource usage. `Err` only when the flavor itself is unsupported, so
/// callers can fall back from v6 to v4 once; `Ok(None)` when the process is gone.
pub fn rusage(pid: u32, flavor: RusageFlavor) -> Result<Option<RusageInfo>, FlavorUnsupported> {
    let Some(raw_pid) = raw(pid) else {
        return Ok(None);
    };
    let mut info = RusageInfo::default();
    // SAFETY: `info` is a writable `rusage_info_v6`, large enough for v4 and v6;
    // libproc's `rusage_info_t *` parameter is really a pointer to that buffer.
    let status = unsafe {
        libc::proc_pid_rusage(
            raw_pid,
            flavor.raw(),
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    if status == 0 {
        return Ok(Some(info));
    }
    let error = io::Error::last_os_error();
    if flavor == RusageFlavor::V6 && error.raw_os_error() == Some(libc::EINVAL) {
        return Err(FlavorUnsupported);
    }
    Ok(None)
}

/// The host port, looked up once; the send right lives as long as the process.
#[derive(Debug, Clone, Copy)]
pub struct HostPort(libc::mach_port_t);

impl HostPort {
    /// Looks up the host port. Each lookup takes a port reference, so keep one.
    pub fn new() -> Self {
        // SAFETY: `mach_host_self` has no preconditions.
        Self(unsafe { mach_host_self() })
    }

    /// Cumulative CPU ticks per core, each indexed by `libc::CPU_STATE_*` (user,
    /// system, idle, nice), from `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`.
    /// `host_statistics(HOST_CPU_LOAD_INFO)` is rate-limited by the kernel for
    /// non-platform binaries and then returns stale counters. The counters are
    /// 32-bit and wrap.
    pub fn processor_ticks(self) -> io::Result<Vec<[u32; 4]>> {
        let mut cpus: libc::natural_t = 0;
        let mut info: libc::processor_info_array_t = std::ptr::null_mut();
        let mut len: libc::mach_msg_type_number_t = 0;
        // SAFETY: every out-pointer refers to a writable local. On success the
        // kernel maps `len` `integer_t`s at `info` into this task.
        let status = unsafe {
            libc::host_processor_info(
                self.0,
                libc::PROCESSOR_CPU_LOAD_INFO,
                &raw mut cpus,
                &raw mut info,
                &raw mut len,
            )
        };
        if status != libc::KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "host_processor_info failed with {status}"
            )));
        }
        if info.is_null() {
            return Err(io::Error::other("host_processor_info returned no data"));
        }
        let len = usize::try_from(len).unwrap_or(0);
        let states = libc::CPU_STATE_MAX as usize;
        let cpus = usize::try_from(cpus).unwrap_or(0);
        // SAFETY: the kernel returned `len` initialized `integer_t`s at `info`,
        // valid until the `vm_deallocate` below; the slice is not used after it.
        let raw = unsafe { std::slice::from_raw_parts(info, len) };
        let ticks: Vec<[u32; 4]> = raw
            .chunks_exact(states)
            .take(cpus)
            .map(|cpu| {
                [
                    cpu[0].cast_unsigned(),
                    cpu[1].cast_unsigned(),
                    cpu[2].cast_unsigned(),
                    cpu[3].cast_unsigned(),
                ]
            })
            .collect();
        let bytes = len.saturating_mul(size_of::<libc::integer_t>());
        // SAFETY: `mach_task_self_` is initialized by the runtime before `main`
        // and only read here. `info` and `bytes` describe exactly the region the
        // kernel allocated in this task above, and nothing refers to it any more.
        let freed = unsafe {
            libc::vm_deallocate(
                mach_task_self_,
                info as libc::vm_address_t,
                bytes as libc::vm_size_t,
            )
        };
        if freed != libc::KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "vm_deallocate failed with {freed}"
            )));
        }
        if ticks.len() != cpus || cpus == 0 {
            return Err(io::Error::other(format!(
                "host_processor_info returned {len} values for {cpus} CPUs"
            )));
        }
        Ok(ticks)
    }
}

impl Default for HostPort {
    /// Same as [`HostPort::new`].
    fn default() -> Self {
        Self::new()
    }
}

/// Integer types a sysctl may be read into: every bit pattern is valid.
pub trait SysctlInt: Copy + Default + sealed::Sealed {}
impl SysctlInt for i32 {}
impl SysctlInt for u32 {}
impl SysctlInt for i64 {}
impl SysctlInt for u64 {}

mod sealed {
    /// Keeps [`super::SysctlInt`] limited to plain integers.
    pub trait Sealed {}
    impl Sealed for i32 {}
    impl Sealed for u32 {}
    impl Sealed for i64 {}
    impl Sealed for u64 {}
}

/// Reads one integer sysctl by name; a size mismatch is an error.
pub fn sysctl<T: SysctlInt>(name: &CStr) -> io::Result<T> {
    let mut value = T::default();
    let mut size = size_of::<T>();
    // SAFETY: `name` is NUL-terminated; `value` is a writable `T` and `size` is its
    // exact byte length, which the kernel checks before writing. `T` is a plain
    // integer, so any bytes written form a valid value.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast::<c_void>(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if size != size_of::<T>() {
        return Err(io::Error::other(format!(
            "sysctl {} returned {size} bytes",
            name.to_string_lossy()
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live readings for this process and the host are plausible.
    #[test]
    fn live_readings() {
        let pid = std::process::id();
        let short = short_info(pid).expect("own short info");
        assert_eq!(short.uid, crate::current_uid());
        assert!(!short.zombie);
        let usage = rusage(pid, RusageFlavor::V6)
            .or_else(|_| rusage(pid, RusageFlavor::V4))
            .expect("a supported flavor")
            .expect("own usage");
        assert!(usage.phys_footprint > 0);
        let timebase = Timebase::read().expect("timebase");
        assert!(timebase.nanos(mach_now()) > 0);
        assert!(list_pids().expect("pids").contains(&pid));
        let ticks = HostPort::new().processor_ticks().expect("cpu ticks");
        let cpus = sysctl::<i32>(c"hw.ncpu").expect("ncpu");
        assert_eq!(ticks.len(), usize::try_from(cpus).expect("cpu count"));
        assert!(ticks.iter().flatten().any(|&t| t > 0));
        assert!(sysctl::<u64>(c"hw.memsize").expect("memsize") > 0);
        assert!(sysctl::<u32>(c"hw.memsize").is_err());
    }
}
