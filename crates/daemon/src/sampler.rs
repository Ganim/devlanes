//! Resource sampling (plan 13.5). Ticks every second while a client watches
//! (subscribers or recent `metrics` calls), and every five seconds otherwise while
//! units run, so CI summaries still get per-start peaks. Monitoring only.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use devlanes_core::{LaneId, ProcessIdentity, UnitRunId, gate::MemoryPressure};
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
    /// Row labels of unit, lane and container series as of their last sample,
    /// so `metrics` can still send rows for what stopped or went away. Keyed
    /// by series, not by lane reference, because a
    /// cleaned lane's reference can come back on a new lane. Pruned with the
    /// series.
    pub labels: BTreeMap<Subject, Label>,
    /// Unit, lane and container subjects that got a sample in the latest tick.
    pub sampled: BTreeSet<Subject>,
    pub interest_until: Option<Instant>,
    /// When the sampler last pruned (Unix ms).
    pub last_prune_ms: u64,
    /// The machine as of the last tick, for `metrics.system`.
    pub system: Option<SystemReading>,
    /// One [`SystemPoint`] per tick, newest last, at most [`RAW_CAPACITY`]
    /// entries.
    pub system_history: VecDeque<SystemPoint>,
    /// Physical memory, read once.
    pub memory_total: Option<u64>,
    /// The daemon's own process, set by the first tick that could identify it.
    pub daemon: Option<DaemonProcess>,
}

/// `(ts_ms, cpu_percent, available_memory_bytes, memory_pressure)` of one tick.
pub(crate) type SystemPoint = (u64, Option<f64>, Option<u64>, Option<&'static str>);

/// The daemon's own process, measured on every tick under [`Subject::Daemon`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct DaemonProcess {
    pub identity: ProcessIdentity,
    /// When the engine started (Unix ms).
    pub started_at_ms: u64,
    /// `(cpu, memory)` from the last tick, `None` when that tick could not read it.
    pub latest: Option<(f64, u64)>,
    /// Highest `(cpu, memory)` of every tick so far, each maximised on its own.
    /// Kept apart from the history, which forgets ticks older than its horizon.
    pub peak: Option<(f64, u64)>,
}

impl DaemonProcess {
    /// Stores one tick's reading as [`Self::latest`] and raises [`Self::peak`]
    /// with it.
    fn observe(&mut self, reading: Option<(f64, u64)>) {
        self.latest = reading;
        if let Some((cpu, memory)) = reading {
            self.peak = Some(self.peak.map_or((cpu, memory), |(peak_cpu, peak_memory)| {
                (peak_cpu.max(cpu), peak_memory.max(memory))
            }));
        }
    }
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
    /// Busy share of each CPU, 0-100; empty when the platform cannot tell.
    pub core_percents: Vec<f64>,
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
        self.labels
            .retain(|subject, _| history.series(subject).is_some());
    }
}

/// What a `metrics` row of one series shows besides its readings, as of the
/// series' last sample.
#[derive(Debug, Clone)]
pub(crate) enum Label {
    Unit {
        /// Lane reference.
        lane: String,
        unit: String,
        /// [`crate::model::Kind::word`].
        kind: &'static str,
        role: &'static str,
    },
    Lane {
        /// Lane reference.
        lane: String,
    },
    /// The container's row with `latest` cleared, so a gone container never
    /// reports a reading.
    Container(ContainerRow),
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
    /// [`crate::model::Kind::word`], for the unit's [`Label`].
    kind: &'static str,
    role: &'static str,
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

