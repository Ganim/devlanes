//! Startup pacing: every service start (replicas and restarts included) waits
//! for a slot from the machine-wide start gate before it spawns, and gives the
//! slot back once its startup is over. Waiting units show as `queued` in
//! `status` and `why`. Pacing only delays spawns; running processes are never
//! limited.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
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
/// Keeps the sampler on its one-second tick while a unit settles.
const SAMPLER_INTEREST: Duration = Duration::from_secs(3);
/// Past runs per unit used for the memory warning.
const BASELINE_RUNS: u32 = 5;

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
        let peaks = if gate.is_some() || self.integ.pressure.is_some() {
            self.baseline_peaks(cx, services).await
        } else {
            HashMap::new()
        };
        self.warn_memory(cx, &peaks).await;
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
        let mut tickets: Vec<Option<StartTicket>> = match &gate {
            Some(gate) => gate
                .enqueue_batch(
                    ordered
                        .iter()
                        .map(|(order, service)| StartRequest {
                            lane: cx.reference.clone(),
                            unit: service.unit.clone(),
                            order: *order,
                            memory_need: peaks
                                .get(&service.unit)
                                .copied()
                                .unwrap_or(DEFAULT_MEMORY_NEED),
                        })
                        .collect(),
                )
                .into_iter()
                .map(Some)
                .collect(),
            None => Vec::new(),
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
                    tokio::spawn(self.clone().hold_slot(host, slot, started, reference, name));
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

    /// Holds `slot` until the unit is ready or failed. `ready: started` units
    /// are ready at once, so they hold it until their tree CPU settles. The gate
    /// may end the hold earlier with its slot timeout.
    async fn hold_slot(
        self: Arc<Self>,
        host: UnitHost,
        slot: StartSlot,
        started: bool,
        reference: String,
        name: String,
    ) {
        let mut state = host.watch_state();
        let key = (reference, name);
        // Only readings of this run taken after the last one reported count:
        // the sampler can still hold (or, mid-sample, re-insert) the previous
        // run's value, and a value read twice is not a second second of calm.
        let pid = host.identity().map(|identity| identity.pid);
        let mut reported_at = host.info().started_at_ms;
        loop {
            if slot.released().is_some() {
                return;
            }
            let current = state.borrow_and_update().clone();
            let settling = match current {
                UnitState::Failed(_) | UnitState::Exited(_) | UnitState::Stopping => {
                    slot.release(Release::Failed);
                    return;
                }
                UnitState::Ready if !started => {
                    slot.release(Release::Ready);
                    return;
                }
                UnitState::Ready => true,
                UnitState::Starting => false,
            };
            if settling {
                let mut metrics = lock(&self.metrics);
                let until = std::time::Instant::now() + SAMPLER_INTEREST;
                metrics.interest_until =
                    Some(metrics.interest_until.map_or(until, |at| at.max(until)));
            }
            tokio::select! {
                changed = state.changed() => {
                    if changed.is_err() {
                        slot.release(Release::Failed);
                        return;
                    }
                }
                () = tokio::time::sleep(HOLD_POLL) => {
                    if settling {
                        let reading = lock(&self.metrics).cpu_readings.get(&key).copied();
                        if let Some(reading) = fresh_reading(reading, pid, reported_at) {
                            reported_at = reading.at_ms;
                            slot.report_cpu(reading.cpu_percent);
                        }
                    }
                }
            }
        }
    }

    /// Baseline peak memory per unit: the median `mem_peak` of its recent runs
    /// in this lane. Units without history are absent.
    async fn baseline_peaks(
        &self,
        cx: &StartCx,
        services: &[&ResolvedService],
    ) -> HashMap<String, u64> {
        let mut baselines = HashMap::new();
        let Some(lane_id) = cx.lane.st().record.as_ref().map(|record| record.id) else {
            return baselines;
        };
        for service in services {
            let series = Subject::Unit(UnitKey {
                project: cx.project.id,
                lane: lane_id,
                unit: service.unit.clone(),
            })
            .key();
            let Ok(runs) = self.store.run_summaries(series, BASELINE_RUNS).await else {
                continue;
            };
            let mut peaks: Vec<u64> = runs
                .iter()
                .filter(|run| run.samples > 0)
                .map(|run| run.mem_peak)
                .collect();
            if peaks.is_empty() {
                continue;
            }
            peaks.sort_unstable();
            baselines.insert(service.unit.clone(), peaks[peaks.len() / 2]);
        }
        baselines
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

    /// Marks units waiting in the start gate as `queued`, with the reason data.
    pub(crate) fn fill_queued(&self, summary: &mut LaneSummary) {
        let Some(gate) = &self.integ.start_gate else {
            return;
        };
        let snapshot = gate.snapshot();
        if snapshot.queued.is_empty() {
            return;
        }
        let waiting = lock(&self.pacing.waiting);
        for unit in &mut summary.units {
            if let Some(entry) = snapshot.find(&summary.reference, &unit.name) {
                unit.state = "queued".into();
                unit.reason = Some(RpcError::new("queued", queued_data(&snapshot, entry), 0));
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

/// A reading worth reporting to the settle rule: same run (leader PID) and
/// taken after `reported_at` (Unix ms).
fn fresh_reading(
    reading: Option<CpuReading>,
    pid: Option<u32>,
    reported_at: u64,
) -> Option<CpuReading> {
    reading.filter(|reading| pid.is_some() && reading.pid == pid && reading.at_ms > reported_at)
}

/// `{position, queued, in_flight, window, hold, memory_need_bytes,
/// available_memory_bytes?, cpu_percent?, memory_pressure?}`. `hold` is
/// `window_full`, `memory_critical` or `memory`.
/// `window` is null for an unlimited gate.
fn queued_data(snapshot: &GateSnapshot, entry: &QueuedStart) -> Value {
    let mut data = json!({
        "position": entry.position,
        "queued": snapshot.queued.len(),
        "in_flight": snapshot.in_flight,
        "window": snapshot.window,
        "hold": snapshot.hold,
    });
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

    fn reading(pid: u32, at_ms: u64) -> Option<CpuReading> {
        Some(CpuReading {
            cpu_percent: 1.0,
            at_ms,
            pid: Some(pid),
        })
    }

    #[test]
    fn settle_only_counts_new_readings_of_the_same_run() {
        // The previous run's value (other PID), even if re-inserted late.
        assert!(fresh_reading(reading(7, 2_000), Some(8), 1_000).is_none());
        // Older than the run's start or than the last reported reading.
        assert!(fresh_reading(reading(8, 900), Some(8), 1_000).is_none());
        assert!(fresh_reading(reading(8, 1_000), Some(8), 1_000).is_none());
        assert!(fresh_reading(reading(8, 1_500), Some(8), 1_000).is_some());
        // Without an identity nothing can be matched to the run.
        assert!(fresh_reading(reading(8, 1_500), None, 1_000).is_none());
        assert!(fresh_reading(None, Some(8), 1_000).is_none());
    }
}
