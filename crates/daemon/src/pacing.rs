//! Startup pacing: every service start (replicas and restarts included) waits
//! for a slot from the machine-wide start gate before it spawns, and gives the
//! slot back once its startup is over. Waiting units show as `queued` in
//! `status` and `why`. Pacing only delays spawns; running processes are never
//! limited.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use devlanes_core::{
    WorktreeId,
    gate::{
        DEFAULT_MEMORY_NEED, GateSnapshot, QueuedStart, Release, StartRequest, StartSlot,
        StartTicket,
    },
};
use devlanes_metrics::{Subject, UnitKey};
use devlanes_proto::{Actor, LaneSummary, RpcError, UnitFailureData, events};
use devlanes_resolve::{Ready, ReadySpec, ResolvedLane, ResolvedService};
use devlanes_state::StartupPeak;
use futures_util::future::join_all;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    engine::Inner,
    lifecycle::{SpawnRequest, StartCx, readiness},
    model::{LaneRt, ProjectRt, is_mine, lock},
    ready::run_check,
    registry::internal,
    sampler::CpuReading,
    units::{UnitHost, UnitState},
};

/// How often a slot holder looks at its unit: CPU reports for the settle rule
/// and noticing a gate timeout.
const HOLD_POLL: Duration = Duration::from_secs(1);
/// Keeps the sampler on its one-second tick while a slot is held, for CPU
/// settle and memory reports.
const SAMPLER_INTEREST: Duration = Duration::from_secs(3);
/// Past runs (or startups) per unit used for baselines.
const BASELINE_RUNS: u32 = 5;
/// A startup peak covers at most this much time after the spawn.
const STARTUP_WINDOW: Duration = Duration::from_secs(300);

/// A queued start that `stop` can cancel.
struct Waiting {
    id: u64,
    token: CancellationToken,
    /// Lane reference at enqueue time, matching the gate's request.
    reference: String,
    actor: Actor,
}

/// Queued starts by lane and unit, so `stop` can withdraw them.
#[derive(Default)]
pub(crate) struct Pacing {
    waiting: Mutex<HashMap<(WorktreeId, String), Waiting>>,
    next: AtomicU64,
    /// Set once the "cannot measure settle" message was logged.
    unmeasured_logged: AtomicBool,
}

impl Pacing {
    /// Registers a queued start; the token fires when `stop` withdraws it.
    fn register(
        &self,
        lane: WorktreeId,
        reference: &str,
        unit: &str,
        actor: &Actor,
    ) -> (u64, CancellationToken) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        let previous = lock(&self.waiting).insert(
            (lane, unit.to_owned()),
            Waiting {
                id,
                token: token.clone(),
                reference: reference.to_owned(),
                actor: actor.clone(),
            },
        );
        // A second start of the same unit replaces the first in the queue.
        if let Some(previous) = previous {
            previous.token.cancel();
        }
        (id, token)
    }

    /// Forgets a start once it left the queue; a newer registration stays.
    fn unregister(&self, lane: WorktreeId, unit: &str, id: u64) {
        let mut waiting = lock(&self.waiting);
        let key = (lane, unit.to_owned());
        if waiting.get(&key).is_some_and(|entry| entry.id == id) {
            waiting.remove(&key);
        }
    }

    /// Withdraws every queued start (daemon shutdown).
    pub(crate) fn cancel_all(&self) {
        for (_, entry) in lock(&self.waiting).drain() {
            entry.token.cancel();
        }
    }
}

/// Everything one paced spawn task needs.
struct PacedStart {
    project: Arc<ProjectRt>,
    lane: Arc<LaneRt>,
    reference: String,
    request: SpawnRequest,
    ready: Ready,
    ticket: Option<StartTicket>,
    /// Registration in [`Pacing`].
    id: u64,
    token: CancellationToken,
    /// Closes once the previous unit of the same request spawned or gave up.
    after: Option<oneshot::Receiver<()>>,
    /// Dropped once this unit spawned or gave up.
    done: oneshot::Sender<()>,
    /// Metrics series of the unit, where its startup peak is recorded.
    series: Option<String>,
}

