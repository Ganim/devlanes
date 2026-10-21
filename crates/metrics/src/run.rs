use std::{collections::HashMap, time::Duration};

use devlanes_core::UnitRunId;
use serde::{Deserialize, Serialize};

use crate::{QuantileSketch, Sample, Subject, UnitKey};

/// Median, 95th percentile and peak of a byte series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ByteSummary {
    pub p50: u64,
    pub p95: u64,
    pub peak: u64,
}

/// Median, 95th percentile and peak of a CPU percent series.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CpuSummary {
    pub p50: f64,
    pub p95: f64,
    pub peak: f64,
}

/// Resource profile of one unit run, computed from its unit-total samples.
/// Percentiles are sketch estimates within 1%; peaks are exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run: UnitRunId,
    pub unit: UnitKey,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub samples: u64,
    pub footprint: ByteSummary,
    pub cpu: CpuSummary,
    pub rss_peak: u64,
}

impl RunSummary {
    /// Wall-clock length of the run.
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.ended_ms.saturating_sub(self.started_ms))
    }
}

#[derive(Debug, Clone)]
struct ActiveRun {
    unit: UnitKey,
    started_ms: u64,
    last_ms: u64,
    footprint: QuantileSketch,
    cpu: QuantileSketch,
    rss_peak: u64,
}

impl ActiveRun {
    /// Builds the summary as of `ended_ms`.
    fn summary(&self, run: UnitRunId, ended_ms: u64) -> RunSummary {
        let bytes = |q: f64| self.footprint.quantile(q).map_or(0, |v| v.round() as u64);
        let cpu = |q: f64| self.cpu.quantile(q).unwrap_or(0.0);
        RunSummary {
            run,
            unit: self.unit.clone(),
            started_ms: self.started_ms,
            ended_ms,
            samples: self.footprint.count(),
            footprint: ByteSummary {
                p50: bytes(0.5),
                p95: bytes(0.95),
                peak: self.footprint.max().map_or(0, |v| v as u64),
            },
            cpu: CpuSummary {
                p50: cpu(0.5),
                p95: cpu(0.95),
                peak: self.cpu.max().unwrap_or(0.0),
            },
            rss_peak: self.rss_peak,
        }
    }
}

/// Builds per-run summaries from [`Subject::Unit`] samples.
#[derive(Debug, Clone, Default)]
pub struct RunTracker {
    runs: HashMap<UnitRunId, ActiveRun>,
}

impl RunTracker {
    /// Starts tracking a run. Restarting an id discards its previous data.
    pub fn start(&mut self, run: UnitRunId, unit: UnitKey, started_ms: u64) {
        self.runs.insert(
            run,
            ActiveRun {
                unit,
                started_ms,
                last_ms: started_ms,
                footprint: QuantileSketch::default(),
                cpu: QuantileSketch::default(),
                rss_peak: 0,
            },
        );
    }

    /// Feeds a unit-total sample to every active run of that unit. Other subjects
    /// and samples taken before a run started are ignored.
    pub fn observe(&mut self, sample: &Sample) {
        let Subject::Unit(unit) = &sample.subject else {
            return;
        };
        for run in self.runs.values_mut() {
            if &run.unit == unit && sample.ts_ms >= run.started_ms {
                run.footprint.add(sample.footprint_bytes as f64);
                run.cpu.add(sample.cpu_percent);
                run.rss_peak = run.rss_peak.max(sample.rss_bytes);
                run.last_ms = run.last_ms.max(sample.ts_ms);
            }
        }
    }

    /// Feeds every sample through [`RunTracker::observe`].
    pub fn observe_all(&mut self, samples: &[Sample]) {
        samples.iter().for_each(|sample| self.observe(sample));
    }

    /// Returns the live summary of an active run, ending at its latest sample.
    pub fn current(&self, run: UnitRunId) -> Option<RunSummary> {
        self.runs
            .get(&run)
            .map(|active| active.summary(run, active.last_ms))
    }

    /// Stops tracking a run and returns its final summary for persistence.
    pub fn finish(&mut self, run: UnitRunId, ended_ms: u64) -> Option<RunSummary> {
        self.runs
            .remove(&run)
            .map(|active| active.summary(run, ended_ms.max(active.started_ms)))
    }
}

/// Which past runs count toward a baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaselinePolicy {
    /// Most recent qualifying runs to use.
    pub runs: usize,
    /// Shorter runs are skipped: they mostly measure startup.
    pub min_duration: Duration,
}

impl Default for BaselinePolicy {
    /// Uses the last 10 runs of at least 2 minutes.
    fn default() -> Self {
        Self {
            runs: 10,
            min_duration: Duration::from_secs(120),
        }
    }
}

/// Typical load of a unit: the median of recent runs' p95 values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub footprint_p95: u64,
    pub cpu_p95: f64,
    /// Number of runs the medians were taken over.
    pub runs: usize,
}

/// Computes a baseline from one unit's past runs, in any order. Runs without
/// samples or shorter than the policy minimum are ignored; the newest
/// `policy.runs` by end time are used. Returns `None` when no run qualifies.
pub fn baseline(runs: &[RunSummary], policy: BaselinePolicy) -> Option<Baseline> {
    let mut recent: Vec<&RunSummary> = runs
        .iter()
        .filter(|run| run.samples > 0 && run.duration() >= policy.min_duration)
        .collect();
    recent.sort_unstable_by_key(|run| std::cmp::Reverse(run.ended_ms));
    recent.truncate(policy.runs.max(1));
    let mut footprint: Vec<f64> = recent.iter().map(|run| run.footprint.p95 as f64).collect();
    let mut cpu: Vec<f64> = recent.iter().map(|run| run.cpu.p95).collect();
    Some(Baseline {
        footprint_p95: median(&mut footprint)?.round() as u64,
        cpu_p95: median(&mut cpu)?,
        runs: recent.len(),
    })
}

/// Median with the mean of the middle pair for even lengths.
fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}
