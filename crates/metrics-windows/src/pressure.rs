//! System pressure for the start gate: CPU from `GetSystemTimes` deltas, and the
//! memory level from Windows' low-memory notification and available physical
//! memory (`GlobalMemoryStatusEx`). Windows has no pressure stall information.

use std::{
    io,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use devlanes_core::gate::{MemoryPressure, PressureSample, PressureSource};
use devlanes_sys::windows::usage::{SystemTimes, low_memory, memory_status, system_times};

/// Share of physical memory, in percent, that available memory must fall under
/// (together with [`WARN_AVAILABLE_BYTES`]) for `Warn`. The memory load is not
/// used: Docker Desktop's VM and Windows' cache keep it above 85% on ordinary
/// dev machines, which would halve the adaptive start window for no reason.
pub(crate) const WARN_AVAILABLE_PERCENT: u64 = 10;
/// Available memory under which `Warn` applies once it is also under
/// [`WARN_AVAILABLE_PERCENT`]: about what one or two dev servers take while
/// starting, so on large machines a few free percent still count as room.
pub(crate) const WARN_AVAILABLE_BYTES: u64 = 2 << 30;

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

/// Grades memory: `Critical` while Windows signals low memory, `Warn` when
/// available physical memory is under both [`WARN_AVAILABLE_PERCENT`] of `total`
/// and [`WARN_AVAILABLE_BYTES`], else `Normal`.
pub(crate) fn memory_level(low_memory: bool, available: u64, total: u64) -> MemoryPressure {
    if low_memory {
        return MemoryPressure::Critical;
    }
    let share_floor = u128::from(total) * u128::from(WARN_AVAILABLE_PERCENT) / 100;
    if u128::from(available) < share_floor && available < WARN_AVAILABLE_BYTES {
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

    /// Reads synchronously; three system calls.
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
        let low = low_memory()?;
        Ok(PressureSample {
            cpu_percent,
            memory: Some(memory_level(
                low,
                memory.available_physical,
                memory.total_physical,
            )),
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

    /// CPU since the previous call and the graded memory level.
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

    const GIB: u64 = 1 << 30;

    #[test]
    fn low_memory_notification_is_critical() {
        assert_eq!(memory_level(true, 0, 16 * GIB), MemoryPressure::Critical);
        // Windows' signal wins even with room by the other measures.
        assert_eq!(
            memory_level(true, 8 * GIB, 16 * GIB),
            MemoryPressure::Critical
        );
    }

    #[test]
    fn large_machine_warns_under_the_byte_floor() {
        // 64 GiB: 10% is 6.4 GiB, so the 2 GiB floor decides.
        let total = 64 * GIB;
        assert_eq!(memory_level(false, 3 * GIB, total), MemoryPressure::Normal);
        assert_eq!(memory_level(false, 2 * GIB, total), MemoryPressure::Normal);
        assert_eq!(
            memory_level(false, 2 * GIB - 1, total),
            MemoryPressure::Warn
        );
    }

    #[test]
    fn small_machine_warns_under_the_share() {
        // 8 GiB: 10% is 819.2 MiB, under the 2 GiB floor, so the share decides.
        let total = 8 * GIB;
        assert_eq!(memory_level(false, GIB, total), MemoryPressure::Normal);
        assert_eq!(
            memory_level(false, 820 << 20, total),
            MemoryPressure::Normal
        );
        assert_eq!(memory_level(false, 819 << 20, total), MemoryPressure::Warn);
        assert_eq!(memory_level(false, 0, total), MemoryPressure::Warn);
    }

    #[test]
    fn docker_desktop_machine_with_high_load_is_normal() {
        // 32 GiB with 88% in use (the old warn grade): 3.8 GiB available.
        let total = 32 * GIB;
        let available = total / 100 * 12;
        assert_eq!(
            memory_level(false, available, total),
            MemoryPressure::Normal
        );
        assert_eq!(memory_level(false, 0, 0), MemoryPressure::Normal);
    }
}