    /// Samples every live unit and the daemon itself once and updates history,
    /// peaks and rollups.
    pub(crate) async fn sample_once(self: &Arc<Self>) {
        let mut targets = Vec::new();
        let mut request = SampleRequest {
            daemon: self.daemon_identity().await,
            ..SampleRequest::default()
        };
        let mut containers: BTreeMap<Subject, ContainerRow> = BTreeMap::new();
        // Lane series with their references, for the lanes' labels.
        let mut lane_refs: Vec<(Subject, String)> = Vec::new();
        for (project, lane) in self.all_lanes() {
            let project_name = project.name();
            let (reference, lane_id, units) = {
                let st = lane.st();
                let Some(record) = &st.record else { continue };
                let units: Vec<_> = st
                    .units
                    .iter()
                    .filter(|(_, rec)| rec.host.is_alive())
                    .map(|(name, rec)| {
                        (
                            name.clone(),
                            rec.host.clone(),
                            rec.run_id,
                            rec.kind.word(),
                            rec.role,
                        )
                    })
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
                lane_refs.push((
                    Subject::Lane {
                        project: project.id,
                        lane: record.id,
                    },
                    reference.clone(),
                ));
                (reference, record.id, units)
            };
            for (name, host, run, kind, role) in units {
                let key = UnitKey {
                    project: project.id,
                    lane: lane_id,
                    unit: name.clone(),
                };
                let identity = host.identity();
                let processes = host.subjects().await;
                request
                    .units
                    .push(unit_target(key.clone(), identity, processes));
                targets.push(Target {
                    lane: lane.clone(),
                    reference: reference.clone(),
                    name,
                    run,
                    key,
                    pid: identity.map(|identity| identity.pid),
                    kind,
                    role,
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
        let daemon_samples: Vec<Sample> = leaves
            .iter()
            .filter(|sample| sample.subject == Subject::Daemon)
            .cloned()
            .collect();
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
        let fresh: BTreeSet<Subject> = totals
            .iter()
            .map(|sample| &sample.subject)
            .filter(|subject| matches!(subject, Subject::Unit(_) | Subject::Lane { .. }))
            .chain(container_samples.iter().map(|sample| &sample.subject))
            .cloned()
            .collect();
        // Labels only for series that got a point, so each one has a series to
        // be pruned with and shows what its newest point was taken under.
        let mut labels: Vec<(Subject, Label)> = Vec::new();
        for target in &targets {
            let subject = Subject::Unit(target.key.clone());
            if fresh.contains(&subject) {
                labels.push((
                    subject,
                    Label::Unit {
                        lane: target.reference.clone(),
                        unit: target.name.clone(),
                        kind: target.kind,
                        role: target.role,
                    },
                ));
            }
        }
        for (subject, reference) in lane_refs {
            if fresh.contains(&subject) {
                labels.push((subject, Label::Lane { lane: reference }));
            }
        }
        for (subject, row) in &containers {
            if row.latest.is_some() {
                let row = ContainerRow {
                    latest: None,
                    ..row.clone()
                };
                labels.push((subject.clone(), Label::Container(row)));
            }
        }
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
                    reading.memory_pressure,
                ));
                state.system = Some(reading);
            }
            state.history.record_all(&totals);
            state.history.record_all(&container_samples);
            state.history.record_all(&daemon_samples);
            if let Some(daemon) = &mut state.daemon {
                daemon.observe(
                    daemon_samples
                        .last()
                        .map(|sample| (sample.cpu_percent, sample.footprint_bytes)),
                );
            }
            state.containers = containers;
            state.labels.extend(labels);
            state.sampled = fresh;
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
                core_percents: source.core_percents(),
            },
            total,
        ))
    }

    /// The daemon's own process identity, read on the first call and kept in
    /// [`MetricsState::daemon`]. `None` while the process table cannot be read.
    async fn daemon_identity(&self) -> Option<ProcessIdentity> {
        let known = lock(&self.metrics).daemon.map(|daemon| daemon.identity);
        if known.is_some() {
            return known;
        }
        let identity = tokio::task::spawn_blocking(|| devlanes_sys::identity(std::process::id()))
            .await
            .ok()?
            .ok()??;
        // `uptime_ms` in `status` counts from the same instant.
        let uptime = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut state = lock(&self.metrics);
        // A concurrent tick may have stored it first; keep that one.
        let daemon = state.daemon.get_or_insert(DaemonProcess {
            identity,
            started_at_ms: now_ms().saturating_sub(uptime),
            latest: None,
            peak: None,
        });
        Some(daemon.identity)
    }

    /// Adds the latest CPU and memory to a lane summary's units.
    pub(crate) fn fill_metrics(&self, summary: &mut LaneSummary) {
        let state = lock(&self.metrics);
        for unit in &mut summary.units {
            if unit.state == "idle" || unit.state == "stopped" || unit.state == "exited" {
                continue;
            }
            let key = (summary.reference.clone(), unit.name.clone());
            // `latest` outlives a unit that stopped (it is no target any more,
            // so nothing clears it) and a cleaned lane whose reference a new
            // lane took; only a sample of the latest tick is current.
            let current = state
                .subjects
                .get(&key)
                .is_some_and(|subject| state.sampled.contains(subject));
            if let Some((cpu, memory)) = state.latest.get(&key).filter(|_| current) {
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

    /// `metrics`: per-unit samples and history, lane totals, the machine and the
    /// daemon's own process, or `peaks: true` for the current lane start. The
    /// reply shape is documented on [`devlanes_proto::Method::Metrics`].
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
        // Series already sent, so no remembered row repeats one.
        let mut sent: BTreeSet<Subject> = BTreeSet::new();
        // Lanes with a record now; remembered rows of any other lane are `removed`.
        let mut current: BTreeSet<LaneId> = BTreeSet::new();
        for (project, lane) in self.all_lanes() {
            let st = lane.st();
            let reference = st.reference(&project.name());
            let lane_id = st.record.as_ref().map(|record| record.id);
            current.extend(lane_id);
            for (name, rec) in &st.units {
                let alive = rec.host.is_alive();
                // From the lane's id rather than `subjects`, which is keyed by
                // reference and can still point at a cleaned lane's series when
                // a new lane took its reference. Without a record the unit was
                // never sampled and has no series.
                let subject = lane_id.map(|lane| {
                    Subject::Unit(UnitKey {
                        project: project.id,
                        lane,
                        unit: name.clone(),
                    })
                });
                let series = subject
                    .as_ref()
                    .and_then(|subject| state.history.series(subject));
                if !alive && !view.holds(series) {
                    continue;
                }
                // `latest` is keyed by reference too; a reading of the latest
                // tick for this series is the only one that belongs to it.
                let latest = subject
                    .as_ref()
                    .filter(|subject| alive && state.sampled.contains(subject))
                    .and_then(|_| state.latest.get(&(reference.clone(), name.clone())))
                    .copied();
                let peak = st.peaks.get(name).copied().unwrap_or((None, None));
                let mut row = json!({
                    "lane": reference,
                    "unit": name,
                    "kind": rec.kind.word(),
                    "role": rec.role,
                    "series": subject.as_ref().map(Subject::key),
                    "ended": !alive,
                    "removed": false,
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
                sent.extend(subject);
            }
            let Some(lane_id) = lane_id else { continue };
            let subject = Subject::Lane {
                project: project.id,
                lane: lane_id,
            };
            let Some(series) = state.history.series(&subject) else {
                continue;
            };
            // The newest point stays for hours once nothing in the lane runs;
            // only a sample of the latest tick is a current reading.
            let fresh = state.sampled.contains(&subject);
            let latest = series.latest().filter(|_| fresh);
            let peak = st
                .started_at_ms
                .map(|started| peak_since(series, started))
                .unwrap_or_default();
            let mut row = json!({
                "lane": reference,
                "series": subject.key(),
                "ended": !fresh,
                "removed": false,
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
            sent.insert(subject);
        }
        let (ended_units, ended_lanes) = remembered_entries(&state, &view, &sent, &current);
        units.extend(ended_units);
        lanes.extend(ended_lanes);
        units.extend(container_entries(&state, &view, &current));
        let system = self.integ.system_pressure.is_some().then(|| {
            let reading = state.system.clone().unwrap_or_default();
            let history: Vec<Value> = state
                .system_history
                .iter()
                .filter(|(ts, ..)| *ts >= view.since_ms)
                .map(|(ts, cpu, available, pressure)| {
                    json!([ts, cpu, available, pressure.and_then(pressure_level)])
                })
                .collect();
            json!({
                "cpus": std::thread::available_parallelism().map(usize::from).ok(),
                "memory_total_bytes": state.memory_total,
                "available_memory_bytes": reading.available_memory_bytes,
                "cpu_percent": reading.cpu_percent,
                "memory_pressure": reading.memory_pressure,
                "cpu_stall_percent": reading.cpu_stall_percent,
                "core_percents": reading.core_percents,
                "history": history,
            })
        });
        let mut result = json!({"units": units, "lanes": lanes, "range_ms": range_ms});
        if let Some(system) = system {
            result["system"] = system;
        }
        if let Some(daemon) = daemon_entry(&state, &view) {
            result["daemon"] = daemon;
        }
        Ok(result)
    }
}

/// The `metrics` reply's `daemon` object; `None` until a tick has read the
/// daemon's own process.
fn daemon_entry(state: &MetricsState, view: &View) -> Option<Value> {
    let daemon = state.daemon?;
    let series = state.history.series(&Subject::Daemon)?;
    let mut entry = json!({
        "pid": daemon.identity.pid,
        "started_at_ms": daemon.started_at_ms,
        "cpu_percent": daemon.latest.map(|l| l.0),
        "memory_bytes": daemon.latest.map(|l| l.1),
        "peak_cpu_percent": daemon.peak.map(|p| p.0),
        "peak_memory_bytes": daemon.peak.map(|p| p.1),
        "history": view.history(Some(series)),
    });
    if view.windows {
        entry["windows"] = json!(view.windows(Some(series)));
    }
    Some(entry)
}

/// The `metrics` system history's pressure level for a
/// [`MemoryPressure::word`]: 0 normal, 1 warn, 2 critical.
fn pressure_level(word: &str) -> Option<usize> {
    // Matching through `word()` keeps this in step with what `read_system` stores.
    [
        MemoryPressure::Normal,
        MemoryPressure::Warn,
        MemoryPressure::Critical,
    ]
    .iter()
    .position(|level| level.word() == word)
}

/// What to measure for one unit: the leader's session, the leader and the
/// runner's tracked processes.
#[cfg(unix)]
fn unit_target(
    unit: UnitKey,
    identity: Option<ProcessIdentity>,
    mut processes: Vec<ProcessIdentity>,
) -> UnitTarget {
    if let Some(identity) = identity
        && !processes.iter().any(|p| p.pid == identity.pid)
    {
        processes.push(identity);
    }
    UnitTarget {
        unit,
        sessions: identity.map(|i| vec![i.sid]).unwrap_or_default(),
        processes,
    }
}

/// What to measure for one unit: exactly the job members the runner reports.
/// Windows has no sessions, and the leader is a member while it runs; once it
/// has exited, its identity names nothing left to measure.
#[cfg(windows)]
fn unit_target(
    unit: UnitKey,
    _identity: Option<ProcessIdentity>,
    processes: Vec<ProcessIdentity>,
) -> UnitTarget {
    UnitTarget {
        unit,
        sessions: Vec::new(),
        processes,
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
    /// Whether the series' newest raw point is inside the range. Rows of what
    /// stopped or went away are sent only while this holds, so a graph keeps
    /// their past until it leaves the range, and no row comes back with an
    /// empty history.
    fn holds(&self, series: Option<&Series>) -> bool {
        series
            .and_then(Series::latest)
            .is_some_and(|point| point.ts_ms >= self.since_ms)
    }

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

/// Whether a remembered series belongs to a lane that has no record now
/// (`current` holds the ids that do). Shared containers have no lane and never
/// count as removed.
fn lane_removed(subject: &Subject, current: &BTreeSet<LaneId>) -> bool {
    subject.lane().is_some_and(|lane| !current.contains(&lane))
}

/// Ended `metrics` rows for remembered unit and lane series that are not in
/// `sent` and whose newest point is inside the range, as `(units, lanes)`.
/// They carry the labels of their last sample and no readings or peaks.
fn remembered_entries(
    state: &MetricsState,
    view: &View,
    sent: &BTreeSet<Subject>,
    current: &BTreeSet<LaneId>,
) -> (Vec<Value>, Vec<Value>) {
    let mut units = Vec::new();
    let mut lanes = Vec::new();
    for (subject, label) in &state.labels {
        if sent.contains(subject) {
            continue;
        }
        let series = state.history.series(subject);
        if !view.holds(series) {
            continue;
        }
        // Units still in an existing lane's units went out above with its
        // `peaks`; these belong to a gone lane or are no longer in its units.
        let mut row = match label {
            Label::Unit {
                lane,
                unit,
                kind,
                role,
            } => json!({
                "lane": lane,
                "unit": unit,
                "kind": kind,
                "role": role,
                "peak_cpu_percent": null,
                "peak_memory_bytes": null,
            }),
            Label::Lane { lane } => json!({
                "lane": lane,
                "peak_cpu_percent": null,
                "peak_memory_bytes": null,
            }),
            Label::Container(_) => continue,
        };
        row["series"] = json!(subject.key());
        row["ended"] = json!(true);
        row["removed"] = json!(lane_removed(subject, current));
        row["cpu_percent"] = Value::Null;
        row["memory_bytes"] = Value::Null;
        row["history"] = json!(view.history(series));
        if view.windows {
            row["windows"] = json!(view.windows(series));
        }
        match label {
            Label::Lane { .. } => lanes.push(row),
            _ => units.push(row),
        }
    }
    (units, lanes)
}

/// Infra containers as `metrics` rows with `kind: container`, grouped under
/// their lane or `<project> (shared)`: those of the last tick, then remembered
/// ones that are no longer listed. Containers without a reading this tick are
/// `ended` and sent only while their newest point is inside the range.
/// Replicas of one service are told apart by their container name.
fn container_entries(state: &MetricsState, view: &View, current: &BTreeSet<LaneId>) -> Vec<Value> {
    let gone = state
        .labels
        .iter()
        .filter_map(|(subject, label)| match label {
            Label::Container(row) if !state.containers.contains_key(subject) => {
                Some((subject, row))
            }
            _ => None,
        });
    let rows: Vec<(&Subject, &ContainerRow)> = state
        .containers
        .iter()
        .chain(gone)
        .filter(|(subject, row)| row.latest.is_some() || view.holds(state.history.series(subject)))
        .collect();
    rows.iter()
        .map(|(subject, row)| {
            // Same lane id too: a cleaned lane's container and the one of the
            // new lane that took its reference share group and name, and are
            // no replicas of each other.
            let replicas = rows
                .iter()
                .filter(|(other_subject, other)| {
                    other.group == row.group
                        && other.name == row.name
                        && other_subject.lane() == subject.lane()
                })
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
                "series": subject.key(),
                "ended": row.latest.is_none(),
                "removed": lane_removed(subject, current),
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
        state.labels.insert(
            sample.subject.clone(),
            Label::Unit {
                lane: key.0.clone(),
                unit: key.1.clone(),
                kind: "service",
                role: "service",
            },
        );
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
        // A label goes with its series, even while the unit is still sampled.
        let labelled: Vec<&Subject> = state.labels.keys().collect();
        assert_eq!(labelled, [&unit_sample("recent", 0).subject]);
        // Within a minute of the last pass nothing is walked again.
        track(&mut state, "gone", 0);
        state.prune(now + 1_000, &live);
        assert_eq!(state.subjects.len(), 3);
        assert_eq!(state.labels.len(), 2);
    }

    /// A sample of `subject` at `ts_ms`.
    fn sample(subject: Subject, ts_ms: u64) -> Sample {
        Sample {
            subject,
            cpu_percent: 1.0,
            footprint_bytes: 1,
            rss_bytes: 1,
            ts_ms,
        }
    }

    /// A unit subject of project 1.
    fn unit_in(lane: i64, unit: &str) -> Subject {
        Subject::Unit(UnitKey {
            project: ProjectId(1),
            lane: LaneId(lane),
            unit: unit.into(),
        })
    }

    /// The view of the last 10 s before `now`.
    fn last_ten_seconds(now: u64) -> View {
        View {
            since_ms: now - 10_000,
            range_ms: 10_000,
            points: None,
            windows: false,
        }
    }

    /// A row's `(lane, unit, series, removed)`.
    fn summary(row: &Value) -> (String, String, String, bool) {
        (
            row["lane"].as_str().unwrap_or_default().to_owned(),
            row["unit"].as_str().unwrap_or_default().to_owned(),
            row["series"].as_str().unwrap_or_default().to_owned(),
            row["removed"].as_bool().unwrap_or_default(),
        )
    }

    /// A series is in range while its newest point is, however old its
    /// first one.
    #[test]
    fn a_series_is_in_range_by_its_newest_point() {
        let view = last_ten_seconds(100_000);
        let mut history = History::default();
        let subject = unit_in(1, "api");
        assert!(!view.holds(None));
        history.record(&sample(subject.clone(), 50_000));
        assert!(!view.holds(history.series(&subject)));
        history.record(&sample(subject.clone(), 90_000));
        assert!(view.holds(history.series(&subject)), "exactly at the edge");
    }

    /// Remembered units and lanes go out as ended rows from their labels while
    /// they are in range and not sent yet; `removed` when their lane has no
    /// record. A reused reference keeps two rows apart by series.
    #[test]
    fn remembered_rows_follow_the_range_and_the_current_lanes() {
        let now = 100_000;
        let mut state = MetricsState::default();
        let unit_label = |lane: &str, unit: &str| Label::Unit {
            lane: lane.into(),
            unit: unit.into(),
            kind: "service",
            role: "service",
        };
        // Lane 2 was `acme@1` before it was cleaned; lane 3 has that reference now.
        for (subject, label, ts) in [
            (unit_in(2, "api"), unit_label("acme@1", "api"), 95_000),
            (unit_in(3, "api"), unit_label("acme@1", "api"), 99_000),
            (unit_in(1, "old"), unit_label("acme@0", "old"), 80_000),
            (unit_in(1, "gone"), unit_label("acme@0", "gone"), 95_000),
            (
                Subject::Lane {
                    project: ProjectId(1),
                    lane: LaneId(2),
                },
                Label::Lane {
                    lane: "acme@1".into(),
                },
                95_000,
            ),
        ] {
            state.history.record(&sample(subject.clone(), ts));
            state.labels.insert(subject, label);
        }
        let sent = BTreeSet::from([unit_in(3, "api")]);
        let current = BTreeSet::from([LaneId(1), LaneId(3)]);
        let (units, lanes) = remembered_entries(&state, &last_ten_seconds(now), &sent, &current);
        let units: Vec<_> = units.iter().map(summary).collect();
        assert_eq!(
            units,
            [
                (
                    "acme@0".into(),
                    "gone".into(),
                    "unit:1:1:gone".into(),
                    false
                ),
                ("acme@1".into(), "api".into(), "unit:1:2:api".into(), true),
            ]
        );
        assert_eq!(lanes.len(), 1);
        let lane = &lanes[0];
        assert_eq!(
            (&lane["series"], &lane["removed"], &lane["ended"]),
            (&json!("lane:1:2"), &json!(true), &json!(true))
        );
        assert!(lane["cpu_percent"].is_null() && lane["peak_cpu_percent"].is_null());
        assert_eq!(lane["history"].as_array().map(Vec::len), Some(1));
    }

    /// A container row of `group`.
    fn container_row(
        name: &str,
        docker: &str,
        group: &str,
        latest: Option<(f64, u64)>,
    ) -> ContainerRow {
        ContainerRow {
            name: name.into(),
            docker: docker.into(),
            shared: group.ends_with("(shared)"),
            group: group.into(),
            lanes: vec!["acme@0".into()],
            latest,
        }
    }

    /// Listed containers with a reading are live; listed ones without and
    /// gone ones are ended while in range, gone per-lane ones of a lane
    /// without a record are `removed`, and ended rows count as replicas only
    /// within their own lane.
    #[test]
    fn container_rows_keep_ended_and_gone_containers_in_range() {
        let now = 100_000;
        let container = |lane: Option<i64>, id: &str| Subject::Container {
            project: ProjectId(1),
            lane: lane.map(LaneId),
            id: id.into(),
        };
        let mut state = MetricsState::default();
        let listed = [
            (
                container(Some(1), "db-1"),
                container_row("db", "db-1", "acme@0", Some((1.0, 1))),
            ),
            (
                container(Some(1), "db-2"),
                container_row("db", "db-2", "acme@0", None),
            ),
            (
                container(Some(1), "old-1"),
                container_row("old", "old-1", "acme@0", None),
            ),
            // Lane 3 took lane 2's reference after lane 2 was cleaned.
            (
                container(Some(3), "cache-1"),
                container_row("cache", "cache-1", "acme@1", Some((1.0, 1))),
            ),
        ];
        for (subject, row) in listed {
            let ts = if row.name == "old" { 50_000 } else { 99_000 };
            state.history.record(&sample(subject.clone(), ts));
            state.containers.insert(subject, row);
        }
        let gone = [
            (
                container(Some(2), "cache-1"),
                container_row("cache", "cache-1", "acme@1", None),
            ),
            (
                container(None, "shared-1"),
                container_row("shared", "shared-1", "acme (shared)", None),
            ),
        ];
        for (subject, row) in gone {
            state.history.record(&sample(subject.clone(), 95_000));
            state.labels.insert(subject, Label::Container(row));
        }
        let current = BTreeSet::from([LaneId(1), LaneId(3)]);
        let rows = container_entries(&state, &last_ten_seconds(now), &current);
        let rows: Vec<(String, String, bool, bool)> = rows
            .iter()
            .map(|row| {
                let (lane, unit, _, removed) = summary(row);
                (
                    lane,
                    unit,
                    row["ended"].as_bool().unwrap_or_default(),
                    removed,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("acme@0".into(), "db-1".into(), false, false),
                ("acme@0".into(), "db-2".into(), true, false),
                // Not replicas of the cleaned lane's `cache`: both keep the name.
                ("acme@1".into(), "cache".into(), false, false),
                ("acme (shared)".into(), "shared".into(), true, false),
                ("acme@1".into(), "cache".into(), true, true),
            ]
        );
    }

    /// Every pressure word maps to its wire level; anything else has none.
    #[test]
    fn pressure_levels_follow_the_words() {
        let levels: Vec<Option<usize>> = [
            MemoryPressure::Normal,
            MemoryPressure::Warn,
            MemoryPressure::Critical,
        ]
        .iter()
        .map(|level| pressure_level(level.word()))
        .collect();
        assert_eq!(levels, [Some(0), Some(1), Some(2)]);
        assert_eq!(pressure_level("unknown"), None);
    }

    /// The `daemon` entry waits for the first reading and carries the peaks of
    /// every tick, not only those the history still holds.
    #[test]
    fn daemon_entry_needs_a_reading() {
        let mut state = MetricsState::default();
        let view = View {
            since_ms: 0,
            range_ms: RAW_RANGE_MS,
            points: None,
            windows: false,
        };
        assert!(daemon_entry(&state, &view).is_none());
        let mut daemon = DaemonProcess {
            identity: process(42, 42),
            started_at_ms: 2_000,
            latest: None,
            peak: None,
        };
        // CPU and memory peak on different ticks; a tick without a reading
        // clears `latest` and keeps the peaks.
        for reading in [Some((90.0, 300)), None, Some((1.5, 900)), Some((1.5, 300))] {
            daemon.observe(reading);
        }
        state.daemon = Some(daemon);
        assert!(daemon_entry(&state, &view).is_none(), "no series yet");
        state.history.record(&Sample {
            subject: Subject::Daemon,
            cpu_percent: 1.5,
            footprint_bytes: 300,
            rss_bytes: 300,
            ts_ms: 3_000,
        });
        let entry = daemon_entry(&state, &view).expect("entry");
        assert_eq!(entry["pid"], 42);
        assert_eq!(entry["started_at_ms"], 2_000);
        assert_eq!(
            (&entry["cpu_percent"], &entry["memory_bytes"]),
            (&json!(1.5), &json!(300))
        );
        assert_eq!(
            (&entry["peak_cpu_percent"], &entry["peak_memory_bytes"]),
            (&json!(90.0), &json!(900)),
            "peaks outlast the history"
        );
        assert_eq!(entry["history"].as_array().map(Vec::len), Some(1));
        assert!(entry.get("windows").is_none());
    }

    /// The unit key every target test uses.
    fn web() -> UnitKey {
        UnitKey {
            project: ProjectId(1),
            lane: LaneId(1),
            unit: "web".into(),
        }
    }

    /// A process identity with a recognisable start time.
    fn process(pid: u32, sid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            sid,
            start_time: u64::from(pid) * 10,
        }
    }

    /// Unix measures the leader's session and adds the leader when the runner
    /// no longer lists it.
    #[cfg(unix)]
    #[test]
    fn unit_target_adds_the_leader_and_its_session() {
        let leader = process(10, 10);
        let child = process(11, 10);
        let target = unit_target(web(), Some(leader), vec![child]);
        assert_eq!(target.sessions, [10]);
        assert_eq!(target.processes, [child, leader]);
        let target = unit_target(web(), Some(leader), vec![leader, child]);
        assert_eq!(target.processes, [leader, child]);
        let target = unit_target(web(), None, vec![child]);
        assert!(target.sessions.is_empty());
        assert_eq!(target.processes, [child]);
    }

    /// Windows measures the job members the runner reports and nothing else,
    /// so an exited leader is not added back.
    #[cfg(windows)]
    #[test]
    fn unit_target_is_the_job_members() {
        let leader = process(8, 0);
        let member = process(12, 0);
        let target = unit_target(web(), Some(leader), vec![member]);
        assert!(target.sessions.is_empty());
        assert_eq!(target.processes, [member]);
        let target = unit_target(web(), Some(leader), vec![leader, member]);
        assert_eq!(target.processes, [leader, member]);
        assert_eq!(target.unit, web());
    }
}
