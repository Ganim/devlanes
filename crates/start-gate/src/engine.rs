//! The queue, slot bookkeeping and background driver shared by every gate.
//! Policies differ only in how they size the window.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use devlanes_core::{
    clock::Clock,
    gate::{
        Arrival, BurstStats, GateSnapshot, HoldReason, PressureSample, PressureSource, QueuedStart,
        Release, SlotHandle, StartRequest, StartSlot, StartTicket,
    },
};
use tokio::sync::{Notify, oneshot};

use crate::controller::Controller;

/// Reading interval of gates whose policy does not set one.
const DEFAULT_SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// Slot rules shared by every gate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateOptions {
    /// A slot is given back after this even if its unit never reported back.
    pub slot_timeout: Duration,
    /// Tree CPU (percent of one core) under which a `ready: started` unit counts as settled.
    pub settle_cpu: f64,
    /// How long the tree CPU must stay under `settle_cpu`.
    pub settle_for: Duration,
}

impl Default for GateOptions {
    /// 20 s slot timeout; settled below 10% of a core for 2 s.
    fn default() -> Self {
        Self {
            slot_timeout: Duration::from_secs(20),
            settle_cpu: 10.0,
            settle_for: Duration::from_secs(2),
        }
    }
}

/// How the window is sized.
pub(crate) enum Policy {
    Unlimited,
    Fixed(usize),
    Adaptive(Controller),
}

impl Policy {
    /// Starts allowed at once.
    fn window(&self) -> usize {
        match self {
            Self::Unlimited => usize::MAX,
            Self::Fixed(max) => *max,
            Self::Adaptive(controller) => controller.window(),
        }
    }

    /// True when no new start may begin regardless of the window.
    fn paused(&self) -> bool {
        matches!(self, Self::Adaptive(controller) if controller.paused())
    }

    /// Reading interval while busy.
    fn sample_every(&self) -> Duration {
        match self {
            Self::Adaptive(controller) => controller.config().sample_every,
            _ => DEFAULT_SAMPLE_EVERY,
        }
    }
}

/// Release state shared by a slot's handle and the gate's record of it.
#[derive(Default)]
struct Cell(Mutex<Option<Release>>);

impl Cell {
    /// Records `reason` unless a release was already recorded.
    fn set(&self, reason: Release) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(reason);
    }

    /// The recorded release.
    fn get(&self) -> Option<Release> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A queued start.
struct Waiter {
    request: StartRequest,
    sender: oneshot::Sender<StartSlot>,
}

/// A granted, unreleased slot.
struct Held {
    deadline: Duration,
    below_since: Option<Duration>,
    /// Memory the unit was expected to need, capped at what was available when
    /// it was granted.
    memory_need: u64,
    /// The unit's tree footprint as last reported. It is already missing from
    /// the system's available memory, so only `memory_need - footprint` is
    /// still to come.
    footprint: u64,
    cell: Arc<Cell>,
}

impl Held {
    /// Memory the unit may still claim.
    fn reserved(&self) -> u64 {
        self.memory_need.saturating_sub(self.footprint)
    }
}

/// Queue key: batch arrival, declaration order within the batch, then ticket id.
type Key = (u64, usize, u64);

/// Mutable gate state, behind one short-lived lock.
struct State {
    policy: Policy,
    next_id: u64,
    next_batch: u64,
    queue: BTreeMap<Key, Waiter>,
    held: HashMap<u64, Held>,
    /// Latest reading of the current busy period; cleared when idle.
    pressure: Option<PressureSample>,
    available_memory: Option<u64>,
    /// When the last memory read of this busy period completed (successful or
    /// not); `None` before the first one.
    memory_read_at: Option<Duration>,
    next_sample: Duration,
    /// A grant pass needs a reading before it can decide; the driver should
    /// sample at once.
    sample_now: bool,
    /// Why the last grant pass stopped with starts still queued.
    hold: Option<HoldReason>,
    /// Set at the start of each busy period: the first CPU reading spans the
    /// idle time before it and is dropped.
    fresh_period: bool,
    /// Busy period number; readings that straddle a period boundary are dropped.
    period: u64,
    busy: bool,
    burst_started: Duration,
    burst: Option<BurstStats>,
}

