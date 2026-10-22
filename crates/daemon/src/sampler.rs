//! Resource sampling (plan 13.5). Ticks every second while a client watches
//! (subscribers or recent `metrics` calls), and every five seconds otherwise while
//! units run, so CI summaries still get per-start peaks. Monitoring only.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use devlanes_core::{ProcessIdentity, UnitRunId};
use devlanes_metrics::{
    COARSE_CAPACITY, COARSE_INTERVAL_MS, ContainerTarget, History, RunTracker, Sample,
    SampleRequest, Series, Subject, UnitKey, UnitTarget, aggregate,
};
use devlanes_proto::{Context, LaneSummary, RpcError, UnitPeak};
use devlanes_state::{RollupRow, RunSummaryRow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    engine::{Inner, params},
    model::{LaneRt, lock},
    units::now_ms,
};

/// How long a `metrics` call keeps the fast tick on.
const INTEREST: Duration = Duration::from_secs(15);
/// Points of history returned per unit.
const HISTORY_POINTS: usize = 120;
/// The in-memory history horizon (the coarse ring, 6 hours). Series idle longer
/// hold nothing a client can still be shown, so they are dropped with the
/// per-unit entries that point at them; otherwise every removed lane or unit
/// would keep its preallocated rings for the daemon's lifetime.
const HISTORY_HORIZON: Duration =
    Duration::from_millis(COARSE_INTERVAL_MS * COARSE_CAPACITY as u64);
/// How often the sampler prunes; pruning walks every series.
const PRUNE_EVERY_MS: u64 = 60_000;

/// Sampler state shared with the `metrics` method.
#[derive(Default)]
pub(crate) struct MetricsState {
    pub history: History,
    pub tracker: RunTracker,
    /// Runs being tracked, with their persistence series key.
    pub tracked: HashMap<UnitRunId, String>,
    /// Latest `(cpu, memory)` per `(lane reference, unit)`.
    pub latest: BTreeMap<(String, String), (f64, u64)>,
    /// Latest unit CPU with its time and run, so the start gate's settle rule
    /// only counts new readings of the run it is watching.
    pub cpu_readings: BTreeMap<(String, String), CpuReading>,
    /// Last [`CpuReading::seq`] handed out.
    pub reading_seq: u64,
    /// Series key per `(lane reference, unit)` for history lookups.
    pub subjects: BTreeMap<(String, String), Subject>,
    /// Infra containers of the last tick with their readings; rebuilt every tick,
    /// so containers that went away drop out.
    pub containers: BTreeMap<Subject, ContainerRow>,
    pub interest_until: Option<Instant>,
    /// When the sampler last pruned (Unix ms).
    pub last_prune_ms: u64,
}

impl MetricsState {
    /// Forgets series idle past [`HISTORY_HORIZON`], then the per-unit entries of
    /// units that are not sampled any more and whose series went with them. Call
    /// after [`History::drain_rollups`] so no closed minute is lost. Runs at most
    /// once per [`PRUNE_EVERY_MS`].
    fn prune(&mut self, now_ms: u64, live: &BTreeSet<(String, String)>) {
        if now_ms < self.last_prune_ms.saturating_add(PRUNE_EVERY_MS) {
            return;
        }
        self.last_prune_ms = now_ms;
        self.history.prune(now_ms, HISTORY_HORIZON);
        let history = &self.history;
        self.subjects
            .retain(|key, subject| live.contains(key) || history.series(subject).is_some());
        let subjects = &self.subjects;
        self.latest.retain(|key, _| subjects.contains_key(key));
        self.cpu_readings
            .retain(|key, _| subjects.contains_key(key));
    }
}

/// One sampled infra container and the lanes that use it.
#[derive(Debug, Clone)]
pub(crate) struct ContainerRow {
    /// Namespace key (compose service or `infra.containers` name).
    pub name: String,
    /// Container name the Docker API was asked about.
    pub docker: String,
    pub shared: bool,
    /// Monitor group: the lane reference, or `<project> (shared)` for shared infra.
    pub group: String,
    /// References of the lanes whose infra includes the container.
    pub lanes: Vec<String>,
    /// `(cpu, memory)` from this tick, when the engine reported one.
    pub latest: Option<(f64, u64)>,
}

