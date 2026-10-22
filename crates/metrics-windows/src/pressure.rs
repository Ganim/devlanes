//! System pressure for the start gate: CPU from `GetSystemTimes` deltas, the
//! memory level from `GlobalMemoryStatusEx`'s memory load, and available memory
//! from its available physical memory. Windows has no pressure stall information,
//! so the level is graded from the load.

use std::{
    io,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use devlanes_core::gate::{MemoryPressure, PressureSample, PressureSource};
use devlanes_sys::windows::usage::{SystemTimes, memory_status, system_times};

/// Memory load (percent of physical memory in use) graded as `Warn`. Windows
/// starts trimming working sets and paging well before memory runs out.
pub(crate) const WARN_LOAD: u32 = 85;
/// Memory load graded as `Critical`: new processes are mostly paged in from disk.
pub(crate) const CRITICAL_LOAD: u32 = 95;

/// Busy share of all CPUs between `earlier` and `later`, 0-100. `None` when no
/// time passed or a counter went backwards.
pub(crate) fn busy_percent(earlier: &SystemTimes, later: &SystemTimes) -> Option<f64> {
    let idle = later.idle.checked_sub(earlier.idle)?;
    // Kernel time includes idle time, so kernel plus user is the total.
    let total = later
        .kernel
        .checked_sub(earlier.kernel)?
        .checked_add(later.user.checked_sub(earlier.user)?)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((total - idle) as f64 / total as f64 * 100.0)
}

/// Grades `GlobalMemoryStatusEx`'s `dwMemoryLoad`.
pub(crate) fn memory_level(load_percent: u32) -> MemoryPressure {
    if load_percent >= CRITICAL_LOAD {
        MemoryPressure::Critical
    } else if load_percent >= WARN_LOAD {
        MemoryPressure::Warn
    } else {
        MemoryPressure::Normal
    }
}

/// Reads system CPU load, the memory level and available memory. Each call's
/// CPU covers the time since the previous call; the first call has none.
#[derive(Debug, Clone, Default)]
pub struct WindowsPressure {
    previous: Arc<Mutex<Option<SystemTimes>>>,
}

impl WindowsPressure {
    /// Creates a source with no previous CPU reading.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads synchronously; two system calls.
    pub fn sample_blocking(&self) -> io::Result<PressureSample> {
        let times = system_times()?;
        let cpu_percent = {
            let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
            let percent = previous
                .as_ref()
                .and_then(|earlier| busy_percent(earlier, &times));
            *previous = Some(times);
            percent
        };
        let memory = memory_status()?;
        Ok(PressureSample {
            cpu_percent,
            memory: Some(memory_level(memory.load_percent)),
            cpu_stall_percent: None,
        })
    }

    /// Available physical memory (free, zeroed and standby pages), synchronously.
    pub fn available_memory_blocking(&self) -> io::Result<Option<u64>> {
        Ok(Some(memory_status()?.available_physical))
    }
}

#[async_trait]
impl PressureSource for WindowsPressure {
    /// Returns `windows`.
    fn name(&self) -> &'static str {
        "windows"
    }

    /// CPU since the previous call and the graded memory load.
    async fn sample(&self) -> Result<PressureSample, devlanes_core::Error> {
        let source = self.clone();
        tokio::task::spawn_blocking(move || source.sample_blocking())
            .await
            .map_err(|error| devlanes_core::Error::Integration(error.to_string()))?
            .map_err(devlanes_core::Error::Io)
    }

    /// Available physical memory.
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

    /// Readings in 100 ns units.
    fn times(idle: u64, kernel: u64, user: u64) -> SystemTimes {
        SystemTimes { idle, kernel, user }
    }

    #[test]
    fn busy_share_counts_kernel_without_idle() {
        let earlier = times(1_000, 2_000, 500);
        // 60 idle, 80 kernel (idle included), 20 user: 40 busy of 100.
        let later = times(1_060, 2_080, 520);
        let busy = busy_percent(&earlier, &later).expect("time passed");
        assert!((busy - 40.0).abs() < 1e-9, "{busy}");
        assert_eq!(busy_percent(&later, &later), None);
    }

    #[test]
    fn backwards_or_inconsistent_counters_are_no_reading() {
        let earlier = times(1_000, 2_000, 500);
        assert_eq!(busy_percent(&earlier, &times(990, 2_100, 600)), None);
        assert_eq!(busy_percent(&earlier, &times(1_100, 1_990, 600)), None);
        // More idle than kernel plus user time cannot happen.
        assert_eq!(busy_percent(&earlier, &times(1_200, 2_050, 510)), None);
    }

    #[test]
    fn memory_load_grades() {
        assert_eq!(memory_level(0), MemoryPressure::Normal);
        assert_eq!(memory_level(84), MemoryPressure::Normal);
        assert_eq!(memory_level(85), MemoryPressure::Warn);
        assert_eq!(memory_level(94), MemoryPressure::Warn);
        assert_eq!(memory_level(95), MemoryPressure::Critical);
        assert_eq!(memory_level(100), MemoryPressure::Critical);
    }
}