/// Grants made under the lock and delivered after it is released, because a
/// slot whose receiver is gone releases itself, which takes the lock again.
struct Granted {
    slots: Vec<(oneshot::Sender<StartSlot>, StartSlot)>,
    /// The driver must run now (a reading is due).
    wake: bool,
}

/// Outcome of the memory check for the head of the queue.
enum MemoryCheck {
    Fits,
    Blocked,
    /// No reading of this busy period, or an old one: wait for the driver.
    Stale,
}

/// A gate engine. Public gates wrap it in an `Arc`.
pub(crate) struct Engine {
    mode: &'static str,
    me: Weak<Engine>,
    clock: Arc<dyn Clock>,
    pressure: Option<Arc<dyn PressureSource>>,
    options: GateOptions,
    state: Mutex<State>,
    wake: Arc<Notify>,
    driving: AtomicBool,
}

impl Engine {
    /// A gate with `policy`. `pressure` is read while busy: adaptive policies
    /// size the window from it, and every policy checks memory with it.
    pub(crate) fn new(
        mode: &'static str,
        policy: Policy,
        options: GateOptions,
        clock: Arc<dyn Clock>,
        pressure: Option<Arc<dyn PressureSource>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            mode,
            me: me.clone(),
            clock,
            pressure,
            options,
            state: Mutex::new(State {
                policy,
                next_id: 0,
                next_batch: 0,
                queue: BTreeMap::new(),
                held: HashMap::new(),
                pressure: None,
                available_memory: None,
                memory_read_at: None,
                next_sample: Duration::ZERO,
                sample_now: false,
                hold: None,
                fresh_period: true,
                period: 0,
                busy: false,
                burst_started: Duration::ZERO,
                burst: None,
            }),
            wake: Arc::new(Notify::new()),
            driving: AtomicBool::new(false),
        })
    }

    /// Locks the state; a panicked holder leaves consistent data behind because
    /// every mutation completes before the guard drops.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the next place in line.
    pub(crate) fn arrive(&self) -> Arrival {
        let mut state = self.state();
        let batch = state.next_batch;
        state.next_batch += 1;
        Arrival(batch)
    }

    /// Queues one start request's units as a batch at its place in line,
    /// granting what the window allows at once.
    pub(crate) fn enqueue_batch_at(
        &self,
        Arrival(batch): Arrival,
        requests: Vec<StartRequest>,
    ) -> Vec<StartTicket> {
        self.start_driver();
        let mut tickets = Vec::with_capacity(requests.len());
        let granted = {
            let mut state = self.state();
            for request in requests {
                let (sender, receiver) = oneshot::channel();
                let id = state.next_id;
                state.next_id += 1;
                let key = (batch, request.order, id);
                state.queue.insert(key, Waiter { request, sender });
                tickets.push(StartTicket::queued(
                    receiver,
                    Leave {
                        engine: self.me.clone(),
                        key,
                    },
                ));
            }
            self.grant(&mut state)
        };
        self.deliver(granted);
        self.wake.notify_one();
        tickets
    }

    /// Point-in-time view for `status` and `why`.
    pub(crate) fn snapshot(&self) -> GateSnapshot {
        let state = self.state();
        let queued: Vec<QueuedStart> = state
            .queue
            .values()
            .filter(|waiter| !waiter.sender.is_closed())
            .enumerate()
            .map(|(index, waiter)| QueuedStart {
                request: waiter.request.clone(),
                position: index + 1,
            })
            .collect();
        let hold = if queued.is_empty() {
            None
        } else {
            Some(state.hold.unwrap_or(HoldReason::WindowFull))
        };
        let mut burst = state.burst.clone();
        if let Some(burst) = burst.as_mut().filter(|_| state.busy) {
            burst.duration_ms = millis(self.clock.now().saturating_sub(state.burst_started));
        }
        GateSnapshot {
            mode: self.mode.into(),
            window: match state.policy {
                Policy::Unlimited => None,
                _ => Some(state.policy.window()),
            },
            in_flight: state.held.len(),
            queued,
            hold,
            pressure: state.pressure,
            available_memory: state.available_memory,
            burst,
        }
    }

    /// Whether the head's `need` fits. Gates without a pressure source and
    /// starts with nothing in flight always fit: the check delays, it never
    /// refuses, so the wait ends at the latest when the starts in flight finish
    /// or reach `slot_timeout`.
    fn memory_check(&self, state: &State, need: u64, now: Duration) -> MemoryCheck {
        if self.pressure.is_none() || state.held.is_empty() {
            return MemoryCheck::Fits;
        }
        let every = state.policy.sample_every();
        match state.memory_read_at {
            Some(at) if now.saturating_sub(at) <= every => {}
            _ => return MemoryCheck::Stale,
        }
        // A read that failed or a platform without the number: nothing to check.
        let Some(available) = state.available_memory else {
            return MemoryCheck::Fits;
        };
        let reserved = state
            .held
            .values()
            .fold(0_u64, |sum, held| sum.saturating_add(held.reserved()));
        if need > available.saturating_sub(reserved) {
            MemoryCheck::Blocked
        } else {
            MemoryCheck::Fits
        }
    }

    /// Moves waiters into free slots, in queue order, records why it stopped
    /// while starts are still waiting, and tracks busy-period boundaries.
    fn grant(&self, state: &mut State) -> Granted {
        let mut slots = Vec::new();
        let now = self.clock.now();
        if !state.busy && !state.queue.is_empty() {
            self.begin_period(state, now);
        }
        state.hold = None;
        while let Some((&key, waiter)) = state.queue.first_key_value() {
            // The ticket was dropped between its last poll and now.
            if waiter.sender.is_closed() {
                state.queue.remove(&key);
                continue;
            }
            let need = waiter.request.memory_need;
            if state.policy.paused() {
                state.hold = Some(HoldReason::MemoryCritical);
                break;
            }
            if state.held.len() >= state.policy.window() {
                state.hold = Some(HoldReason::WindowFull);
                break;
            }
            match self.memory_check(state, need, now) {
                MemoryCheck::Fits => {}
                MemoryCheck::Blocked => {
                    state.hold = Some(HoldReason::Memory);
                    if let Some(burst) = state.burst.as_mut() {
                        burst.memory_holds += 1;
                    }
                    break;
                }
                MemoryCheck::Stale => {
                    state.hold = Some(HoldReason::Memory);
                    state.sample_now = true;
                    break;
                }
            }
            let Some(waiter) = state.queue.remove(&key) else {
                break;
            };
            let id = key.2;
            let cell = Arc::new(Cell::default());
            // One unit cannot hold back more than there was to give.
            let memory_need = state
                .available_memory
                .map_or(need, |available| need.min(available));
            state.held.insert(
                id,
                Held {
                    deadline: now + self.options.slot_timeout,
                    below_since: None,
                    memory_need,
                    footprint: 0,
                    cell: cell.clone(),
                },
            );
            if let Some(burst) = state.burst.as_mut() {
                burst.granted += 1;
            }
            tracing::debug!(lane = %waiter.request.lane, unit = %waiter.request.unit, "start slot granted");
            let slot = StartSlot::new(Handle {
                engine: self.me.clone(),
                id,
                cell,
            });
            slots.push((waiter.sender, slot));
        }
        if state.busy && state.queue.is_empty() && state.held.is_empty() {
            self.end_period(state, now);
        }
        Granted {
            slots,
            wake: state.sample_now,
        }
    }

    /// Starts a busy period with a fresh window and no readings.
    fn begin_period(&self, state: &mut State, now: Duration) {
        state.busy = true;
        state.period += 1;
        state.fresh_period = true;
        state.pressure = None;
        state.available_memory = None;
        state.memory_read_at = None;
        state.next_sample = now;
        if let Policy::Adaptive(controller) = &mut state.policy {
            controller.reset();
        }
        state.burst_started = now;
        state.burst = Some(BurstStats {
            max_window: match state.policy {
                Policy::Unlimited => None,
                _ => Some(state.policy.window()),
            },
            ..BurstStats::default()
        });
    }

    /// Ends a busy period: logs what it saw and drops readings that would be
    /// stale by the next one.
    fn end_period(&self, state: &mut State, now: Duration) {
        state.busy = false;
        state.pressure = None;
        state.available_memory = None;
        state.memory_read_at = None;
        state.sample_now = false;
        if let Policy::Adaptive(controller) = &mut state.policy {
            controller.reset();
        }
        if let Some(burst) = state.burst.as_mut() {
            burst.duration_ms = millis(now.saturating_sub(state.burst_started));
            tracing::info!(
                mode = self.mode,
                granted = burst.granted,
                duration_ms = burst.duration_ms,
                max_window = ?burst.max_window,
                peak_cpu_percent = ?burst.peak_cpu_percent,
                worst_memory = ?burst.worst_memory,
                memory_holds = burst.memory_holds,
                "start burst finished"
            );
        }
    }

    /// Hands grants to their tickets outside the lock.
    fn deliver(&self, granted: Granted) {
        let wake = granted.wake || !granted.slots.is_empty();
        for (sender, slot) in granted.slots {
            // A receiver that vanished returns the slot; dropping it releases it.
            drop(sender.send(slot));
        }
        if wake {
            // New deadlines may be earlier than whatever the driver sleeps on,
            // or a reading is needed before the head can be decided.
            self.wake.notify_one();
        }
    }

    /// Removes a held slot with `reason` and fills the freed capacity.
    fn finish(&self, id: u64, reason: Release) {
        let granted = {
            let mut state = self.state();
            match state.held.remove(&id) {
                Some(held) => {
                    held.cell.set(reason);
                    Some(self.grant(&mut state))
                }
                None => None,
            }
        };
        if let Some(granted) = granted {
            self.deliver(granted);
        }
    }

    /// Applies one CPU report to the settle rule of slot `id`.
    fn report_cpu(&self, id: u64, cpu_percent: f64) {
        let now = self.clock.now();
        let settled = {
            let mut state = self.state();
            let Some(held) = state.held.get_mut(&id) else {
                return;
            };
            if cpu_percent < self.options.settle_cpu {
                let since = *held.below_since.get_or_insert(now);
                now.saturating_sub(since) >= self.options.settle_for
            } else {
                held.below_since = None;
                false
            }
        };
        if settled {
            self.finish(id, Release::Settled);
        }
    }

    /// Records slot `id`'s current footprint; a smaller reservation may let the
    /// head through.
    fn report_memory(&self, id: u64, footprint: u64) {
        let granted = {
            let mut state = self.state();
            let Some(held) = state.held.get_mut(&id) else {
                return;
            };
            held.footprint = footprint;
            self.grant(&mut state)
        };
        self.deliver(granted);
    }

    /// Takes a queued entry out and re-runs the grant, because the head may
    /// have been the one holding everything up.
    fn leave(&self, key: Key) {
        let granted = {
            let mut state = self.state();
            if state.queue.remove(&key).is_none() {
                return;
            }
            self.grant(&mut state)
        };
        self.deliver(granted);
    }

    /// Spawns the background driver once, from inside a runtime. Without a
    /// runtime, slot timeouts and pressure readings are inactive.
    fn start_driver(&self) {
        if self.driving.swap(true, Ordering::SeqCst) {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(drive(
                    self.me.clone(),
                    self.wake.clone(),
                    self.clock.clone(),
                ));
            }
            Err(_) => self.driving.store(false, Ordering::SeqCst),
        }
    }

    /// Reads pressure and available memory when due, outside the lock, and
    /// stores them unless the busy period changed meanwhile.
    async fn read_pressure(&self, source: &dyn PressureSource) {
        let now = self.clock.now();
        let due = {
            let mut state = self.state();
            let due = state.busy && (state.sample_now || now >= state.next_sample);
            if due {
                state.sample_now = false;
            }
            due.then_some(state.period)
        };
        let Some(period) = due else {
            return;
        };
        let sample = source.sample().await;
        let available = source.available_memory().await;
        let now = self.clock.now();
        let mut state = self.state();
        if !state.busy || state.period != period {
            return;
        }
        state.available_memory = available.unwrap_or_else(|error| {
            tracing::debug!(source = source.name(), %error, "available memory unknown");
            None
        });
        state.memory_read_at = Some(now);
        match sample {
            Ok(mut sample) => {
                // The source's CPU counters date from before this busy period.
                if std::mem::take(&mut state.fresh_period) {
                    sample.cpu_percent = None;
                }
                state.pressure = Some(sample);
                // The window only grows while it is what holds starts back.
                let demand = state.hold == Some(HoldReason::WindowFull);
                if let Policy::Adaptive(controller) = &mut state.policy {
                    controller.observe(now, &sample, demand);
                }
                let window = match state.policy {
                    Policy::Unlimited => None,
                    _ => Some(state.policy.window()),
                };
                if let Some(burst) = state.burst.as_mut() {
                    if let Some(cpu) = sample.cpu_percent {
                        burst.peak_cpu_percent =
                            Some(burst.peak_cpu_percent.map_or(cpu, |peak| peak.max(cpu)));
                    }
                    if let Some(memory) = sample.memory {
                        burst.worst_memory = burst.worst_memory.max(Some(memory));
                    }
                    burst.max_window = burst.max_window.max(window);
                }
            }
            Err(error) => {
                state.pressure = None;
                tracing::debug!(source = source.name(), %error, "pressure reading failed");
            }
        }
        state.next_sample = now + state.policy.sample_every();
    }

    /// One driver pass: reads pressure when due, expires slots, grants, and
    /// returns when the next pass is due (`None`: only when woken).
    async fn pass(&self) -> Option<Duration> {
        if let Some(source) = &self.pressure {
            self.read_pressure(source.as_ref()).await;
        }
        let now = self.clock.now();
        let (granted, next) = {
            let mut state = self.state();
            let expired: Vec<u64> = state
                .held
                .iter()
                .filter(|(_, held)| held.deadline <= now)
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                if let Some(held) = state.held.remove(&id) {
                    tracing::debug!("start slot timed out");
                    held.cell.set(Release::Timeout);
                }
            }
            let granted = self.grant(&mut state);
            let deadline = state.held.values().map(|held| held.deadline).min();
            let sample = (state.busy && self.pressure.is_some()).then(|| {
                if state.sample_now {
                    now
                } else {
                    state.next_sample
                }
            });
            let next = [deadline, sample].into_iter().flatten().min();
            (granted, next)
        };
        self.deliver(granted);
        next
    }
}

