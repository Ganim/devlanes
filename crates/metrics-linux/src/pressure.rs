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
    /// Busy share of all CPUs between `earlier` and `self`, 0-100. `None` only
    /// when no time passed. The kernel's per-CPU `iowait` can go backwards
    /// between reads, so the idle delta is clamped to `0..=total` rather than
    /// turning the reading into `None` (which would freeze an adaptive window).
    pub fn busy_percent_since(&self, earlier: &Self) -> Option<f64> {
        let total = self.total.saturating_sub(earlier.total);
        if total == 0 {
            return None;
        }
        let idle = self.idle.saturating_sub(earlier.idle).min(total);
        Some((total - idle) as f64 / total as f64 * 100.0)
    }
}

/// Parses the aggregate `cpu` line of `/proc/stat`.
pub fn parse_proc_stat_cpu(text: &str) -> Result<CpuTimes, Error> {
    let line = text
        .lines()
        .find(|line| line.split_ascii_whitespace().next() == Some("cpu"))
        .ok_or_else(|| malformed("/proc/stat", "missing cpu line"))?;
    cpu_times(line)
}

/// Parses the per-CPU `cpu0`, `cpu1`, ... lines of `/proc/stat`, in order.
pub fn parse_proc_stat_cores(text: &str) -> Result<Vec<CpuTimes>, Error> {
    text.lines()
        .filter(|line| {
            line.split_ascii_whitespace()
                .next()
                .and_then(|word| word.strip_prefix("cpu"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(cpu_times)
        .collect()
}

/// The counters of one `cpu…` line.
fn cpu_times(line: &str) -> Result<CpuTimes, Error> {
    let what = "/proc/stat";
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
    parse_meminfo(text, "MemAvailable")
}

/// `MemTotal` of `/proc/meminfo` in bytes.
pub fn parse_meminfo_total(text: &str) -> Result<u64, Error> {
    parse_meminfo(text, "MemTotal")
}

/// One `kB` field of `/proc/meminfo` in bytes.
fn parse_meminfo(text: &str, key: &str) -> Result<u64, Error> {
    let what = "/proc/meminfo";
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .ok_or_else(|| malformed(what, format!("missing {key}")))?;
    let mut parts = line.split_ascii_whitespace();
    let value: u64 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| malformed(what, format!("bad {key} value")))?;
    match parts.next() {
        Some("kB") => Ok(value.saturating_mul(1024)),
        other => Err(malformed(what, format!("unexpected {key} unit {other:?}"))),
    }
}

/// The cgroup v2 path of a process from `/proc/<pid>/cgroup` (the `0::` line),
/// relative to the cgroup mount; `None` on cgroup v1-only systems.
pub fn parse_cgroup_v2_path(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().trim_start_matches('/').to_owned())
}

/// A cgroup v2 `memory.max`: the limit in bytes, `None` for `max` (no limit)
/// or anything unparsable.
pub fn parse_memory_max(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Memory still available under a cgroup limit: `max - current`, never above
/// the system's `available`.
pub fn cgroup_available(available: u64, max: Option<u64>, current: Option<u64>) -> u64 {
    match (max, current) {
        (Some(max), Some(current)) => available.min(max.saturating_sub(current)),
        _ => available,
    }
}

#[cfg(target_os = "linux")]
pub use source::LinuxPressure;

#[cfg(target_os = "linux")]
mod source {
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex, PoisonError,
            atomic::{AtomicBool, Ordering},
        },
    };

    use async_trait::async_trait;
    use devlanes_core::gate::{PressureSample, PressureSource};

    use super::{
        CpuTimes, cgroup_available, parse_cgroup_v2_path, parse_meminfo_available,
        parse_memory_max, parse_proc_stat_cores, parse_proc_stat_cpu, parse_psi,
    };

    /// Reads `/proc/stat`, PSI and available memory. When the daemon runs in a
    /// cgroup v2 group (containers, systemd units), its `memory.pressure` and
    /// `memory.max - memory.current` take precedence: services started by the
    /// daemon live under the same limit. Without PSI there is no memory level;
    /// that is logged once.
    #[derive(Debug, Clone, Default)]
    pub struct LinuxPressure {
        previous: Arc<Mutex<Option<CpuTimes>>>,
        /// Per-CPU counters of the last sample, and each CPU's share since
        /// the one before.
        cores: Arc<Mutex<(Vec<CpuTimes>, Vec<f64>)>>,
        /// The daemon's cgroup directory, looked up once.
        cgroup: Option<PathBuf>,
        psi_missing_logged: Arc<AtomicBool>,
    }

    impl LinuxPressure {
        /// A source with no previous CPU reading, bound to this process's cgroup.
        pub fn new() -> Self {
            let cgroup = std::fs::read_to_string("/proc/self/cgroup")
                .ok()
                .and_then(|text| parse_cgroup_v2_path(&text))
                .map(|path| PathBuf::from("/sys/fs/cgroup").join(path))
                .filter(|path| path.join("memory.current").exists());
            Self {
                cgroup,
                ..Self::default()
            }
        }

        /// Reads one file of the daemon's cgroup.
        fn cgroup_file(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.cgroup.as_ref()?.join(name)).ok()
        }

        /// Memory pressure: the cgroup's PSI when present, else the system's.
        fn memory_psi(&self) -> Option<super::Psi> {
            self.cgroup_file("memory.pressure")
                .or_else(|| std::fs::read_to_string("/proc/pressure/memory").ok())
                .and_then(|text| parse_psi(&text).ok())
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
            if let Ok(times) = parse_proc_stat_cores(&stat) {
                let mut cores = self.cores.lock().unwrap_or_else(PoisonError::into_inner);
                cores.1 = if cores.0.len() == times.len() {
                    times
                        .iter()
                        .zip(&cores.0)
                        .map(|(later, earlier)| later.busy_percent_since(earlier).unwrap_or(0.0))
                        .collect()
                } else {
                    Vec::new()
                };
                cores.0 = times;
            }
            let memory = self.memory_psi();
            if memory.is_none() && !self.psi_missing_logged.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "no memory PSI (/proc/pressure/memory): start pacing sees no memory pressure level"
                );
            }
            let cpu = std::fs::read_to_string("/proc/pressure/cpu")
                .ok()
                .and_then(|text| parse_psi(&text).ok());
            Ok(PressureSample {
                cpu_percent,
                memory: memory.map(|psi| psi.memory_level()),
                cpu_stall_percent: cpu.map(|psi| psi.some_avg10),
            })
        }

        /// `MemAvailable`, lowered to the room left under the cgroup's limit.
        pub fn available_memory_blocking(&self) -> Result<Option<u64>, devlanes_core::Error> {
            let text = std::fs::read_to_string("/proc/meminfo")?;
            let available = parse_meminfo_available(&text).map_err(integration)?;
            let max = self
                .cgroup_file("memory.max")
                .and_then(|text| parse_memory_max(&text));
            let current = self
                .cgroup_file("memory.current")
                .and_then(|text| text.trim().parse().ok());
            Ok(Some(cgroup_available(available, max, current)))
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

        /// `MemAvailable`, or less under a cgroup memory limit.
        async fn available_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
            let source = self.clone();
            blocking(move || source.available_memory_blocking()).await
        }

        /// `MemTotal`.
        async fn total_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
            blocking(|| {
                let text = std::fs::read_to_string("/proc/meminfo")?;
                super::parse_meminfo_total(&text)
                    .map(Some)
                    .map_err(integration)
            })
            .await
        }

        /// Each CPU's share from the `/proc/stat` the last sample read.
        fn core_percents(&self) -> Vec<f64> {
            self.cores
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .1
                .clone()
        }
    }
}
