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
        GateSnapshot, HoldReason, PressureSample, PressureSource, QueuedStart, Release, SlotHandle,
        StartRequest, StartSlot, StartTicket,
    },
};
use tokio::sync::{Notify, oneshot};

use crate::controller::Controller;

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

    /// Reading interval while busy; `None` when the policy ignores pressure.
    fn sample_every(&self) -> Option<Duration> {
        match self {
            Self::Adaptive(controller) => Some(controller.config().sample_every),
            _ => None,
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
    cell: Arc<Cell>,
}

/// Mutable gate state, behind one short-lived lock.
struct State {
    policy: Policy,
    next_id: u64,
    /// Keyed by declaration order, then arrival.
    queue: BTreeMap<(usize, u64), Waiter>,
    held: HashMap<u64, Held>,
    pressure: Option<PressureSample>,
    next_sample: Duration,
}

/// Grants made under the lock and delivered after it is released, because a
/// slot whose receiver is gone releases itself, which takes the lock again.
type Grants = Vec<(oneshot::Sender<StartSlot>, StartSlot)>;

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
    /// A gate with `policy`. `pressure` is read while busy for adaptive policies
    /// and, when given, for snapshots of the others.
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
                queue: BTreeMap::new(),
                held: HashMap::new(),
                pressure: None,
                next_sample: Duration::ZERO,
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

    /// Queues a start, granting it at once when the window allows.
    pub(crate) fn enqueue(&self, request: StartRequest) -> StartTicket {
        self.start_driver();
        let (sender, receiver) = oneshot::channel();
        let (grants, key) = {
            let mut state = self.state();
            let id = state.next_id;
            state.next_id += 1;
            let key = (request.order, id);
            state.queue.insert(key, Waiter { request, sender });
            (self.grant(&mut state), key)
        };
        self.deliver(grants);
        self.wake.notify_one();
        StartTicket::queued(
            receiver,
            Leave {
                engine: self.me.clone(),
                key,
            },
        )
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
        let hold = (!queued.is_empty()).then(|| {
            if state.policy.paused() {
                HoldReason::MemoryCritical
            } else {
                HoldReason::WindowFull
            }
        });
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
        }
    }

    /// Moves waiters into free slots, in queue order.
    fn grant(&self, state: &mut State) -> Grants {
        let mut grants = Vec::new();
        let now = self.clock.now();
        while state.held.len() < state.policy.window() && !state.policy.paused() {
            let Some(((_, id), waiter)) = state.queue.pop_first() else {
                break;
            };
            // The ticket was dropped between its last poll and now.
            if waiter.sender.is_closed() {
                continue;
            }
            let cell = Arc::new(Cell::default());
            state.held.insert(
                id,
                Held {
                    deadline: now + self.options.slot_timeout,
                    below_since: None,
                    cell: cell.clone(),
                },
            );
            tracing::debug!(lane = %waiter.request.lane, unit = %waiter.request.unit, "start slot granted");
            let slot = StartSlot::new(Handle {
                engine: self.me.clone(),
                id,
                cell,
            });
            grants.push((waiter.sender, slot));
        }
        grants
    }

    /// Hands grants to their tickets outside the lock.
    fn deliver(&self, grants: Grants) {
        let granted = !grants.is_empty();
        for (sender, slot) in grants {
            // A receiver that vanished returns the slot; dropping it releases it.
            drop(sender.send(slot));
        }
        if granted {
            // New deadlines may be earlier than whatever the driver sleeps on.
            self.wake.notify_one();
        }
    }

    /// Removes a held slot with `reason` and fills the freed capacity.
    fn finish(&self, id: u64, reason: Release) {
        let grants = {
            let mut state = self.state();
            match state.held.remove(&id) {
                Some(held) => {
                    held.cell.set(reason);
                    self.grant(&mut state)
                }
                None => Vec::new(),
            }
        };
        self.deliver(grants);
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

    /// One driver pass: reads pressure when due, expires slots, grants, and
    /// returns when the next pass is due (`None`: only when woken).
    async fn pass(&self) -> Option<Duration> {
        let now = self.clock.now();
        let reading = {
            let mut state = self.state();
            let busy = !state.queue.is_empty() || !state.held.is_empty();
            if !busy {
                if let Policy::Adaptive(controller) = &mut state.policy {
                    controller.reset();
                }
                state.next_sample = Duration::ZERO;
            }
            let every = state
                .policy
                .sample_every()
                .or_else(|| self.pressure.as_ref().map(|_| Duration::from_secs(1)));
            (busy && now >= state.next_sample)
                .then_some(every)
                .flatten()
        };
        if let (Some(every), Some(source)) = (reading, &self.pressure) {
            let sample = source.sample().await;
            let now = self.clock.now();
            let mut state = self.state();
            match sample {
                Ok(sample) => {
                    state.pressure = Some(sample);
                    let demand = !state.queue.is_empty();
                    if let Policy::Adaptive(controller) = &mut state.policy {
                        controller.observe(now, &sample, demand);
                    }
                }
                Err(error) => {
                    tracing::debug!(source = source.name(), %error, "pressure reading failed")
                }
            }
            state.next_sample = now + every;
        }
        let now = self.clock.now();
        let (grants, next) = {
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
            let grants = self.grant(&mut state);
            let busy = !state.queue.is_empty() || !state.held.is_empty();
            let deadline = state.held.values().map(|held| held.deadline).min();
            let sample = (busy && self.pressure.is_some()).then_some(state.next_sample);
            let next = match (deadline, sample) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            (grants, next)
        };
        self.deliver(grants);
        next
    }
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
    key: (usize, u64),
}

impl Drop for Leave {
    /// Removes the entry if it is still queued; a granted entry is already gone.
    fn drop(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.state().queue.remove(&self.key);
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

    /// Release reason, including a gate timeout.
    fn released(&self) -> Option<Release> {
        self.cell.get()
    }
}