/// Whole milliseconds of `duration`, saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Drop for Engine {
    /// Wakes the driver so it notices the engine is gone and exits.
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

/// Runs passes until the engine is dropped. Holds only a weak reference while
/// sleeping so dropping the gate ends the task.
async fn drive(engine: Weak<Engine>, wake: Arc<Notify>, clock: Arc<dyn Clock>) {
    loop {
        let next = match engine.upgrade() {
            Some(engine) => engine.pass().await,
            None => return,
        };
        match next {
            Some(at) => {
                tokio::select! {
                    () = clock.sleep_until(at) => {}
                    () = wake.notified() => {}
                }
            }
            None => wake.notified().await,
        }
    }
}

/// Takes an abandoned ticket's request out of the queue.
struct Leave {
    engine: Weak<Engine>,
    key: Key,
}

impl Drop for Leave {
    /// Removes the entry if it is still queued; a granted entry is already gone.
    fn drop(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.leave(self.key);
        }
    }
}

/// Holder side of a granted slot.
struct Handle {
    engine: Weak<Engine>,
    id: u64,
    cell: Arc<Cell>,
}

impl SlotHandle for Handle {
    /// Frees the slot; ignored after the first release.
    fn release(&self, reason: Release) {
        match self.engine.upgrade() {
            Some(engine) => engine.finish(self.id, reason),
            None => self.cell.set(reason),
        }
    }

    /// Feeds the settle rule.
    fn report_cpu(&self, cpu_percent: f64) {
        if let Some(engine) = self.engine.upgrade() {
            engine.report_cpu(self.id, cpu_percent);
        }
    }

    /// Lowers the slot's memory reservation by what the unit already holds.
    fn report_memory(&self, footprint_bytes: u64) {
        if let Some(engine) = self.engine.upgrade() {
            engine.report_memory(self.id, footprint_bytes);
        }
    }

    /// Release reason, including a gate timeout.
    fn released(&self) -> Option<Release> {
        self.cell.get()
    }
}
