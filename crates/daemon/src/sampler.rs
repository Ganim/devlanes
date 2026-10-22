//! Resource sampling (plan 13.5). Ticks every second while a client watches
//! (subscribers or recent `metrics` calls), and every five seconds otherwise while
//! units run, so CI summaries still get per-start peaks. Monitoring only.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use devlanes_core::UnitRunId;
use devlanes_metrics::{
    COARSE_CAPACITY, COARSE_INTERVAL_MS, ContainerTarget, History, RAW_CAPACITY, RunTracker,
    Sample, SampleRequest, Series, Subject, UnitKey, UnitTarget, Window, aggregate,
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
    /// The machine as of the last tick, for `metrics.system`.
    pub system: Option<SystemReading>,
    /// `(ts_ms, cpu_percent, available_memory_bytes)` per tick, newest last,
    /// at most [`RAW_CAPACITY`] entries.
    pub system_history: VecDeque<(u64, Option<f64>, Option<u64>)>,
    /// Physical memory, read once.
    pub memory_total: Option<u64>,
}

/// One reading of the whole machine.
#[derive(Debug, Clone, Default)]
pub(crate) struct SystemReading {
    /// Busy share of all CPUs, 0-100.
    pub cpu_percent: Option<f64>,
    pub memory_pressure: Option<&'static str>,
    pub available_memory_bytes: Option<u64>,
    /// Linux PSI `cpu some avg10`.
    pub cpu_stall_percent: Option<f64>,
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
    /// How far back histories reach; default [`DEFAULT_RANGE_MS`], at most
    /// [`HISTORY_HORIZON`].
    #[serde(default)]
    range_ms: Option<u64>,
    /// History points per row: buckets of the range, with the highest CPU and the
    /// average memory of each. Without it, every point of the range.
    #[serde(default)]
    points: Option<usize>,
    /// Also send `windows` (min/avg/max) per row; always on past the raw ring.
    #[serde(default)]
    windows: bool,
}