/// `metrics` parameters.
#[derive(Debug, Default, Deserialize)]
struct MetricsParams {
    #[serde(default)]
    peaks: bool,
}

/// One unit CPU and footprint reading, for the start gate's slot holders.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CpuReading {
    pub cpu_percent: f64,
    /// Increases with every stored reading; the settle rule compares these,
    /// not wall-clock times, to tell new readings from repeats.
    pub seq: u64,
    /// The unit's process-tree footprint in the same sample.
    pub footprint_bytes: u64,
    /// Leader PID of the run the sample belongs to.
    pub pid: Option<u32>,
}

/// One unit to sample with its lane.
struct Target {
    lane: Arc<LaneRt>,
    reference: String,
    name: String,
    run: Option<UnitRunId>,
    key: UnitKey,
    /// Leader PID when the target was collected; a restart changes it.
    pid: Option<u32>,
}

impl Inner {
    /// Runs the sampler until shutdown.
    pub(crate) async fn sampler_loop(self: Arc<Self>) {
        if self.integ.metrics.is_empty() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut count: u64 = 0;
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            count += 1;
            let interested = self.bus.subscribers() > 0
                || lock(&self.metrics)
                    .interest_until
                    .is_some_and(|until| until > Instant::now());
            if interested || count.is_multiple_of(5) {
                self.sample_once().await;
            }
        }
    }

    /// Samples every live unit once and updates history, peaks and rollups.
    pub(crate) async fn sample_once(self: &Arc<Self>) {
        let mut targets = Vec::new();
        let mut request = SampleRequest::default();
        let mut containers: BTreeMap<Subject, ContainerRow> = BTreeMap::new();
        for (project, lane) in self.all_lanes() {
            let project_name = project.name();
            let (reference, lane_id, units) = {
                let st = lane.st();
                let Some(record) = &st.record else { continue };
                let units: Vec<_> = st
                    .units
                    .iter()
                    .filter(|(_, rec)| rec.host.is_alive())
                    .map(|(name, rec)| (name.clone(), rec.host.clone(), rec.run_id))
                    .collect();
                let reference = st.reference(&project_name);
                // Shared infra appears in every lane's list; one row per container.
                for container in &st.infra_containers {
                    let subject = Subject::Container {
                        project: project.id,
                        lane: (!container.shared).then_some(record.id),
                        id: container.docker.clone(),
                    };
                    containers
                        .entry(subject)
                        .or_insert_with(|| ContainerRow {
                            name: container.name.clone(),
                            docker: container.docker.clone(),
                            shared: container.shared,
                            group: if container.shared {
                                format!("{project_name} (shared)")
                            } else {
                                reference.clone()
                            },
                            lanes: Vec::new(),
                            latest: None,
                        })
                        .lanes
                        .push(reference.clone());
                }
                (reference, record.id, units)
            };
            for (name, host, run) in units {
                let key = UnitKey {
                    project: project.id,
                    lane: lane_id,
                    unit: name.clone(),
                };
                let identity = host.identity();
                let mut processes = host.subjects().await;
                if let Some(identity) = identity
                    && !processes.iter().any(|p| p.pid == identity.pid)
                {
                    processes.push(identity);
                }
                request.units.push(UnitTarget {
                    unit: key.clone(),
                    sessions: unit_sessions(identity),
                    processes,
                });
                targets.push(Target {
                    lane: lane.clone(),
                    reference: reference.clone(),
                    name,
                    run,
                    key,
                    pid: identity.map(|identity| identity.pid),
                });
            }
        }
        request.containers = containers
            .keys()
            .filter_map(|subject| match subject {
                Subject::Container { project, lane, id } => Some(ContainerTarget {
                    project: *project,
                    lane: *lane,
                    id: id.clone(),
                }),
                _ => None,
            })
            .collect();
        let mut leaves = Vec::new();
        for source in &self.integ.metrics {
            match source.sample(&request).await {
                Ok(samples) => leaves.extend(samples),
                Err(error) => tracing::debug!(source = source.name(), %error, "sample failed"),
            }
        }
        let totals = aggregate(&leaves);
        // Container leaves get their own history, for the monitor and rollups.
        let container_samples: Vec<Sample> = leaves
            .into_iter()
            .filter(|sample| containers.contains_key(&sample.subject))
            .collect();
        for sample in &container_samples {
            if let Some(row) = containers.get_mut(&sample.subject) {
                row.latest = Some((sample.cpu_percent, sample.footprint_bytes));
            }
        }
        let sampled: BTreeSet<(String, String)> = targets
            .iter()
            .map(|target| (target.reference.clone(), target.name.clone()))
            .collect();
        let now = now_ms();
        let (rollups, finished) = {
            let mut state = lock(&self.metrics);
            state.history.record_all(&totals);
            state.history.record_all(&container_samples);
            state.containers = containers;
            for target in &targets {
                if let Some(run) = target.run
                    && !state.tracked.contains_key(&run)
                {
                    state
                        .tracker
                        .start(run, target.key.clone(), now.saturating_sub(1000));
                    state
                        .tracked
                        .insert(run, Subject::Unit(target.key.clone()).key());
                }
                state.subjects.insert(
                    (target.reference.clone(), target.name.clone()),
                    Subject::Unit(target.key.clone()),
                );
            }
            state.tracker.observe_all(&totals);
            let live: Vec<UnitRunId> = targets.iter().filter_map(|t| t.run).collect();
            let ended: Vec<(UnitRunId, String)> = state
                .tracked
                .iter()
                .filter(|(run, _)| !live.contains(run))
                .map(|(run, series)| (*run, series.clone()))
                .collect();
            let mut finished = Vec::new();
            for (run, series) in ended {
                state.tracked.remove(&run);
                if let Some(summary) = state.tracker.finish(run, now) {
                    finished.push((summary, series));
                }
            }
            for target in &targets {
                let Some(sample) = totals
                    .iter()
                    .find(|s| s.subject == Subject::Unit(target.key.clone()))
                else {
                    continue;
                };
                state.latest.insert(
                    (target.reference.clone(), target.name.clone()),
                    (sample.cpu_percent, sample.footprint_bytes),
                );
                state.reading_seq += 1;
                let seq = state.reading_seq;
                state.cpu_readings.insert(
                    (target.reference.clone(), target.name.clone()),
                    CpuReading {
                        cpu_percent: sample.cpu_percent,
                        seq,
                        footprint_bytes: sample.footprint_bytes,
                        pid: target.pid,
                    },
                );
                let mut st = target.lane.st();
                let peak = st.peaks.entry(target.name.clone()).or_insert((None, None));
                peak.0 = Some(peak.0.unwrap_or(0).max(sample.footprint_bytes));
                peak.1 = Some(peak.1.unwrap_or(0.0).max(sample.cpu_percent));
            }
            let rollups = state.history.drain_rollups(now);
            state.prune(now, &sampled);
            (rollups, finished)
        };
        let rows: Vec<RollupRow> = rollups
            .into_iter()
            .map(|record| RollupRow {
                series: record.subject.key(),
                minute_ms: record.window.start_ms,
                samples: record.window.samples,
                cpu_min: record.window.cpu.min,
                cpu_avg: record.window.cpu.avg,
                cpu_max: record.window.cpu.max,
                mem_min: record.window.footprint.min,
                mem_avg: record.window.footprint.avg,
                mem_max: record.window.footprint.max,
                rss_max: record.window.rss.max,
            })
            .collect();
        let _ = self.store.insert_rollups(rows).await;
        for (summary, series) in finished {
            let _ = self
                .store
                .record_run_summary(RunSummaryRow {
                    unit_run_id: summary.run,
                    series,
                    started_ms: summary.started_ms,
                    ended_ms: summary.ended_ms,
                    samples: summary.samples,
                    mem_p50: summary.footprint.p50,
                    mem_p95: summary.footprint.p95,
                    mem_peak: summary.footprint.peak,
                    cpu_p50: summary.cpu.p50,
                    cpu_p95: summary.cpu.p95,
                    cpu_peak: summary.cpu.peak,
                    rss_peak: summary.rss_peak,
                })
                .await;
        }
    }

    /// Adds the latest CPU and memory to a lane summary's units.
    pub(crate) fn fill_metrics(&self, summary: &mut LaneSummary) {
        let state = lock(&self.metrics);
        for unit in &mut summary.units {
            if unit.state == "idle" || unit.state == "stopped" || unit.state == "exited" {
                continue;
            }
            if let Some((cpu, memory)) = state
                .latest
                .get(&(summary.reference.clone(), unit.name.clone()))
            {
                unit.cpu_percent = Some(*cpu);
                unit.memory_bytes = Some(*memory);
            }
        }
        for container in &mut summary.containers {
            // Replicas of one compose service add up to the service's numbers.
            let readings: Vec<(f64, u64)> = state
                .containers
                .values()
                .filter(|row| row.name == container.name && row.lanes.contains(&summary.reference))
                .filter_map(|row| row.latest)
                .collect();
            if !readings.is_empty() {
                container.cpu_percent = Some(readings.iter().map(|r| r.0).sum());
                container.memory_bytes = Some(readings.iter().map(|r| r.1).sum());
            }
        }
    }

    /// `metrics`: per-unit samples and history, or `peaks: true` for the current
    /// lane start.
    pub(crate) async fn metrics_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: MetricsParams = params(value)?;
        let first = {
            let mut state = lock(&self.metrics);
            let first = state
                .interest_until
                .is_none_or(|until| until < Instant::now());
            state.interest_until = Some(Instant::now() + INTEREST);
            first
        };
        if first {
            self.sample_once().await;
        }
        if args.peaks {
            let (project, lane) = self.target_lane(context).await?;
            let st = lane.st();
            let mut names: Vec<String> = st.units.keys().cloned().collect();
            names.extend(
                st.peaks
                    .keys()
                    .filter(|k| !st.units.contains_key(*k))
                    .cloned(),
            );
            let peaks: Vec<UnitPeak> = names
                .into_iter()
                .map(|unit| {
                    let peak = st.peaks.get(&unit).copied().unwrap_or((None, None));
                    UnitPeak {
                        unit,
                        peak_memory_bytes: peak.0,
                        peak_cpu_percent: peak.1,
                    }
                })
                .collect();
            return Ok(json!({"lane": st.reference(&project.name()), "peaks": peaks}));
        }
        let state = lock(&self.metrics);
        let mut units = Vec::new();
        for (project, lane) in self.all_lanes() {
            let st = lane.st();
            let reference = st.reference(&project.name());
            for (name, rec) in &st.units {
                if !rec.host.is_alive() {
                    continue;
                }
                let key = (reference.clone(), name.clone());
                let latest = state.latest.get(&key).copied();
                let history = recent_points(
                    state
                        .subjects
                        .get(&key)
                        .and_then(|subject| state.history.series(subject)),
                );
                units.push(json!({
                    "lane": reference,
                    "unit": name,
                    "cpu_percent": latest.map(|l| l.0),
                    "memory_bytes": latest.map(|l| l.1),
                    "history": history,
                }));
            }
        }
        units.extend(container_entries(&state));
        Ok(json!({"units": units}))
    }
}

