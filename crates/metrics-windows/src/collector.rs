use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use devlanes_core::ProcessIdentity;
use devlanes_metrics::{Error, MetricsSource, Sample, SampleRequest, Subject, UnitTarget, unix_ms};
use devlanes_sys::windows::usage::{ProcessUsage, process_usage};

/// 100 ns ticks between 1601-01-01 (the `FILETIME` epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 116_444_736_000_000_000;

/// Previous CPU reading of one process, keyed by (pid, creation time).
#[derive(Debug, Clone, Copy)]
struct CpuReading {
    /// Kernel plus user time in 100 ns units.
    cpu_time: u64,
    at: Instant,
}

/// Samples the processes the daemon tracks for each unit.
///
/// A unit's processes are its job's members as the runner last listed them (at
/// most half a second old), so the collector measures exactly the identities it
/// is given and ignores session ids, which Windows does not have. Identities
/// whose PID now names a process started at another time, exited processes and
/// processes that refuse to be opened are skipped. A process listed by several
/// targets counts for the first one.
#[derive(Debug, Clone, Default)]
pub struct WindowsSource {
    previous: Arc<Mutex<HashMap<(u32, u64), CpuReading>>>,
}

impl WindowsSource {
    /// Creates a collector with no previous readings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples synchronously: one open and three queries per process. Async
    /// callers use [`MetricsSource::sample`], which runs this on the blocking pool.
    pub fn sample_blocking(&self, units: &[UnitTarget]) -> Vec<Sample> {
        // A poisoned lock only means an earlier sample panicked; the CPU cache is
        // replaced on every call anyway.
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        collect(&mut previous, units)
    }
}

#[async_trait]
impl MetricsSource for WindowsSource {
    /// Returns `windows`.
    fn name(&self) -> &'static str {
        "windows"
    }

    /// Samples every unit target on the blocking pool; containers are ignored.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        let source = self.clone();
        let units = request.units.clone();
        tokio::task::spawn_blocking(move || source.sample_blocking(&units))
            .await
            .map_err(|error| Error::Unavailable {
                source_name: "windows",
                detail: format!("sampling task failed: {error}"),
            })
    }
}

/// Captures the identity `devlanes-sys` would record for `pid`. Returns `None`
/// once the process is gone or cannot be opened.
pub fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    devlanes_sys::identity(pid).ok().flatten()
}

/// Reads every target's processes and converts CPU times to percentages against
/// the previous call.
fn collect(previous: &mut HashMap<(u32, u64), CpuReading>, units: &[UnitTarget]) -> Vec<Sample> {
    let ts_ms = unix_ms(SystemTime::now());
    let now_filetime = filetime_now();
    let now = Instant::now();
    let mut seen = HashSet::new();
    let mut readings = HashMap::new();
    let mut samples = Vec::new();
    for target in units {
        for identity in &target.processes {
            if seen.contains(&identity.pid) {
                continue;
            }
            let usage = match process_usage(identity) {
                Ok(Some(usage)) => usage,
                Ok(None) => continue,
                // One unreadable process must not cost the unit its other members.
                Err(error) => {
                    tracing::debug!(pid = identity.pid, %error, "process usage unreadable");
                    continue;
                }
            };
            // Claimed only once read, so a stale identity in one target cannot
            // hide the live process listed by another.
            seen.insert(identity.pid);
            let key = (identity.pid, identity.start_time);
            let cpu_percent = match previous.get(&key) {
                Some(last) => percent(
                    usage.cpu_time.saturating_sub(last.cpu_time),
                    now.duration_since(last.at).as_nanos(),
                ),
                // First sight: lifetime average, so a busy process is not reported
                // idle for a tick. Creation time is wall-clock `FILETIME`.
                None => percent(
                    usage.cpu_time,
                    u128::from(now_filetime.saturating_sub(identity.start_time)) * 100,
                ),
            };
            readings.insert(
                key,
                CpuReading {
                    cpu_time: usage.cpu_time,
                    at: now,
                },
            );
            samples.push(Sample {
                subject: Subject::Process {
                    unit: target.unit.clone(),
                    pid: identity.pid,
                    start_time: identity.start_time,
                },
                cpu_percent,
                footprint_bytes: footprint(&usage),
                rss_bytes: usage.working_set,
                ts_ms,
            });
        }
    }
    // Replacing the cache drops exited processes, so it never grows past the members.
    *previous = readings;
    samples.sort_by(|a, b| a.subject.cmp(&b.subject));
    samples
}

/// The memory number: the private working set, or private bytes on Windows
/// versions that do not report it. Private bytes also count paged-out memory, so
/// they read higher for idle processes.
fn footprint(usage: &ProcessUsage) -> u64 {
    usage.private_working_set.unwrap_or(usage.private_bytes)
}

/// CPU time (100 ns units) over wall time (ns) as a percentage of one core; 0
/// when no time elapsed.
fn percent(cpu_ticks: u64, wall_ns: u128) -> f64 {
    if wall_ns == 0 {
        return 0.0;
    }
    (u128::from(cpu_ticks) * 100) as f64 / wall_ns as f64 * 100.0
}

/// The current time as a `FILETIME` count, comparable with creation times.
fn filetime_now() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos() / 100).unwrap_or(u64::MAX)
        });
    since_epoch.saturating_add(FILETIME_UNIX_OFFSET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_share_of_one_core() {
        // One second of CPU (10^7 ticks) over two seconds of wall time.
        assert!((percent(10_000_000, 2_000_000_000) - 50.0).abs() < 1e-9);
        // Two busy cores over one second.
        assert!((percent(20_000_000, 1_000_000_000) - 200.0).abs() < 1e-9);
        assert_eq!(percent(5, 0), 0.0);
    }

    #[test]
    fn footprint_prefers_the_private_working_set() {
        let usage = ProcessUsage {
            cpu_time: 0,
            working_set: 10,
            private_working_set: Some(4),
            private_bytes: 7,
        };
        assert_eq!(footprint(&usage), 4);
        let older = ProcessUsage {
            private_working_set: None,
            ..usage
        };
        assert_eq!(footprint(&older), 7);
    }

    /// The clock conversion agrees with this process's creation time: it started
    /// in the past, and not long ago.
    #[test]
    fn filetime_now_follows_creation_times() {
        let me = process_identity(std::process::id()).expect("own identity");
        let now = filetime_now();
        assert!(now >= me.start_time, "{now} < {}", me.start_time);
        // An hour in 100 ns ticks; test binaries do not live that long.
        assert!(
            now - me.start_time < 36_000_000_000,
            "{now} {}",
            me.start_time
        );
    }
}