/// A granted slot and what its holder task watches.
struct Hold {
    host: UnitHost,
    slot: StartSlot,
    /// `ready: started`: hold until the tree CPU settles.
    started: bool,
    /// `(lane reference, unit)` in the sampler's maps.
    key: (String, String),
    series: Option<String>,
}

/// Which history a baseline comes from.
#[derive(Debug, Clone, Copy)]
enum Baseline {
    /// Peak footprint while starting: the memory need of a start.
    Startup,
    /// Peak footprint over whole runs: what a lane usually peaks at.
    Run,
}

/// Median of `values`; `None` when empty.
fn median(mut values: Vec<u64>) -> Option<u64> {
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

/// How one paced start ended, as seen by the start request.
enum Paced {
    Spawned,
    Failed(RpcError),
    /// `stop` withdrew it while it was queued.
    Cancelled,
}

impl Inner {
    /// Spawns `services` through the start gate in declaration order and waits
    /// until each spawned or the start's deadline passed. Units still queued at
    /// the deadline keep their place and spawn later; they count as started and
    /// the start reports a timeout.
    ///
    /// The request's place in line is taken when its services phase begins,
    /// before the baseline lookups. Earlier phases (infra, setup, tasks) run
    /// before that, so a request that arrived earlier but has slow infra can
    /// see a later request's services go first.
    pub(crate) async fn spawn_services(
        self: &Arc<Self>,
        cx: &mut StartCx,
        services: &[&ResolvedService],
        resolved: &ResolvedLane,
    ) {
        if services.is_empty() {
            return;
        }
        let gate = self.integ.start_gate.clone().filter(|_| !cx.all_at_once);
        let arrival = gate.as_ref().map(|gate| gate.arrive());
        let series = self.unit_series(cx, services);
        let (needs, run_peaks) = if gate.is_some() || self.integ.pressure.is_some() {
            tokio::join!(
                self.baselines(&series, Baseline::Startup),
                self.baselines(&series, Baseline::Run),
            )
        } else {
            (HashMap::new(), HashMap::new())
        };
        self.warn_memory(cx, &run_peaks).await;
        let mut ordered: Vec<(usize, &ResolvedService)> = services
            .iter()
            .map(|service| {
                let order = resolved
                    .services
                    .get_index_of(&service.unit)
                    .unwrap_or(usize::MAX);
                (order, *service)
            })
            .collect();
        ordered.sort_by_key(|(order, _)| *order);
        let mut tasks = Vec::new();
        // Gated spawns of one request run in file order: each waits for the
        // previous one, which the gate granted no later.
        let mut after: Option<oneshot::Receiver<()>> = None;
        // The whole request is one batch, queued before anything spawns: the
        // gate serves it after earlier requests and in file order within it.
        let mut tickets: Vec<Option<StartTicket>> = match (&gate, arrival) {
            (Some(gate), Some(arrival)) => gate
                .enqueue_batch_at(
                    arrival,
                    ordered
                        .iter()
                        .map(|(order, service)| StartRequest {
                            lane: cx.reference.clone(),
                            unit: service.unit.clone(),
                            order: *order,
                            memory_need: needs
                                .get(&service.unit)
                                .copied()
                                .unwrap_or(DEFAULT_MEMORY_NEED),
                        })
                        .collect(),
                )
                .into_iter()
                .map(Some)
                .collect(),
            _ => Vec::new(),
        };
        tickets.resize_with(ordered.len(), || None);
        for ((_, service), ticket) in ordered.into_iter().zip(tickets) {
            let (id, token) =
                self.pacing
                    .register(cx.lane.worktree, &cx.reference, &service.unit, &cx.actor);
            let (done, next) = oneshot::channel();
            let start = PacedStart {
                project: cx.project.clone(),
                lane: cx.lane.clone(),
                reference: cx.reference.clone(),
                request: SpawnRequest::service(service, &cx.actor),
                ready: service.ready.clone(),
                ticket,
                id,
                token,
                after: if gate.is_some() { after.take() } else { None },
                done,
                series: series.get(&service.unit).cloned(),
            };
            after = Some(next);
            let engine = self.clone();
            tasks.push((
                service.unit.clone(),
                tokio::spawn(async move { engine.start_one(start).await }),
            ));
        }
        if gate.is_some() {
            self.emit_lane(&cx.project, &cx.lane);
        }
        for (unit, mut task) in tasks {
            match tokio::time::timeout_at(cx.deadline, &mut task).await {
                Ok(Ok(Paced::Spawned)) => cx.started.push(unit),
                Ok(Ok(Paced::Failed(error))) => cx.failures.push(UnitFailureData {
                    lane: cx.reference.clone(),
                    unit,
                    reason: Some(error),
                    ..UnitFailureData::default()
                }),
                Ok(Ok(Paced::Cancelled)) => {}
                Ok(Err(error)) => cx.failures.push(UnitFailureData {
                    lane: cx.reference.clone(),
                    unit,
                    reason: Some(internal("spawn_task", error)),
                    ..UnitFailureData::default()
                }),
                Err(_) => {
                    cx.timed_out = true;
                    cx.started.push(unit);
                }
            }
        }
    }

    /// Waits for the slot (unless withdrawn) and the previous spawn, spawns,
    /// starts the ready check and hands the slot to a holder task.
    async fn start_one(self: Arc<Self>, start: PacedStart) -> Paced {
        let PacedStart {
            project,
            lane,
            reference,
            request,
            ready,
            ticket,
            id,
            token,
            after,
            done,
            series,
        } = start;
        let name = request.name.clone();
        // `None` without a gate (or with `all_at_once`): nothing to hold.
        let slot: Option<StartSlot> = match ticket {
            Some(ticket) => tokio::select! {
                slot = ticket => Some(slot),
                () = token.cancelled() => None,
            },
            None => None,
        };
        if let Some(after) = after {
            // An error only means the previous task ended without signalling.
            let _ = after.await;
        }
        // Dropped when this function returns, after the spawn: that lets the
        // next unit of the request spawn.
        let _done = done;
        self.pacing.unregister(lane.worktree, &name, id);
        if token.is_cancelled() {
            // Dropping a granted slot gives it straight back.
            drop(slot);
            self.emit_lane(&project, &lane);
            return Paced::Cancelled;
        }
        // A `cmd` probe runs in the unit's environment; copy it before the spawn takes
        // the spec. Kept in memory only, like the launch environment itself.
        let probe_env = if matches!(ready.check, ReadySpec::Cmd { .. }) {
            request.spec.env.clone()
        } else {
            BTreeMap::new()
        };
        match self.spawn_unit(&project, &lane, request).await {
            Ok((host, managed)) => {
                let started = matches!(ready.check, ReadySpec::Started);
                let checked = host.clone();
                tokio::spawn(async move {
                    run_check(
                        &checked,
                        readiness(&ready.check, probe_env),
                        ready.timeout,
                        || async move { managed.listening_ports().await },
                    )
                    .await;
                });
                if let Some(slot) = slot {
                    let hold = Hold {
                        host,
                        slot,
                        started,
                        key: (reference, name),
                        series,
                    };
                    tokio::spawn(self.clone().hold_slot(hold));
                }
                Paced::Spawned
            }
            Err(error) => {
                if let Some(slot) = slot {
                    slot.release(Release::Failed);
                }
                Paced::Failed(error)
            }
        }
    }

    /// Holds a slot until the unit is ready or failed, feeding the gate the
    /// unit's footprint and, for `ready: started` units (ready at once), its
    /// tree CPU until it settles. The gate may end the hold earlier with its
    /// slot timeout. The highest footprint seen is recorded as the unit's
    /// startup peak.
    async fn hold_slot(self: Arc<Self>, hold: Hold) {
        let Hold {
            host,
            slot,
            started,
            key,
            series,
        } = hold;
        let pid = host.identity().map(|identity| identity.pid);
        if started && (self.integ.metrics.is_empty() || pid.is_none()) {
            // Settling cannot be measured, and holding to `slot_timeout` would
            // only delay everything queued behind it.
            if !self.pacing.unmeasured_logged.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "no process metrics: `ready: started` units release their start slot when they start"
                );
            }
            slot.release(Release::Ready);
            return;
        }
        let begun = tokio::time::Instant::now();
        let mut peak: u64 = 0;
        let mut state = host.watch_state();
        // Only readings of this run newer than the last one used count: the
        // sampler can still hold (or, mid-sample, re-insert) the previous run's
        // value, and a value read twice is not a second second of calm.
        let mut last_seq = lock(&self.metrics).reading_seq;
        let released = loop {
            if let Some(reason) = slot.released() {
                break reason;
            }
            let current = state.borrow_and_update().clone();
            let settling = match current {
                UnitState::Failed(_) | UnitState::Exited(_) | UnitState::Stopping => {
                    slot.release(Release::Failed);
                    break Release::Failed;
                }
                UnitState::Ready if !started => {
                    slot.release(Release::Ready);
                    break Release::Ready;
                }
                UnitState::Ready => true,
                UnitState::Starting => false,
            };
            {
                let mut metrics = lock(&self.metrics);
                let until = std::time::Instant::now() + SAMPLER_INTEREST;
                metrics.interest_until =
                    Some(metrics.interest_until.map_or(until, |at| at.max(until)));
            }
            tokio::select! {
                changed = state.changed() => {
                    if changed.is_err() {
                        slot.release(Release::Failed);
                        break Release::Failed;
                    }
                }
                () = tokio::time::sleep(HOLD_POLL) => {
                    let reading = lock(&self.metrics).cpu_readings.get(&key).copied();
                    if let Some(reading) = fresh_reading(reading, pid, last_seq) {
                        last_seq = reading.seq;
                        if begun.elapsed() <= STARTUP_WINDOW {
                            peak = peak.max(reading.footprint_bytes);
                        }
                        slot.report_memory(reading.footprint_bytes);
                        if settling {
                            slot.report_cpu(reading.cpu_percent);
                        }
                    }
                }
            }
        };
        // A failed start says nothing about what a healthy one needs.
        if matches!(
            released,
            Release::Ready | Release::Settled | Release::Timeout
        ) && peak > 0
            && let Some(series) = series
        {
            let duration = begun.elapsed().min(STARTUP_WINDOW);
            let record = StartupPeak {
                series,
                recorded_ms: crate::units::now_ms(),
                peak_bytes: peak,
                duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            };
            if let Err(error) = self.store.record_startup_peak(record).await {
                tracing::debug!(%error, "startup peak not recorded");
            }
        }
    }

    /// Metrics series per unit; empty while the lane has no record.
    fn unit_series(&self, cx: &StartCx, services: &[&ResolvedService]) -> HashMap<String, String> {
        let Some(lane_id) = cx.lane.st().record.as_ref().map(|record| record.id) else {
            return HashMap::new();
        };
        services
            .iter()
            .map(|service| {
                let key = Subject::Unit(UnitKey {
                    project: cx.project.id,
                    lane: lane_id,
                    unit: service.unit.clone(),
                })
                .key();
                (service.unit.clone(), key)
            })
            .collect()
    }

    /// Median of each unit's recent peaks, looked up concurrently. Units
    /// without history are absent.
    async fn baselines(
        &self,
        series: &HashMap<String, String>,
        kind: Baseline,
    ) -> HashMap<String, u64> {
        let lookups = series.iter().map(|(unit, series)| async move {
            let peaks: Vec<u64> = match kind {
                Baseline::Startup => self
                    .store
                    .startup_peaks(series.clone(), BASELINE_RUNS)
                    .await
                    .map(|rows| rows.iter().map(|row| row.peak_bytes).collect())
                    .unwrap_or_default(),
                Baseline::Run => self
                    .store
                    .run_summaries(series.clone(), BASELINE_RUNS)
                    .await
                    .map(|rows| {
                        rows.iter()
                            .filter(|run| run.samples > 0)
                            .map(|run| run.mem_peak)
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            median(peaks).map(|peak| (unit.clone(), peak))
        });
        join_all(lookups).await.into_iter().flatten().collect()
    }

    /// Emits `lane.warning` when the units about to start usually peak above
    /// the memory available now. Advice only: the start goes ahead.
    async fn warn_memory(&self, cx: &StartCx, peaks: &HashMap<String, u64>) {
        let Some(pressure) = &self.integ.pressure else {
            return;
        };
        let total = peaks
            .values()
            .fold(0_u64, |sum, peak| sum.saturating_add(*peak));
        if total == 0 {
            return;
        }
        let mut units: Vec<Value> = peaks
            .iter()
            .map(|(unit, peak)| json!({"unit": unit, "peak_memory_bytes": peak}))
            .collect();
        units.sort_by(|a, b| a["unit"].as_str().cmp(&b["unit"].as_str()));
        let available = match pressure.available_memory().await {
            Ok(Some(available)) => available,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(%error, "available memory unknown");
                return;
            }
        };
        if total > available {
            self.bus.emit(
                events::LANE_WARNING,
                json!({
                    "lane": cx.reference,
                    "code": "memory_baseline",
                    "data": {
                        "peak_memory_bytes": total,
                        "available_memory_bytes": available,
                        "units": units,
                    },
                }),
            );
        }
    }

    /// Withdraws queued starts of `lane` that a stop covers: all of them for a
    /// whole-lane stop, else those matching `selectors` and (`mine`) the caller.
    pub(crate) fn cancel_queued(
        &self,
        lane: &LaneRt,
        reference: &str,
        selectors: Option<&[String]>,
        mine: Option<&Actor>,
    ) {
        let queued: Vec<(String, Actor)> = lock(&self.pacing.waiting)
            .iter()
            .filter(|((worktree, _), _)| *worktree == lane.worktree)
            .map(|((_, unit), entry)| (unit.clone(), entry.actor.clone()))
            .collect();
        if queued.is_empty() {
            return;
        }
        let chosen: Vec<String> = match selectors.filter(|s| !s.is_empty()) {
            Some(selectors) => {
                let tags = |unit: &str| {
                    lane.st()
                        .view
                        .as_ref()
                        .and_then(|view| view.services.iter().find(|s| s.unit == unit))
                        .map(|service| service.tags.clone())
                        .unwrap_or_default()
                };
                let candidates: Vec<(String, Vec<String>)> = queued
                    .iter()
                    .map(|(unit, _)| (unit.clone(), tags(unit)))
                    .collect();
                // Selectors that match no queued unit still stop running ones.
                selectors
                    .iter()
                    .filter_map(|selector| {
                        crate::engine::match_units(
                            std::slice::from_ref(selector),
                            &candidates,
                            reference,
                        )
                        .ok()
                    })
                    .flatten()
                    .collect()
            }
            None => queued.iter().map(|(unit, _)| unit.clone()).collect(),
        };
        let waiting = lock(&self.pacing.waiting);
        for (unit, actor) in &queued {
            if chosen.contains(unit)
                && mine.is_none_or(|caller| is_mine(actor, caller))
                && let Some(entry) = waiting.get(&(lane.worktree, unit.clone()))
            {
                entry.token.cancel();
            }
        }
    }

    /// The start gate's current snapshot, taken once per status call.
    pub(crate) fn gate_snapshot(&self) -> Option<GateSnapshot> {
        self.integ.start_gate.as_ref().map(|gate| gate.snapshot())
    }

    /// Marks units waiting in the start gate as `queued`, with the reason data.
    pub(crate) fn fill_queued(&self, summary: &mut LaneSummary) {
        self.fill_queued_with(self.gate_snapshot().as_ref(), summary);
    }

    /// [`Inner::fill_queued`] against a snapshot the caller already took.
    pub(crate) fn fill_queued_with(
        &self,
        snapshot: Option<&GateSnapshot>,
        summary: &mut LaneSummary,
    ) {
        let Some(snapshot) = snapshot.filter(|snapshot| !snapshot.queued.is_empty()) else {
            return;
        };
        let waiting = lock(&self.pacing.waiting);
        for unit in &mut summary.units {
            if let Some(entry) = snapshot.find(&summary.reference, &unit.name) {
                unit.state = "queued".into();
                unit.reason = Some(RpcError::new("queued", queued_data(snapshot, entry), 0));
                // Idle summaries carry no actor; `status --mine` needs the requester.
                if let Some((_, start)) = waiting.iter().find(|((_, name), start)| {
                    start.reference == summary.reference && *name == unit.name
                }) {
                    unit.actor = start.actor.clone();
                }
            }
        }
    }

    /// `why` data for a unit waiting in the start gate.
    pub(crate) fn queued_reason(&self, reference: &str, unit: &str) -> Option<Value> {
        let snapshot = self.integ.start_gate.as_ref()?.snapshot();
        let entry = snapshot.find(reference, unit)?;
        Some(queued_data(&snapshot, entry))
    }
}

/// A reading worth reporting to the gate: same run (leader PID) and newer than
/// the last one used (`last_seq`, the sampler's reading sequence).
fn fresh_reading(
    reading: Option<CpuReading>,
    pid: Option<u32>,
    last_seq: u64,
) -> Option<CpuReading> {
    reading.filter(|reading| pid.is_some() && reading.pid == pid && reading.seq > last_seq)
}

/// `{position, queued, in_flight, window, hold, memory_need_bytes,
/// available_memory_bytes?, cpu_percent?, memory_pressure?, head?}`. `hold` is
/// the head's reason: `window_full`, `memory_critical` or `memory`. For
/// position > 1, `head` names the start everyone waits behind:
/// `{lane, unit, memory_need_bytes}`. `window` is null for an unlimited gate.
fn queued_data(snapshot: &GateSnapshot, entry: &QueuedStart) -> Value {
    let mut data = json!({
        "position": entry.position,
        "queued": snapshot.queued.len(),
        "in_flight": snapshot.in_flight,
        "window": snapshot.window,
        "hold": snapshot.hold,
    });
    if entry.position > 1
        && let Some(head) = snapshot.queued.first()
    {
        data["head"] = json!({
            "lane": head.request.lane,
            "unit": head.request.unit,
            "memory_need_bytes": head.request.memory_need,
        });
    }
    if let Some(pressure) = &snapshot.pressure {
        if let Some(cpu) = pressure.cpu_percent {
            data["cpu_percent"] = json!((cpu * 10.0).round() / 10.0);
        }
        if let Some(memory) = pressure.memory {
            data["memory_pressure"] = json!(memory.word());
        }
    }
    data["memory_need_bytes"] = json!(entry.request.memory_need);
    if let Some(available) = snapshot.available_memory {
        data["available_memory_bytes"] = json!(available);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(pid: u32, seq: u64) -> Option<CpuReading> {
        Some(CpuReading {
            cpu_percent: 1.0,
            seq,
            footprint_bytes: 1,
            pid: Some(pid),
        })
    }

    #[test]
    fn settle_only_counts_new_readings_of_the_same_run() {
        // The previous run's value (other PID), even if re-inserted late.
        assert!(fresh_reading(reading(7, 20), Some(8), 10).is_none());
        // Not newer than the hold's start or than the last reading used.
        assert!(fresh_reading(reading(8, 9), Some(8), 10).is_none());
        assert!(fresh_reading(reading(8, 10), Some(8), 10).is_none());
        assert!(fresh_reading(reading(8, 11), Some(8), 10).is_some());
        // Without an identity nothing can be matched to the run.
        assert!(fresh_reading(reading(8, 11), None, 10).is_none());
        assert!(fresh_reading(None, Some(8), 10).is_none());
    }

    #[test]
    fn why_names_the_head_for_later_positions() {
        let request = |unit: &str, memory_need| StartRequest {
            lane: "acme@1".into(),
            unit: unit.into(),
            order: 0,
            memory_need,
        };
        let snapshot = GateSnapshot {
            queued: vec![
                QueuedStart {
                    request: request("db", 4 << 30),
                    position: 1,
                },
                QueuedStart {
                    request: request("web", 1 << 20),
                    position: 2,
                },
            ],
            hold: Some(devlanes_core::gate::HoldReason::Memory),
            available_memory: Some(1 << 30),
            ..GateSnapshot::default()
        };
        let head = queued_data(&snapshot, &snapshot.queued[0]);
        assert!(head.get("head").is_none());
        let second = queued_data(&snapshot, &snapshot.queued[1]);
        assert_eq!(second["hold"], "memory");
        assert_eq!(
            second["head"],
            json!({"lane": "acme@1", "unit": "db", "memory_need_bytes": 4_u64 << 30})
        );
        assert_eq!(second["memory_need_bytes"], 1 << 20);
        assert_eq!(second["available_memory_bytes"], 1_u64 << 30);
    }

    #[test]
    fn medians_of_peaks() {
        assert_eq!(median(Vec::new()), None);
        assert_eq!(median(vec![5, 1, 3]), Some(3));
        assert_eq!(median(vec![4, 1]), Some(4));
    }
}
