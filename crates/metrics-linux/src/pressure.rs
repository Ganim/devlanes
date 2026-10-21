//! System pressure for the start gate: CPU from `/proc/stat`, pressure stall
//! information (PSI) from `/proc/pressure/{cpu,memory}` and available memory
//! from `/proc/meminfo`. The parsers are pure functions over text.

use devlanes_core::gate::MemoryPressure;
use devlanes_metrics::Error;

/// `some avg10` at or above this (percent) grades memory pressure as warn.
pub const MEMORY_WARN_SOME_AVG10: f64 = 10.0;
/// `full avg10` at or above this (percent) grades memory pressure as critical.
pub const MEMORY_CRITICAL_FULL_AVG10: f64 = 10.0;

/// Builds a [`Error::Malformed`] for one procfs file.
fn malformed(what: &'static str, detail: impl Into<String>) -> Error {
    Error::Malformed {
        what,
        detail: detail.into(),
    }
}

/// Aggregate CPU time counters of the `cpu` line of `/proc/stat`, in clock ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// Every counter except `guest`/`guest_nice`, which `user`/`nice` already include.
    pub total: u64,
    /// `idle + iowait`.
    pub idle: u64,
}

impl CpuTimes {
    /// Busy share of all CPUs between `earlier` and `self`, 0-100. `None` when
    /// no time passed or the counters went backwards.
    pub fn busy_percent_since(&self, earlier: &Self) -> Option<f64> {
        let total = self.total.checked_sub(earlier.total)?;
        let idle = self.idle.checked_sub(earlier.idle)?;
        if total == 0 || idle > total {
            return None;
        }
        Some((total - idle) as f64 / total as f64 * 100.0)
    }
}

/// Parses the aggregate `cpu` line of `/proc/stat`.
pub fn parse_proc_stat_cpu(text: &str) -> Result<CpuTimes, Error> {
    let what = "/proc/stat";
    let line = text
        .lines()
        .find(|line| line.split_ascii_whitespace().next() == Some("cpu"))
        .ok_or_else(|| malformed(what, "missing cpu line"))?;
    let fields: Vec<u64> = line
        .split_ascii_whitespace()
        .skip(1)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| malformed(what, "bad cpu counter"))?;
    // user nice system idle iowait irq softirq steal: present since Linux 2.6.11.
    if fields.len() < 8 {
        return Err(malformed(what, "fewer than 8 cpu counters"));
    }
    Ok(CpuTimes {
        total: fields[..8]
            .iter()
            .fold(0_u64, |sum, value| sum.saturating_add(*value)),
        idle: fields[3].saturating_add(fields[4]),
    })
}

/// The `avg10` values of a PSI file (`/proc/pressure/<resource>`), in percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    /// Share of time at least one task stalled.
    pub some_avg10: f64,
    /// Share of time all non-idle tasks stalled; `None` for files without a
    /// `full` line (CPU before Linux 5.13).
    pub full_avg10: Option<f64>,
}

impl Psi {
    /// Grades memory PSI with [`MEMORY_WARN_SOME_AVG10`] and
    /// [`MEMORY_CRITICAL_FULL_AVG10`].
    pub fn memory_level(&self) -> MemoryPressure {
        if self
            .full_avg10
            .is_some_and(|full| full >= MEMORY_CRITICAL_FULL_AVG10)
        {
            MemoryPressure::Critical
        } else if self.some_avg10 >= MEMORY_WARN_SOME_AVG10 {
            MemoryPressure::Warn
        } else {
            MemoryPressure::Normal
        }
    }
}

