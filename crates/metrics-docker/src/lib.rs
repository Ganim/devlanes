//! Docker container collector for `devlanes-metrics`, using the Engine stats API.
//!
//! CPU and memory follow the docker CLI's math, `calculateCPUPercentUnix` and
//! `calculateMemUsageUnixNoCache` in
//! <https://github.com/docker/cli/blob/master/cli/command/container/stats_helpers.go>,
//! with CPU taken over a [`CPU_WINDOW`] instead of one tick.
//!
//! On macOS containers run inside one Linux VM, so the source also emits a
//! [`Subject::DockerVm`] row summing the sampled containers; [`DockerSource::vm_info`]
//! gives the VM's CPU and memory capacity from `docker info`.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use bollard::{Docker, errors::Error as DockerError, query_parameters::StatsOptions};
use devlanes_metrics::{
    ContainerTarget, Error, MetricsSource, Sample, SampleRequest, Subject, unix_ms,
};
use futures_util::{StreamExt, future::join_all};

pub use bollard::models::{
    ContainerCpuStats, ContainerMemoryStats, ContainerStatsResponse, SystemInfo,
};

/// Upper bound for one stats or info request so a wedged engine cannot stall a tick.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Emitted only where the engine runs in a VM the host cannot see into.
const EMITS_VM_ROW: bool = cfg!(target_os = "macos");

/// CPU percent of one core, exactly docker CLI's `calculateCPUPercentUnix`:
/// `(Δcontainer usage / Δsystem usage) × online CPUs × 100`, where online CPUs
/// falls back to the per-CPU array length and the result is 0 unless both deltas
/// are positive.
pub fn cpu_percent_unix(previous: &ContainerCpuStats, current: &ContainerCpuStats) -> f64 {
    let total = |stats: &ContainerCpuStats| {
        stats
            .cpu_usage
            .as_ref()
            .and_then(|usage| usage.total_usage)
            .unwrap_or(0) as f64
    };
    let system = |stats: &ContainerCpuStats| stats.system_cpu_usage.unwrap_or(0) as f64;
    let cpu_delta = total(current) - total(previous);
    let system_delta = system(current) - system(previous);
    let mut online = f64::from(current.online_cpus.unwrap_or(0));
    if online == 0.0 {
        online = current
            .cpu_usage
            .as_ref()
            .and_then(|usage| usage.percpu_usage.as_ref())
            .map_or(0.0, |per_cpu| per_cpu.len() as f64);
    }
    if system_delta > 0.0 && cpu_delta > 0.0 {
        cpu_delta / system_delta * online * 100.0
    } else {
        0.0
    }
}

/// CPU is measured over at least this much engine time. Workloads that work in
/// short bursts about once a second (postgres, Erlang schedulers) leave whole
/// 1 s intervals without a counter change, which a sample-to-sample delta
/// reports as 0%; over 2 s each interval sees at least one burst.
pub const CPU_WINDOW: Duration = Duration::from_secs(2);

/// A container's recent CPU counters, to measure over [`CPU_WINDOW`].
#[derive(Debug, Clone, Default)]
pub struct CpuWindow {
    readings: VecDeque<ContainerCpuStats>,
}

/// More readings than this means samples far faster than one a second; the
/// oldest go even inside the window.
const MAX_READINGS: usize = 64;

