//! System pressure for the start gate: CPU from `host_processor_info`
//! (`PROCESSOR_CPU_LOAD_INFO`, summed over cores), the kernel's memory pressure level from
//! `kern.memorystatus_vm_pressure_level`, and available memory from
//! `kern.memorystatus_level` (percent of RAM free) times `hw.memsize`.

use std::{
    io,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use devlanes_core::gate::{MemoryPressure, PressureSample, PressureSource};
use devlanes_sys::macos::{HostPort, sysctl};

/// A counter this close to `u32::MAX` that reads small afterwards wrapped; any
/// other decrease is a bad reading.
const WRAP_SLACK: u32 = 1 << 24;

/// Cumulative CPU ticks per core: user, system, idle, nice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CpuTicks(Vec<[u32; 4]>);

/// Ticks between two readings of one counter; `None` when it went backwards
/// without wrapping.
fn tick_delta(earlier: u32, later: u32) -> Option<u64> {
    if later >= earlier {
        Some(u64::from(later - earlier))
    } else if earlier > u32::MAX - WRAP_SLACK && later < WRAP_SLACK {
        Some(u64::from(later.wrapping_sub(earlier)))
    } else {
        None
    }
}

impl CpuTicks {
    /// Busy share of all CPUs between `earlier` and `self`, 0-100. `None` when
    /// no time passed, the CPU count changed or a counter went backwards.
    pub(crate) fn busy_percent_since(&self, earlier: &Self) -> Option<f64> {
        if self.0.len() != earlier.0.len() {
            return None;
        }
        let (mut busy, mut total) = (0_u64, 0_u64);
        for (later, earlier) in self.0.iter().zip(&earlier.0) {
            for state in 0..4 {
                let delta = tick_delta(earlier[state], later[state])?;
                total += delta;
                if state != libc::CPU_STATE_IDLE as usize {
                    busy += delta;
                }
            }
        }
        (total > 0).then(|| busy as f64 / total as f64 * 100.0)
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
        let ticks = CpuTicks(self.host.processor_ticks()?);
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

    /// `hw.memsize`; one sysctl, cheap enough inline.
    async fn total_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
        sysctl::<u64>(c"hw.memsize")
            .map(Some)
            .map_err(devlanes_core::Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_share_sums_cores_and_handles_wrapping_counters() {
        let earlier = CpuTicks(vec![[u32::MAX - 9, 100, 1_000, 0], [0, 0, 0, 0]]);
        let later = CpuTicks(vec![[10, 120, 1_060, 0], [5, 5, 90, 0]]);
        // Core 0: user 20 (across the wrap), system 20, idle 60.
        // Core 1: user 5, system 5, idle 90. Together 50 busy of 200.
        let busy = later.busy_percent_since(&earlier).expect("time passed");
        assert!((busy - 25.0).abs() < 1e-9, "{busy}");
        assert_eq!(later.busy_percent_since(&later), None);
    }

    #[test]
    fn backwards_counters_are_no_reading() {
        let earlier = CpuTicks(vec![[1_000, 100, 5_000, 0]]);
        let stale = CpuTicks(vec![[990, 100, 5_100, 0]]);
        assert_eq!(stale.busy_percent_since(&earlier), None);
        let fewer_cores = CpuTicks(vec![[2_000, 100, 5_000, 0], [0; 4]]);
        assert_eq!(fewer_cores.busy_percent_since(&earlier), None);
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
        // A core can report no new ticks over a short interval, so wait (bounded)
        // for a reading instead of expecting the second one to have it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let cpu = loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let reading = source.sample_blocking().expect("reading");
            if let Some(cpu) = reading.cpu_percent {
                break cpu;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no CPU reading in 5 s"
            );
        };
        assert!((0.0..=100.0).contains(&cpu), "{cpu}");
        let available = source
            .available_memory_blocking()
            .expect("sysctls")
            .expect("level in range");
        assert!(available > 0);
    }

    #[tokio::test]
    /// Total memory is the machine's and at least what is available.
    async fn total_memory_is_reported() {
        let source = MacosPressure::new();
        let total = source.total_memory().await.expect("sysctl").expect("total");
        let available = source
            .available_memory()
            .await
            .expect("sysctls")
            .expect("level in range");
        assert!(total >= available && total > 0, "{total} {available}");
    }
}