/// The raw ring: 15 minutes at one point per second.
const RAW_RANGE_MS: u64 = RAW_CAPACITY as u64 * 1000;
/// Default `range_ms`.
const DEFAULT_RANGE_MS: u64 = RAW_RANGE_MS;

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
                    sessions: identity.map(|i| vec![i.sid]).unwrap_or_default(),
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
        let system = self.read_system().await;
        let now = now_ms();
        let (rollups, finished) = {
            let mut state = lock(&self.metrics);
            if let Some((reading, total)) = system {
                state.memory_total = state.memory_total.or(total);
                if state.system_history.len() >= RAW_CAPACITY {
                    state.system_history.pop_front();
                }
                state.system_history.push_back((
                    now,
                    reading.cpu_percent,
                    reading.available_memory_bytes,
                ));
                state.system = Some(reading);
            }
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
                    // No process of the unit was read this tick: report no reading
                    // rather than repeating the last one as if it were current.
                    state
                        .latest
                        .remove(&(target.reference.clone(), target.name.clone()));
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

    /// Reads the machine's CPU, memory pressure and memory, plus total memory the
    /// first time; `None` without a system source.
    async fn read_system(&self) -> Option<(SystemReading, Option<u64>)> {
        let source = self.integ.system_pressure.as_ref()?;
        let sample = source.sample().await.ok().unwrap_or_default();
        let available = source.available_memory().await.ok().flatten();
        let known = lock(&self.metrics).memory_total.is_some();
        let total = if known {
            None
        } else {
            source.total_memory().await.ok().flatten()
        };
        Some((
            SystemReading {
                cpu_percent: sample.cpu_percent,
                memory_pressure: sample.memory.map(|level| level.word()),
                available_memory_bytes: available,
                cpu_stall_percent: sample.cpu_stall_percent,
            },
            total,
        ))
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
        let range_ms = args
            .range_ms
            .unwrap_or(DEFAULT_RANGE_MS)
            .clamp(1000, HISTORY_HORIZON.as_millis() as u64);
        let now = now_ms();
        let view = View {
            since_ms: now.saturating_sub(range_ms),
            range_ms,
            points: args.points.filter(|points| *points > 0),
            windows: args.windows || range_ms > RAW_RANGE_MS,
        };
        let state = lock(&self.metrics);
        let mut units = Vec::new();
        let mut lanes = Vec::new();
        for (project, lane) in self.all_lanes() {
            let st = lane.st();
            let reference = st.reference(&project.name());
            for (name, rec) in &st.units {
                if !rec.host.is_alive() {
                    continue;
                }
                let key = (reference.clone(), name.clone());
                let latest = state.latest.get(&key).copied();
                let series = state
                    .subjects
                    .get(&key)
                    .and_then(|subject| state.history.series(subject));
                let peak = st.peaks.get(name).copied().unwrap_or((None, None));
                let mut row = json!({
                    "lane": reference,
                    "unit": name,
                    "kind": rec.kind.word(),
                    "role": rec.role,
                    "cpu_percent": latest.map(|l| l.0),
                    "memory_bytes": latest.map(|l| l.1),
                    "peak_cpu_percent": peak.1,
                    "peak_memory_bytes": peak.0,
                    "history": view.history(series),
                });
                if view.windows {
                    row["windows"] = json!(view.windows(series));
                }
                units.push(row);
            }
            let Some(record) = &st.record else { continue };
            let series = state.history.series(&Subject::Lane {
                project: project.id,
                lane: record.id,
            });
            let Some(series) = series else { continue };
            let latest = series.latest();
            let peak = st
                .started_at_ms
                .map(|started| peak_since(series, started))
                .unwrap_or_default();
            let mut row = json!({
                "lane": reference,
                "cpu_percent": latest.map(|p| p.cpu_percent),
                "memory_bytes": latest.map(|p| p.footprint_bytes),
                "peak_cpu_percent": peak.0,
                "peak_memory_bytes": peak.1,
                "history": view.history(Some(series)),
            });
            if view.windows {
                row["windows"] = json!(view.windows(Some(series)));
            }
            lanes.push(row);
        }
        units.extend(container_entries(&state, &view));
        let system = self.integ.system_pressure.is_some().then(|| {
            let reading = state.system.clone().unwrap_or_default();
            let history: Vec<Value> = state
                .system_history
                .iter()
                .filter(|(ts, _, _)| *ts >= view.since_ms)
                .map(|(ts, cpu, available)| json!([ts, cpu, available]))
                .collect();
            json!({
                "cpus": std::thread::available_parallelism().map(usize::from).ok(),
                "memory_total_bytes": state.memory_total,
                "available_memory_bytes": reading.available_memory_bytes,
                "cpu_percent": reading.cpu_percent,
                "memory_pressure": reading.memory_pressure,
                "cpu_stall_percent": reading.cpu_stall_percent,
                "history": history,
            })
        });
        let mut result = json!({"units": units, "lanes": lanes, "range_ms": range_ms});
        if let Some(system) = system {
            result["system"] = system;
        }
        Ok(result)
    }
}

/// Highest `(cpu, memory)` of a series since `started_ms`, from its raw points
/// and, for what the raw ring no longer holds, its 10 s windows.
fn peak_since(series: &Series, started_ms: u64) -> (Option<f64>, Option<u64>) {
    let mut cpu: Option<f64> = None;
    let mut memory: Option<u64> = None;
    for point in series.points().filter(|p| p.ts_ms >= started_ms) {
        cpu = Some(cpu.map_or(point.cpu_percent, |c| c.max(point.cpu_percent)));
        memory = Some(memory.map_or(point.footprint_bytes, |m| m.max(point.footprint_bytes)));
    }
    for window in series.windows().filter(|w| w.start_ms >= started_ms) {
        cpu = Some(cpu.map_or(window.cpu.max, |c| c.max(window.cpu.max)));
        memory = Some(memory.map_or(window.footprint.max, |m| m.max(window.footprint.max)));
    }
    (cpu, memory)
}

/// What one `metrics` call asks of every history.
struct View {
    since_ms: u64,
    range_ms: u64,
    /// Bucket count; `None` sends every point of the range.
    points: Option<usize>,
    /// Send `windows` too.
    windows: bool,
}

impl View {
    /// The bucket of a timestamp inside the range.
    fn bucket(&self, ts_ms: u64, points: usize) -> usize {
        let offset = ts_ms.saturating_sub(self.since_ms);
        let index =
            (u128::from(offset) * points as u128 / u128::from(self.range_ms.max(1))) as usize;
        index.min(points - 1)
    }

    /// The start of bucket `index`.
    fn bucket_start(&self, index: usize, points: usize) -> u64 {
        self.since_ms + (u128::from(self.range_ms) * index as u128 / points as u128) as u64
    }