impl CpuWindow {
    /// CPU percent of `current` against the newest kept reading at least
    /// [`CPU_WINDOW`] older, else the oldest one, else `precpu` (first sight),
    /// with [`cpu_percent_unix`]. Then keeps `current`.
    pub fn percent(
        &mut self,
        current: &ContainerCpuStats,
        precpu: Option<&ContainerCpuStats>,
    ) -> f64 {
        let usage = |stats: &ContainerCpuStats| {
            stats
                .cpu_usage
                .as_ref()
                .and_then(|usage| usage.total_usage)
                .unwrap_or(0)
        };
        let system = |stats: &ContainerCpuStats| stats.system_cpu_usage.unwrap_or(0);
        // A counter that went back means the container restarted: start over.
        if self
            .readings
            .back()
            .is_some_and(|last| usage(current) < usage(last))
        {
            self.readings.clear();
        }
        // `system_cpu_usage` counts nanoseconds on every online CPU.
        let cpus = u64::from(current.online_cpus.unwrap_or(1).max(1));
        let span = u64::try_from(CPU_WINDOW.as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_mul(cpus);
        let elapsed = |old: &ContainerCpuStats| system(current).saturating_sub(system(old));
        let fallback = ContainerCpuStats::default();
        let base = self
            .readings
            .iter()
            .rev()
            .find(|old| elapsed(old) >= span)
            .or(self.readings.front())
            .or(precpu)
            .unwrap_or(&fallback);
        let percent = cpu_percent_unix(base, current);
        // Only the newest reading a full window back is needed later.
        while self.readings.len() >= 2 && elapsed(&self.readings[1]) >= span
            || self.readings.len() >= MAX_READINGS
        {
            self.readings.pop_front();
        }
        self.readings.push_back(current.clone());
        percent
    }
}

/// Memory usage without reclaimable page cache, exactly docker CLI's
/// `calculateMemUsageUnixNoCache`: `usage - total_inactive_file` on cgroup v1,
/// `usage - inactive_file` on cgroup v2, else `usage`.
pub fn memory_usage_no_cache(memory: &ContainerMemoryStats) -> u64 {
    let usage = memory.usage.unwrap_or(0);
    let stat = |key: &str| {
        memory
            .stats
            .as_ref()
            .and_then(|stats| stats.get(key))
            .copied()
    };
    if let Some(inactive) = stat("total_inactive_file")
        && inactive < usage
    {
        return usage - inactive;
    }
    // Go's missing-map-key zero makes this branch subtract 0 when absent.
    let inactive = stat("inactive_file").unwrap_or(0);
    if inactive < usage {
        usage - inactive
    } else {
        usage
    }
}

/// One container's numbers derived from a stats response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContainerReading {
    pub cpu_percent: f64,
    /// [`memory_usage_no_cache`]; the `MEM USAGE` column of `docker stats`.
    pub memory_bytes: u64,
    /// Anonymous memory (`anon` on cgroup v2, `total_rss`/`rss` on v1), falling
    /// back to `memory_bytes`.
    pub rss_bytes: u64,
    /// cgroup limit, or the VM's memory when the container has none.
    pub memory_limit_bytes: u64,
}

/// Derives a reading. `previous` is this source's last CPU sample of the container;
/// without one the response's `precpu_stats` is used, as the docker CLI does.
/// Returns `None` for stopped containers, whose responses carry no memory usage.
pub fn reading(
    stats: &ContainerStatsResponse,
    previous: Option<&ContainerCpuStats>,
) -> Option<ContainerReading> {
    let memory = stats.memory_stats.as_ref().filter(|m| m.usage.is_some())?;
    let current = stats.cpu_stats.as_ref()?;
    let fallback = ContainerCpuStats::default();
    let previous = previous
        .or(stats.precpu_stats.as_ref())
        .unwrap_or(&fallback);
    let memory_bytes = memory_usage_no_cache(memory);
    let stat = |key: &str| {
        memory
            .stats
            .as_ref()
            .and_then(|stats| stats.get(key))
            .copied()
    };
    Some(ContainerReading {
        cpu_percent: cpu_percent_unix(previous, current),
        memory_bytes,
        rss_bytes: stat("anon")
            .or_else(|| stat("total_rss"))
            .or_else(|| stat("rss"))
            .unwrap_or(memory_bytes),
        memory_limit_bytes: memory.limit.unwrap_or(0),
    })
}

/// Capacity of the engine host (the VM on macOS) from `docker info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerVmInfo {
    /// Engine host name, e.g. `orbstack` or `docker-desktop`.
    pub name: String,
    pub operating_system: String,
    pub server_version: String,
    pub cpus: u32,
    pub memory_total_bytes: u64,
    pub containers_running: u32,
}

impl DockerVmInfo {
    /// Extracts capacity fields; absent or negative values become 0 or empty.
    pub fn from_info(info: &SystemInfo) -> Self {
        let non_negative =
            |value: Option<i64>| value.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        Self {
            name: info.name.clone().unwrap_or_default(),
            operating_system: info.operating_system.clone().unwrap_or_default(),
            server_version: info.server_version.clone().unwrap_or_default(),
            cpus: u32::try_from(non_negative(info.ncpu)).unwrap_or(u32::MAX),
            memory_total_bytes: non_negative(info.mem_total),
            containers_running: u32::try_from(non_negative(info.containers_running))
                .unwrap_or(u32::MAX),
        }
    }
}

/// Converts an engine failure into a collector error.
fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::Unavailable {
        source_name: "docker",
        detail: error.to_string(),
    }
}

/// Samples managed containers through the Docker Engine API.
///
/// Each tick issues one non-streaming `one-shot` stats request per container,
/// concurrently, and computes CPU over each container's [`CpuWindow`] kept here.
pub struct DockerSource {
    docker: Docker,
    previous: Mutex<HashMap<String, CpuWindow>>,
}

impl DockerSource {
    /// Connects using `DOCKER_HOST` or the platform's default socket, ignoring Docker
    /// contexts; the daemon uses [`DockerSource::new`] with the infra client instead.
    /// No request is made until the first sample.
    pub fn connect() -> Result<Self, Error> {
        Docker::connect_with_defaults()
            .map(Self::new)
            .map_err(unavailable)
    }