/// Parses `some avg10=... avg60=... avg300=... total=...` and the optional
/// `full` line of a PSI file.
pub fn parse_psi(text: &str) -> Result<Psi, Error> {
    let what = "/proc/pressure";
    let avg10 = |kind: &str| -> Result<Option<f64>, Error> {
        let Some(line) = text
            .lines()
            .find(|line| line.split_ascii_whitespace().next() == Some(kind))
        else {
            return Ok(None);
        };
        line.split_ascii_whitespace()
            .find_map(|field| field.strip_prefix("avg10="))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(Some)
            .ok_or_else(|| malformed(what, format!("bad {kind} avg10")))
    };
    Ok(Psi {
        some_avg10: avg10("some")?.ok_or_else(|| malformed(what, "missing some line"))?,
        full_avg10: avg10("full")?,
    })
}

/// `MemAvailable` of `/proc/meminfo` in bytes.
pub fn parse_meminfo_available(text: &str) -> Result<u64, Error> {
    let what = "/proc/meminfo";
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .ok_or_else(|| malformed(what, "missing MemAvailable"))?;
    let mut parts = line.split_ascii_whitespace();
    let value: u64 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| malformed(what, "bad MemAvailable value"))?;
    match parts.next() {
        Some("kB") => Ok(value.saturating_mul(1024)),
        other => Err(malformed(
            what,
            format!("unexpected MemAvailable unit {other:?}"),
        )),
    }
}

#[cfg(target_os = "linux")]
pub use source::LinuxPressure;

#[cfg(target_os = "linux")]
mod source {
    use std::sync::{Arc, Mutex, PoisonError};

    use async_trait::async_trait;
    use devlanes_core::gate::{PressureSample, PressureSource};

    use super::{CpuTimes, parse_meminfo_available, parse_proc_stat_cpu, parse_psi};

    /// Reads `/proc/stat`, `/proc/pressure/*` and `/proc/meminfo`. Kernels or
    /// containers without PSI report no memory level instead of failing.
    #[derive(Debug, Clone, Default)]
    pub struct LinuxPressure {
        previous: Arc<Mutex<Option<CpuTimes>>>,
    }

    impl LinuxPressure {
        /// A source with no previous CPU reading.
        pub fn new() -> Self {
            Self::default()
        }

        /// Reads every file on the calling thread; blocking but short.
        pub fn sample_blocking(&self) -> Result<PressureSample, devlanes_core::Error> {
            let stat = std::fs::read_to_string("/proc/stat")?;
            let times = parse_proc_stat_cpu(&stat).map_err(integration)?;
            let cpu_percent = {
                let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
                let percent = previous
                    .as_ref()
                    .and_then(|earlier| times.busy_percent_since(earlier));
                *previous = Some(times);
                percent
            };
            let psi = |resource: &str| {
                std::fs::read_to_string(format!("/proc/pressure/{resource}"))
                    .ok()
                    .and_then(|text| parse_psi(&text).ok())
            };
            Ok(PressureSample {
                cpu_percent,
                memory: psi("memory").map(|psi| psi.memory_level()),
                cpu_stall_percent: psi("cpu").map(|psi| psi.some_avg10),
            })
        }
    }

    /// Maps a parser failure into the core error.
    fn integration(error: devlanes_metrics::Error) -> devlanes_core::Error {
        devlanes_core::Error::Integration(error.to_string())
    }

    /// Runs blocking reads off the executor.
    async fn blocking<T: Send + 'static>(
        work: impl FnOnce() -> Result<T, devlanes_core::Error> + Send + 'static,
    ) -> Result<T, devlanes_core::Error> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| devlanes_core::Error::Integration(error.to_string()))?
    }

    #[async_trait]
    impl PressureSource for LinuxPressure {
        /// Returns `linux`.
        fn name(&self) -> &'static str {
            "linux"
        }

        /// CPU since the previous call, PSI memory level and CPU stall share.
        async fn sample(&self) -> Result<PressureSample, devlanes_core::Error> {
            let source = self.clone();
            blocking(move || source.sample_blocking()).await
        }

        /// `MemAvailable`.
        async fn available_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
            blocking(|| {
                let text = std::fs::read_to_string("/proc/meminfo")?;
                parse_meminfo_available(&text)
                    .map(Some)
                    .map_err(integration)
            })
            .await
        }
    }
}