    /// `[ts_ms, cpu, memory]` over the range: raw points while the range fits
    /// the raw ring, else 10 s windows (highest CPU, average memory). With
    /// `points`, buckets keep the highest CPU and the average memory.
    fn history(&self, series: Option<&Series>) -> Vec<Value> {
        let Some(series) = series else {
            return Vec::new();
        };
        let samples: Vec<(u64, f64, u64)> = if self.range_ms <= RAW_RANGE_MS {
            series
                .points()
                .filter(|p| p.ts_ms >= self.since_ms)
                .map(|p| (p.ts_ms, p.cpu_percent, p.footprint_bytes))
                .collect()
        } else {
            self.windows_in(series)
                .iter()
                .map(|w| (w.start_ms, w.cpu.max, w.footprint.avg))
                .collect()
        };
        let Some(points) = self.points else {
            return samples
                .into_iter()
                .map(|(ts, cpu, memory)| json!([ts, cpu, memory]))
                .collect();
        };
        let mut buckets: BTreeMap<usize, (f64, u128, u64)> = BTreeMap::new();
        for (ts, cpu, memory) in samples {
            let bucket = buckets
                .entry(self.bucket(ts, points))
                .or_insert((0.0, 0, 0));
            bucket.0 = bucket.0.max(cpu);
            bucket.1 += u128::from(memory);
            bucket.2 += 1;
        }
        buckets
            .into_iter()
            .map(|(index, (cpu, memory, count))| {
                let average = (memory / u128::from(count.max(1))) as u64;
                json!([self.bucket_start(index, points), cpu, average])
            })
            .collect()
    }

    /// Closed 10 s windows inside the range, plus the one still filling.
    fn windows_in(&self, series: &Series) -> Vec<Window> {
        series
            .windows()
            .cloned()
            .chain(series.open_window())
            .filter(|w| w.start_ms >= self.since_ms)
            .collect()
    }

    /// `[start_ms, cpu_min, cpu_avg, cpu_max, mem_min, mem_avg, mem_max]` per
    /// 10 s window of the range, merged into `points` buckets when asked.
    fn windows(&self, series: Option<&Series>) -> Vec<Value> {
        let Some(series) = series else {
            return Vec::new();
        };
        let windows = self.windows_in(series);
        let row = |start: u64, w: &Window| {
            json!([
                start,
                w.cpu.min,
                w.cpu.avg,
                w.cpu.max,
                w.footprint.min,
                w.footprint.avg,
                w.footprint.max
            ])
        };
        let Some(points) = self.points else {
            return windows.iter().map(|w| row(w.start_ms, w)).collect();
        };
        let mut buckets: BTreeMap<usize, Window> = BTreeMap::new();
        for window in windows {
            let index = self.bucket(window.start_ms, points);
            match buckets.get_mut(&index) {
                None => {
                    buckets.insert(index, window);
                }
                Some(merged) => {
                    let (a, b) = (f64::from(merged.samples), f64::from(window.samples));
                    let total = (a + b).max(1.0);
                    merged.cpu.min = merged.cpu.min.min(window.cpu.min);
                    merged.cpu.max = merged.cpu.max.max(window.cpu.max);
                    merged.cpu.avg = (merged.cpu.avg * a + window.cpu.avg * b) / total;
                    merged.footprint.min = merged.footprint.min.min(window.footprint.min);
                    merged.footprint.max = merged.footprint.max.max(window.footprint.max);
                    merged.footprint.avg = ((merged.footprint.avg as f64 * a
                        + window.footprint.avg as f64 * b)
                        / total) as u64;
                    merged.samples = merged.samples.saturating_add(window.samples);
                }
            }
        }
        buckets
            .iter()
            .map(|(index, w)| row(self.bucket_start(*index, points), w))
            .collect()
    }
}

/// Running infra containers as `metrics` rows with `kind: container`, grouped
/// under their lane or `<project> (shared)`. Replicas of one service are told
/// apart by their container name.
fn container_entries(state: &MetricsState, view: &View) -> Vec<Value> {
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
            let series = state.history.series(subject);
            let mut entry = json!({
                "lane": row.group,
                "unit": unit,
                "kind": "container",
                "container": row.docker,
                "shared": row.shared,
                "lanes": row.lanes,
                "cpu_percent": row.latest.map(|l| l.0),
                "memory_bytes": row.latest.map(|l| l.1),
                "history": view.history(series),
            });
            if view.windows {
                entry["windows"] = json!(view.windows(series));
            }
            entry
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