/// Sessions whose members belong to a unit: the leader's, which also catches
/// members the runner has not tracked yet.
#[cfg(unix)]
fn unit_sessions(leader: Option<ProcessIdentity>) -> Vec<u32> {
    leader.map(|leader| vec![leader.sid]).unwrap_or_default()
}

/// None: Windows has no sessions to group by (an identity's `sid` is its own
/// PID). The unit's job members arrive as tracked processes instead.
#[cfg(windows)]
fn unit_sessions(_leader: Option<ProcessIdentity>) -> Vec<u32> {
    Vec::new()
}

/// The newest [`HISTORY_POINTS`] raw points as `[ts_ms, cpu, memory]`.
fn recent_points(series: Option<&Series>) -> Vec<Value> {
    let Some(series) = series else {
        return Vec::new();
    };
    let skip = series.points().len().saturating_sub(HISTORY_POINTS);
    series
        .points()
        .skip(skip)
        .map(|p| json!([p.ts_ms, p.cpu_percent, p.footprint_bytes]))
        .collect()
}

/// Running infra containers as `metrics` rows with `kind: container`, grouped
/// under their lane or `<project> (shared)`. Replicas of one service are told
/// apart by their container name.
fn container_entries(state: &MetricsState) -> Vec<Value> {
    let running: Vec<(&Subject, &ContainerRow)> = state
        .containers
        .iter()
        .filter(|(_, row)| row.latest.is_some())
        .collect();
    running
        .iter()
        .map(|(subject, row)| {
            let replicas = running
                .iter()
                .filter(|(_, other)| other.group == row.group && other.name == row.name)
                .count();
            let unit = if replicas > 1 { &row.docker } else { &row.name };
            json!({
                "lane": row.group,
                "unit": unit,
                "kind": "container",
                "container": row.docker,
                "shared": row.shared,
                "cpu_percent": row.latest.map(|l| l.0),
                "memory_bytes": row.latest.map(|l| l.1),
                "history": recent_points(state.history.series(subject)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use devlanes_core::{LaneId, ProjectId};

    use super::*;

    /// A unit total sampled at `ts_ms`.
    fn unit_sample(unit: &str, ts_ms: u64) -> Sample {
        Sample {
            subject: Subject::Unit(UnitKey {
                project: ProjectId(1),
                lane: LaneId(1),
                unit: unit.into(),
            }),
            cpu_percent: 1.0,
            footprint_bytes: 1,
            rss_bytes: 1,
            ts_ms,
        }
    }

    /// Fills every per-unit map for `unit` as one tick would.
    fn track(state: &mut MetricsState, unit: &str, ts_ms: u64) {
        let sample = unit_sample(unit, ts_ms);
        state.history.record(&sample);
        let key = ("acme@1".to_owned(), unit.to_owned());
        state.subjects.insert(key.clone(), sample.subject.clone());
        state.latest.insert(key.clone(), (1.0, 1));
        state.cpu_readings.insert(
            key,
            CpuReading {
                cpu_percent: 1.0,
                seq: ts_ms,
                footprint_bytes: 1,
                pid: None,
            },
        );
    }

    /// A unit gone for longer than the history horizon loses its series and
    /// entries; a live one and a recently gone one keep theirs.
    #[test]
    fn prune_forgets_units_gone_past_the_horizon() {
        let horizon = HISTORY_HORIZON.as_millis() as u64;
        let now = horizon + 600_000;
        let mut state = MetricsState::default();
        track(&mut state, "gone", 0);
        track(&mut state, "recent", now - 60_000);
        track(&mut state, "live", 0);
        state.history.drain_rollups(now);
        let live = BTreeSet::from([("acme@1".to_owned(), "live".to_owned())]);
        state.prune(now, &live);
        let units: Vec<&str> = state.subjects.keys().map(|(_, u)| u.as_str()).collect();
        assert_eq!(units, ["live", "recent"]);
        assert_eq!(state.latest.len(), 2);
        assert_eq!(state.cpu_readings.len(), 2);
        assert!(
            state
                .history
                .series(&unit_sample("gone", 0).subject)
                .is_none()
        );
        // Within a minute of the last pass nothing is walked again.
        track(&mut state, "gone", 0);
        state.prune(now + 1_000, &live);
        assert_eq!(state.subjects.len(), 3);
    }
}