    /// Wraps an existing client, e.g. one shared with the infra integration.
    pub fn new(docker: Docker) -> Self {
        Self {
            docker,
            previous: Mutex::new(HashMap::new()),
        }
    }

    /// Reads engine capacity from `docker info`.
    pub async fn vm_info(&self) -> Result<DockerVmInfo, Error> {
        let info = tokio::time::timeout(REQUEST_TIMEOUT, self.docker.info())
            .await
            .map_err(|_| unavailable("docker info timed out"))?
            .map_err(unavailable)?;
        Ok(DockerVmInfo::from_info(&info))
    }

    /// Fetches one stats response; `Ok(None)` means the container is gone.
    async fn stats(&self, id: &str) -> Result<Option<ContainerStatsResponse>, Error> {
        let options = StatsOptions {
            stream: false,
            one_shot: true,
        };
        let mut stream = self.docker.stats(id, Some(options));
        match tokio::time::timeout(REQUEST_TIMEOUT, stream.next()).await {
            Err(_) => Err(unavailable(format!("stats for {id} timed out"))),
            Ok(None) => Ok(None),
            Ok(Some(Ok(stats))) => Ok(Some(stats)),
            Ok(Some(Err(DockerError::DockerResponseServerError {
                status_code: 404, ..
            }))) => Ok(None),
            Ok(Some(Err(error))) => Err(unavailable(error)),
        }
    }
}

#[async_trait]
impl MetricsSource for DockerSource {
    /// Returns `docker`.
    fn name(&self) -> &'static str {
        "docker"
    }

    /// Samples every container target; unit targets are ignored. Missing or
    /// stopped containers are skipped. Fails only when every request failed,
    /// which means the engine itself is unreachable.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        if request.containers.is_empty() {
            self.previous
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            return Ok(Vec::new());
        }
        let responses = join_all(
            request
                .containers
                .iter()
                .map(|target| async move { (target, self.stats(&target.id).await) }),
        )
        .await;
        let ts_ms = unix_ms(SystemTime::now());
        // The cache only holds CPU counters, so a poisoned lock is still usable.
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = HashMap::with_capacity(responses.len());
        let mut samples = Vec::with_capacity(responses.len() + 1);
        let mut failures = Vec::new();
        for (target, response) in responses {
            let stats = match response {
                Ok(Some(stats)) => stats,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(container = %target.id, %error, "container stats failed");
                    failures.push(error);
                    // One failed request keeps the container's window.
                    if let Some(window) = previous.remove(&target.id) {
                        next.insert(target.id.clone(), window);
                    }
                    continue;
                }
            };
            let Some(mut reading) = reading(&stats, None) else {
                continue;
            };
            if let Some(cpu) = &stats.cpu_stats {
                let mut window = previous.remove(&target.id).unwrap_or_default();
                reading.cpu_percent = window.percent(cpu, stats.precpu_stats.as_ref());
                next.insert(target.id.clone(), window);
            }
            samples.push(container_sample(target, &reading, ts_ms));
        }
        // Replacing the cache forgets containers that stopped or left the request.
        *previous = next;
        drop(previous);
        if samples.is_empty()
            && failures.len() == request.containers.len()
            && let Some(error) = failures.pop()
        {
            return Err(error);
        }
        if EMITS_VM_ROW && !samples.is_empty() {
            samples.push(vm_row(&samples));
        }
        Ok(samples)
    }
}

/// Builds the leaf sample for one container.
fn container_sample(target: &ContainerTarget, reading: &ContainerReading, ts_ms: u64) -> Sample {
    Sample {
        subject: Subject::Container {
            project: target.project,
            lane: target.lane,
            id: target.id.clone(),
        },
        cpu_percent: reading.cpu_percent,
        footprint_bytes: reading.memory_bytes,
        rss_bytes: reading.rss_bytes,
        ts_ms,
    }
}

/// Sums container samples into the Docker VM row.
fn vm_row(containers: &[Sample]) -> Sample {
    containers.iter().fold(
        Sample {
            subject: Subject::DockerVm,
            cpu_percent: 0.0,
            footprint_bytes: 0,
            rss_bytes: 0,
            ts_ms: 0,
        },
        |mut total, sample| {
            total.cpu_percent += sample.cpu_percent;
            total.footprint_bytes = total.footprint_bytes.saturating_add(sample.footprint_bytes);
            total.rss_bytes = total.rss_bytes.saturating_add(sample.rss_bytes);
            total.ts_ms = total.ts_ms.max(sample.ts_ms);
            total
        },
    )
}
