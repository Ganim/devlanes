//! Platform-neutral resource monitoring: samples, aggregation, in-memory history,
//! per-run summaries and baselines. Monitoring only; nothing here limits or
//! signals processes.
//!
//! Collectors (`devlanes-metrics-macos`, `-linux`, `-windows`, `-docker`) implement
//! [`MetricsSource`] and emit leaf samples ([`Subject::Process`],
//! [`Subject::Container`], [`Subject::DockerVm`]). The daemon rolls leaves up with
//! [`aggregate`], records everything in [`History`], feeds unit totals to
//! [`RunTracker`] and persists [`RollupRecord`]s.
//!
//! ```
//! use devlanes_core::{LaneId, ProjectId};
//! use devlanes_metrics::{History, Sample, Subject, UnitKey, aggregate};
//!
//! let unit = UnitKey { project: ProjectId(1), lane: LaneId(2), unit: "web".into() };
//! let leaf = Sample {
//!     subject: Subject::Process { unit: unit.clone(), pid: 42, start_time: 7 },
//!     cpu_percent: 12.5,
//!     footprint_bytes: 1 << 20,
//!     rss_bytes: 1 << 19,
//!     ts_ms: 61_000,
//! };
//! let totals = aggregate(std::slice::from_ref(&leaf));
//! let mut history = History::default();
//! history.record_all(&totals);
//! assert!(history.series(&Subject::Unit(unit)).is_some());
//! assert_eq!(history.drain_rollups(120_000).len(), 3); // unit, lane, project
//! ```

mod aggregate;
mod history;
mod ring;
mod run;
mod sketch;

pub use aggregate::aggregate;
pub use history::{
    ByteStats, COARSE_CAPACITY, COARSE_INTERVAL_MS, CpuStats, History, HistoryConfig, Point,
    RAW_CAPACITY, ROLLUP_INTERVAL_MS, RollupRecord, Series, Window, retention_cutoff_ms,
};
pub use ring::Ring;
pub use run::{
    Baseline, BaselinePolicy, ByteSummary, CpuSummary, RunSummary, RunTracker, baseline,
};
pub use sketch::QuantileSketch;

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use devlanes_core::{LaneId, ProcessIdentity, ProjectId};
use serde::{Deserialize, Serialize};

/// A unit within a lane; the daemon's stable name for one supervised process tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnitKey {
    pub project: ProjectId,
    pub lane: LaneId,
    pub unit: String,
}

/// What a sample measures. Collectors emit leaves; [`aggregate`] derives totals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    Project {
        project: ProjectId,
    },
    Lane {
        project: ProjectId,
        lane: LaneId,
    },
    Unit(UnitKey),
    /// One process of a unit. `start_time` uses the [`ProcessIdentity`] token.
    Process {
        unit: UnitKey,
        pid: u32,
        start_time: u64,
    },
    /// A container; shared project infra has no lane.
    Container {
        project: ProjectId,
        lane: Option<LaneId>,
        id: String,
    },
    /// Sum of sampled containers inside the Docker engine's VM (macOS only).
    DockerVm,
}

impl Subject {
    /// Returns the owning project, if the subject belongs to one.
    pub fn project(&self) -> Option<ProjectId> {
        match self {
            Self::Project { project }
            | Self::Lane { project, .. }
            | Self::Container { project, .. } => Some(*project),
            Self::Unit(unit) | Self::Process { unit, .. } => Some(unit.project),
            Self::DockerVm => None,
        }
    }

    /// Returns the owning lane, if the subject belongs to one.
    pub fn lane(&self) -> Option<LaneId> {
        match self {
            Self::Lane { lane, .. } => Some(*lane),
            Self::Container { lane, .. } => *lane,
            Self::Unit(unit) | Self::Process { unit, .. } => Some(unit.lane),
            Self::Project { .. } | Self::DockerVm => None,
        }
    }

    /// Returns the owning unit for unit and process subjects.
    pub fn unit(&self) -> Option<&UnitKey> {
        match self {
            Self::Unit(unit) | Self::Process { unit, .. } => Some(unit),
            _ => None,
        }
    }

    /// Stable text key for persistence (`metric_rollups.series`). Free-form names
    /// come last so a `:` inside a unit name or container id cannot shift fields.
    pub fn key(&self) -> String {
        match self {
            Self::Project { project } => format!("project:{}", project.0),
            Self::Lane { project, lane } => format!("lane:{}:{}", project.0, lane.0),
            Self::Unit(unit) => format!("unit:{}:{}:{}", unit.project.0, unit.lane.0, unit.unit),
            Self::Process {
                unit,
                pid,
                start_time,
            } => format!(
                "process:{}:{}:{pid}:{start_time}:{}",
                unit.project.0, unit.lane.0, unit.unit
            ),
            Self::Container { project, lane, id } => match lane {
                Some(lane) => format!("container:{}:{}:{id}", project.0, lane.0),
                None => format!("container:{}:-:{id}", project.0),
            },
            Self::DockerVm => "docker_vm".into(),
        }
    }
}

/// One measurement. CPU is a percentage of one core (200 = two busy cores), the
/// convention shared by `ps`, `top` and `docker stats`.
///
/// `footprint_bytes` is the primary memory number: `phys_footprint` on macOS, PSS
/// (RSS until PSS is read) on Linux, the private working set on Windows, and the
/// docker CLI's cache-free usage for containers. `rss_bytes` is for detail views only; it double counts shared pages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub subject: Subject,
    pub cpu_percent: f64,
    pub footprint_bytes: u64,
    pub rss_bytes: u64,
    /// Unix time in milliseconds when the collector read the value.
    pub ts_ms: u64,
}

/// Processes the daemon attributes to one unit. The daemon owns session and
/// descendant tracking; collectors only measure what they are given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitTarget {
    pub unit: UnitKey,
    /// Session ids whose members belong to the unit (normally the leader's).
    /// Collectors ignore 0 and 1 so a bad id cannot sweep in system sessions.
    /// Empty on Windows, which has no sessions; `processes` lists the job's members.
    pub sessions: Vec<u32>,
    /// Leader and tracked descendants. Entries whose start time no longer
    /// matches are skipped because the PID was reused.
    pub processes: Vec<ProcessIdentity>,
}

/// A managed container to sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTarget {
    pub project: ProjectId,
    /// `None` for shared project infra.
    pub lane: Option<LaneId>,
    /// Container id or name accepted by the Docker API.
    pub id: String,
}

/// Everything to sample in one tick. Each source reads only the part it handles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SampleRequest {
    pub units: Vec<UnitTarget>,
    pub containers: Vec<ContainerTarget>,
}

/// Collector failures. Processes or containers that vanish mid-sample are skipped,
/// not reported, because exits between ticks are normal.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("{source_name} unavailable: {detail}")]
    Unavailable {
        source_name: &'static str,
        detail: String,
    },
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
}

/// A metrics collector. Implementations keep the previous reading per process or
/// container so CPU percentages are deltas between successive calls. A process's
/// first reading reports its lifetime average; a container's first reading uses
/// the engine's `precpu_stats`, as the docker CLI does.
///
/// Implementations must not block the async executor; blocking OS reads belong in
/// `spawn_blocking`.
#[async_trait]
pub trait MetricsSource: Send + Sync {
    /// Short identifier for diagnostics (`macos`, `linux`, `windows`, `docker`).
    fn name(&self) -> &'static str;

    /// Samples the request's subjects this source understands and returns leaf
    /// samples. Subjects the source cannot see are omitted.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error>;
}

/// Converts a wall-clock instant to Unix milliseconds, clamping pre-epoch times to 0.
pub fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}
