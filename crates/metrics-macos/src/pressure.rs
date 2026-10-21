//! System pressure for the start gate: CPU from `host_statistics`
//! (`HOST_CPU_LOAD_INFO`), the kernel's memory pressure level from
//! `kern.memorystatus_vm_pressure_level`, and available memory from
//! `kern.memorystatus_level` (percent of RAM free) times `hw.memsize`.

use std::{
    io,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use devlanes_core::gate::{MemoryPressure, PressureSample, PressureSource};
use devlanes_sys::macos::{HostPort, sysctl};

/// Cumulative CPU ticks of all cores: user, system, idle, nice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CpuTicks([u32; 4]);

impl CpuTicks {
    /// Busy share of all CPUs between `earlier` and `self`, 0-100. The kernel
    /// counters are 32-bit and wrap, so deltas use wrapping subtraction.
    pub(crate) fn busy_percent_since(&self, earlier: &Self) -> Option<f64> {
        let delta = |index: usize| u64::from(self.0[index].wrapping_sub(earlier.0[index]));
        let user = delta(libc::CPU_STATE_USER as usize);
        let system = delta(libc::CPU_STATE_SYSTEM as usize);
        let idle = delta(libc::CPU_STATE_IDLE as usize);
        let nice = delta(libc::CPU_STATE_NICE as usize);
        let total = user + system + idle + nice;
        (total > 0).then(|| (user + system + nice) as f64 / total as f64 * 100.0)
    }
}

/// Maps `kern.memorystatus_vm_pressure_level` (the `DISPATCH_MEMORYPRESSURE_*`
/// values 1, 2 and 4) to a level; unknown values are `None`.
pub(crate) fn memory_level(raw: i32) -> Option<MemoryPressure> {
    match raw {
        1 => Some(MemoryPressure::Normal),
        2 => Some(MemoryPressure::Warn),
        4 => Some(MemoryPressure::Critical),
        _ => None,
    }
}

/// Bytes the kernel considers free: `memorystatus_level` percent of RAM.
pub(crate) fn available_bytes(memsize: u64, level_percent: i32) -> Option<u64> {
    let percent = u64::try_from(level_percent).ok().filter(|p| *p <= 100)?;
    Some(u64::try_from(u128::from(memsize) * u128::from(percent) / 100).unwrap_or(u64::MAX))
}

/// Reads CPU load, the memory pressure level and available memory. Each call's
/// CPU covers the time since the previous call; the first call has none.
#[derive(Debug, Clone)]
pub struct MacosPressure {
    host: HostPort,
    previous: Arc<Mutex<Option<CpuTicks>>>,
}

impl MacosPressure {
    /// Looks up the host port once; the send right lives as long as the daemon.
    pub fn new() -> Self {
        Self {
            host: HostPort::new(),
            previous: Arc::new(Mutex::new(None)),
        }
    }

    /// Reads synchronously; a few microseconds of syscalls.
    pub fn sample_blocking(&self) -> io::Result<PressureSample> {
        let ticks = CpuTicks(self.host.cpu_ticks()?);
        let cpu_percent = {
            let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
            let percent = previous
                .as_ref()
                .and_then(|earlier| ticks.busy_percent_since(earlier));
            *previous = Some(ticks);
            percent
        };
        let memory = sysctl::<i32>(c"kern.memorystatus_vm_pressure_level")
            .ok()
            .and_then(memory_level);
        Ok(PressureSample {
            cpu_percent,
            memory,
            cpu_stall_percent: None,
        })
    }

    /// Available memory, synchronously.
    pub fn available_memory_blocking(&self) -> io::Result<Option<u64>> {
        let memsize = sysctl::<u64>(c"hw.memsize")?;
        let level = sysctl::<i32>(c"kern.memorystatus_level")?;
        Ok(available_bytes(memsize, level))
    }
}

impl Default for MacosPressure {
    /// Same as [`MacosPressure::new`].
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PressureSource for MacosPressure {
    /// Returns `macos`.
    fn name(&self) -> &'static str {
        "macos"
    }

    /// CPU since the previous call and the kernel memory pressure level.
    async fn sample(&self) -> Result<PressureSample, devlanes_core::Error> {
        let source = self.clone();
        tokio::task::spawn_blocking(move || source.sample_blocking())
            .await
            .map_err(|error| devlanes_core::Error::Integration(error.to_string()))?
            .map_err(devlanes_core::Error::Io)
    }

    /// `memorystatus_level` percent of `hw.memsize`.
    async fn available_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
        let source = self.clone();
        tokio::task::spawn_blocking(move || source.available_memory_blocking())
            .await
            .map_err(|error| devlanes_core::Error::Integration(error.to_string()))?
            .map_err(devlanes_core::Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_share_handles_wrapping_counters() {
        let earlier = CpuTicks([u32::MAX - 9, 100, 1_000, 0]);
        let later = CpuTicks([10, 120, 1_060, 0]);
        // user 20 (across the wrap), system 20, idle 60.
        let busy = later.busy_percent_since(&earlier).expect("time passed");
        assert!((busy - 40.0).abs() < 1e-9, "{busy}");
        assert_eq!(later.busy_percent_since(&later), None);
    }

    #[test]
    fn pressure_levels_and_available_memory() {
        assert_eq!(memory_level(1), Some(MemoryPressure::Normal));
        assert_eq!(memory_level(2), Some(MemoryPressure::Warn));
        assert_eq!(memory_level(4), Some(MemoryPressure::Critical));
        assert_eq!(memory_level(0), None);
        assert_eq!(available_bytes(16 << 30, 25), Some(4 << 30));
        assert_eq!(available_bytes(16 << 30, 101), None);
        assert_eq!(available_bytes(16 << 30, -1), None);
    }

    #[test]
    fn live_readings_are_in_range() {
        let source = MacosPressure::new();
        let first = source.sample_blocking().expect("first reading");
        assert_eq!(first.cpu_percent, None);
        assert!(first.memory.is_some(), "pressure level sysctl");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let second = source.sample_blocking().expect("second reading");
        assert!(
            second
                .cpu_percent
                .is_some_and(|cpu| (0.0..=100.0).contains(&cpu))
        );
        let available = source
            .available_memory_blocking()
            .expect("sysctls")
            .expect("level in range");
        assert!(available > 0);
    }
}
