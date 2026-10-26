//! Idle lanes. A lane without activity for `lanes.throttle_after` runs its
//! services at background priority (macOS, Windows), and after
//! `lanes.freeze_after` its service processes are suspended and its own
//! containers paused. The next request, connection, CPU burst or command runs it
//! normally again. Nothing is stopped or restarted: processes keep their
//! state, ports and PIDs, so this is neither auto-restart nor a resource limit.
//!
//! Activity reaches the loop through [`IdleBoard`], which the proxy writes to
//! without touching lane state, so the request path stays one map lookup.
//! Idle times use a monotonic clock, so a wall-clock step never hides activity.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use devlanes_core::Power;
use devlanes_proto::{Context, RpcError, events};
use futures_util::future::join_all;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::{
    engine::{Inner, LanePair},
    integrations::{InfraError, ManagedUnit},
    model::{Kind, LanePowerSt, LaneRt, LaneSt, PowerWord, ProjectRt, lock},
    units::{UnitState, now_ms},
};

/// How often the idle timers are checked; also bounds [`KNOCK_FROZEN`].
const TICK: Duration = Duration::from_millis(500);
/// How often the TCP table is read for new connections to lane ports.
const KNOCK_EVERY: Duration = Duration::from_secs(5);
/// The same while a lane is frozen, so a direct connection thaws it quickly.
const KNOCK_FROZEN: Duration = Duration::from_millis(500);
/// A lane whose services together use this much of one core in a metrics
/// sample is busy, even without requests (workers, builds, test runs).
const CPU_ACTIVE_PERCENT: f64 = 25.0;
/// Bound on one unit's power change, so a wedged runner cannot stall a lane.
const SET_POWER_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on pausing or unpausing a lane's containers.
const CONTAINERS_TIMEOUT: Duration = Duration::from_secs(60);
/// Proxy requests closer together than this record no new activity; the idle
/// timers count minutes, and the proxy calls the board on every request.
const PROXY_TOUCH_EVERY_MS: u64 = 1_000;

/// What last kept a lane awake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A request or upgraded-stream bytes through the proxy.
    Proxy,
    /// A new TCP connection to one of the lane's ports.
    Connection,
    /// The lane's services used CPU above the threshold.
    Cpu,
    /// A client command that needs the lane's processes.
    Command,
    /// A stop, restart or clean, which thaws the lane first.
    Stop,
}

impl Activity {
    /// Stable word for `LanePower.last_activity` and `lane.thawed`.
    pub fn word(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Connection => "connection",
            Self::Cpu => "cpu",
            Self::Command => "command",
            Self::Stop => "stop",
        }
    }

    /// Packs the kind for an atomic slot.
    fn to_u8(self) -> u8 {
        match self {
            Self::Proxy => 1,
            Self::Connection => 2,
            Self::Cpu => 3,
            Self::Command => 4,
            Self::Stop => 5,
        }
    }

    /// Unpacks [`Activity::to_u8`]; `None` before any activity.
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Proxy,
            2 => Self::Connection,
            3 => Self::Cpu,
            4 => Self::Command,
            5 => Self::Stop,
            _ => return None,
        })
    }
}

/// A unit by name with the runner handle that changes its power level.
type Named = (String, Arc<dyn ManagedUnit>);

/// A future the proxy awaits before forwarding to a frozen lane.
pub type ThawWait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Per-lane activity and power, shared by the proxy, client commands and the
/// idle loop. Keyed by lane reference (`acme@0`), as proxy routes name lanes.
pub struct IdleBoard {
    lanes: Mutex<HashMap<String, Arc<Slot>>>,
    wakes: mpsc::UnboundedSender<String>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    /// Origin of the monotonic milliseconds in [`Slot::last`].
    epoch: Instant,
    /// Set once a runner answered that it cannot throttle (Linux), so later
    /// ticks go straight to freezing.
    no_throttle: AtomicBool,
    /// Reads of the machine's TCP table for new connections; tests turn them off.
    connection_checks: AtomicBool,
}

/// One lane's activity and power.
struct Slot {
    /// Monotonic ms ([`IdleBoard::mono`]) of the last activity; 0 before any.
    last: AtomicU64,
    /// Unix ms of the same activity, for status.
    last_wall: AtomicU64,
    kind: AtomicU8,
    /// The `last` a throttle or freeze failed at; the lane is retried only
    /// after newer activity, so a failing runner is not asked every tick.
    failed_at: AtomicU64,
    power: watch::Sender<Power>,
    /// A wake for this lane is queued for the idle loop.
    waking: AtomicBool,
}

impl Slot {
    /// A lane that has seen no activity yet.
    fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
            last_wall: AtomicU64::new(0),
            kind: AtomicU8::new(0),
            failed_at: AtomicU64::new(u64::MAX),
            power: watch::channel(Power::Normal).0,
            waking: AtomicBool::new(false),
        }
    }
}

impl fmt::Debug for IdleBoard {
    /// Lists lanes with their power level.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lanes = lock(&self.lanes);
        f.debug_map()
            .entries(
                lanes
                    .iter()
                    .map(|(lane, slot)| (lane, *slot.power.borrow())),
            )
            .finish()
    }
}

impl Default for IdleBoard {
    /// An empty board whose wake receiver the idle loop takes once.
    fn default() -> Self {
        let (wakes, receiver) = mpsc::unbounded_channel();
        Self {
            lanes: Mutex::new(HashMap::new()),
            wakes,
            receiver: Mutex::new(Some(receiver)),
            epoch: Instant::now(),
            no_throttle: AtomicBool::new(false),
            connection_checks: AtomicBool::new(true),
        }
    }
}

impl IdleBoard {
    /// Turns off reading the machine's TCP table, so tests depend only on what
    /// they do themselves.
    pub fn without_connection_checks(self) -> Self {
        self.connection_checks.store(false, Ordering::Relaxed);
        self
    }

    /// Milliseconds since the board was made, never 0.
    fn mono(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    /// The slot of `lane`, created on first use.
    fn slot(&self, lane: &str) -> Arc<Slot> {
        let mut lanes = lock(&self.lanes);
        if let Some(slot) = lanes.get(lane) {
            return slot.clone();
        }
        lanes
            .entry(lane.to_owned())
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone()
    }

    /// Records activity on `slot` now.
    fn record(&self, slot: &Slot, activity: Activity) {
        let now = self.mono();
        if activity == Activity::Proxy
            && now.saturating_sub(slot.last.load(Ordering::SeqCst)) < PROXY_TOUCH_EVERY_MS
            && slot.kind.load(Ordering::Relaxed) == Activity::Proxy.to_u8()
        {
            return;
        }
        // SeqCst pairs with the freeze publishing its level and then reading
        // `last`: either the freeze sees this activity or this caller sees the
        // new level and queues a wake.
        slot.last.store(now, Ordering::SeqCst);
        slot.last_wall.store(now_ms(), Ordering::Relaxed);
        slot.kind.store(activity.to_u8(), Ordering::Relaxed);
    }

    /// A request through the proxy for `lane`. When the lane is idle, the idle
    /// loop is asked to wake it; when it is frozen, the returned future
    /// resolves once its processes run again.
    pub fn request(&self, lane: &str) -> Option<ThawWait> {
        let slot = self.slot(lane);
        self.record(&slot, Activity::Proxy);
        let power = *slot.power.borrow();
        if power == Power::Normal {
            return None;
        }
        self.queue_wake(lane, &slot);
        if power != Power::Frozen {
            return None;
        }
        let mut power = slot.power.subscribe();
        Some(Box::pin(async move {
            let _ = power.wait_for(|power| *power != Power::Frozen).await;
        }))
    }

    /// Client bytes on an upgraded stream through the proxy.
    pub fn stream(&self, lane: &str) {
        self.activity(lane, Activity::Proxy);
    }

    /// Records activity and queues a wake when the lane is throttled or frozen.
    pub(crate) fn activity(&self, lane: &str, activity: Activity) {
        let slot = self.slot(lane);
        self.record(&slot, activity);
        if *slot.power.borrow() != Power::Normal {
            self.queue_wake(lane, &slot);
        }
    }

    /// The last activity of `lane`, as Unix ms and kind; `None` before any.
    pub(crate) fn last(&self, lane: &str) -> Option<(u64, Activity)> {
        let slot = self.slot(lane);
        let ms = slot.last_wall.load(Ordering::Acquire);
        let kind = Activity::from_u8(slot.kind.load(Ordering::Acquire))?;
        (ms > 0).then_some((ms, kind))
    }

    /// Queues one wake for the idle loop unless one is pending.
    fn queue_wake(&self, lane: &str, slot: &Slot) {
        if !slot.waking.swap(true, Ordering::AcqRel) {
            let _ = self.wakes.send(lane.to_owned());
        }
    }

    /// Publishes the lane's level and clears a pending wake once it runs
    /// normally. Called with the lane's power lock held.
    fn set_power(&self, lane: &str, power: Power) {
        let slot = self.slot(lane);
        slot.power.send_replace(power);
        if power == Power::Normal {
            slot.waking.store(false, Ordering::Release);
        }
    }

    /// Lets the next activity queue a wake again after a thaw failed.
    fn wake_failed(&self, lane: &str) {
        self.slot(lane).waking.store(false, Ordering::Release);
    }

    /// The idle loop's receiver of wake requests; `None` after the first call.
    fn take_wakes(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        lock(&self.receiver).take()
    }

    /// Forgets lanes that no longer exist.
    fn retain(&self, lanes: &HashSet<String>) {
        lock(&self.lanes).retain(|lane, _| lanes.contains(lane));
    }
}

/// Why a lane cannot go idle now; the code of a refused `debug.freeze`.
fn refusal(code: &str, data: Value) -> RpcError {
    RpcError::new(code, data, 1)
}

/// The lane's live service processes when it may go idle now, else why not.
fn idle_units(st: &LaneSt, reference: &str) -> Result<Vec<Named>, RpcError> {
    let lane = json!({"lane": reference});
    if st.record.is_none() {
        return Err(refusal("lane_not_running", lane));
    }
    if !st.orphans.is_empty() {
        let units: Vec<&String> = st.orphans.keys().collect();
        return Err(refusal(
            "lane_has_orphans",
            json!({"lane": reference, "units": units}),
        ));
    }
    if st.start.is_some() || st.busy.is_some() || st.spawning > 0 || st.closing || st.sweeping {
        return Err(refusal("lane_busy", lane));
    }
    let mut units = Vec::new();
    let mut keep_awake = Vec::new();
    for (name, rec) in st.units.iter().filter(|(_, rec)| rec.host.is_alive()) {
        // A running task or exec does work the user waits for; a unit that is
        // starting or stopping belongs to an operation in flight.
        if rec.kind != Kind::Service
            || matches!(rec.host.state(), UnitState::Starting | UnitState::Stopping)
        {
            return Err(refusal("lane_busy", lane));
        }
        if rec.keep_awake {
            keep_awake.push(name.clone());
        }
        units.push((name.clone(), rec.managed.clone()));
    }
    if !keep_awake.is_empty() {
        return Err(refusal(
            "lane_keep_awake",
            json!({"lane": reference, "units": keep_awake}),
        ));
    }
    if units.is_empty() {
        return Err(refusal("lane_not_running", lane));
    }
    Ok(units)
}

/// Whether the lane runs containers of its own, which a freeze pauses.
fn own_containers(st: &LaneSt) -> bool {
    st.infra_containers
        .iter()
        .any(|container| !container.shared)
}

/// Orders power levels from running to frozen.
fn depth(power: Power) -> u8 {
    match power {
        Power::Normal => 0,
        Power::Throttled => 1,
        Power::Frozen => 2,
    }
}

/// Sets every unit to `power`, each bounded by [`SET_POWER_TIMEOUT`]. Units
/// that exited meanwhile count as done.
async fn set_all(units: &[Named], power: Power) -> Vec<(String, Result<(), devlanes_core::Error>)> {
    join_all(units.iter().map(|(name, unit)| async move {
        let result = match tokio::time::timeout(SET_POWER_TIMEOUT, unit.set_power(power)).await {
            Ok(Err(devlanes_core::Error::Exited)) => Ok(()),
            Ok(result) => result,
            Err(_) => Err(devlanes_core::Error::Integration(format!(
                "setting power took longer than {}s",
                SET_POWER_TIMEOUT.as_secs()
            ))),
        };
        (name.clone(), result)
    }))
    .await
}

/// Runs a container pause or unpause within [`CONTAINERS_TIMEOUT`].
async fn bounded(
    call: impl Future<Output = Result<usize, InfraError>>,
) -> Result<usize, InfraError> {
    tokio::time::timeout(CONTAINERS_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| {
            Err(InfraError::new(
                "timeout",
                format!(
                    "containers did not answer within {}s",
                    CONTAINERS_TIMEOUT.as_secs()
                ),
            ))
        })
}

/// New-connection tracking for the knock check.
#[derive(Default)]
struct Knock {
    last: Option<Instant>,
    /// Connections to each lane's ports at the previous read.
    seen: HashMap<String, HashSet<(SocketAddr, SocketAddr)>>,
    /// A failed read was logged at warn level once.
    warned: bool,
}

impl Inner {
    /// Runs the idle timers, wake requests and activity checks until shutdown.
    pub(crate) async fn idle_loop(self: Arc<Self>) {
        let Some(mut wakes) = self.opts.idle.take_wakes() else {
            return;
        };
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut knock = Knock::default();
        let mut cpu_seen: HashMap<String, u64> = HashMap::new();
        // Wakes have their own task, and each runs on its own, so a tick busy
        // freezing a lane (runner and container calls can take seconds) never
        // holds back a request waiting for another lane.
        let engine = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = engine.shutdown.cancelled() => return,
                    reference = wakes.recv() => {
                        let Some(reference) = reference else { return };
                        let engine = engine.clone();
                        tokio::spawn(async move { engine.wake_reference(&reference).await });
                    }
                }
            }
        });
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = tick.tick() => self.idle_tick(&mut knock, &mut cpu_seen).await,
            }
        }
    }

    /// Thaws the lane named by `reference`, for a wake the board queued.
    async fn wake_reference(self: &Arc<Self>, reference: &str) {
        let found = self
            .all_lanes()
            .into_iter()
            .find(|(project, lane)| lane.st().reference(&project.name()) == reference);
        match found {
            Some((project, lane)) => {
                self.wake_lane(&project, &lane, None).await;
            }
            None => self.opts.idle.set_power(reference, Power::Normal),
        }
    }

    /// One pass: activity checks, then throttle or freeze lanes idle long enough.
    async fn idle_tick(self: &Arc<Self>, knock: &mut Knock, cpu_seen: &mut HashMap<String, u64>) {
        let board = &self.opts.idle;
        let throttle_after = self.opts.user.lanes.throttle_after();
        let freeze_after = self.opts.user.lanes.freeze_after();
        let lanes: Vec<(String, LanePair)> = self
            .all_lanes()
            .into_iter()
            .map(|(project, lane)| {
                let reference = lane.st().reference(&project.name());
                (reference, (project, lane))
            })
            .collect();
        let references: HashSet<String> = lanes.iter().map(|(r, _)| r.clone()).collect();
        board.retain(&references);
        cpu_seen.retain(|reference, _| references.contains(reference));
        let idle_lanes = lanes
            .iter()
            .filter(|(_, (_, lane))| lane.st().power.is_some())
            .count();
        if throttle_after.is_none() && freeze_after.is_none() && idle_lanes == 0 {
            return;
        }
        let frozen = lanes.iter().any(|(_, (_, lane))| {
            lane.st()
                .power
                .as_ref()
                .is_some_and(|power| power.level == Power::Frozen)
        });
        let every = if frozen { KNOCK_FROZEN } else { KNOCK_EVERY };
        if knock.last.is_none_or(|last| last.elapsed() >= every) {
            knock.last = Some(Instant::now());
            self.knock(&lanes, knock).await;
        }
        self.cpu_activity(&lanes, cpu_seen);

        let now = board.mono();
        let throttle_after = throttle_after.filter(|_| !board.no_throttle.load(Ordering::Relaxed));
        for (reference, (project, lane)) in &lanes {
            let slot = board.slot(reference);
            let (power, alive) = {
                let st = lane.st();
                (
                    st.power.clone(),
                    st.units.values().any(|rec| rec.host.is_alive()),
                )
            };
            // First sight of the lane: its idle time counts from now, with no
            // activity kind to report yet.
            let _ = slot
                .last
                .compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst);
            let last = slot.last.load(Ordering::SeqCst);
            if let Some(power) = &power {
                // Activity that raced the freeze, or a lane whose processes all
                // ended while it was idle (its containers may still be paused).
                if last > power.seen || !alive {
                    let (engine, project, lane) = (self.clone(), project.clone(), lane.clone());
                    tokio::spawn(async move { engine.wake_lane(&project, &lane, None).await });
                    continue;
                }
            }
            if slot.failed_at.load(Ordering::Acquire) == last {
                continue;
            }
            let idle = Duration::from_millis(now.saturating_sub(last));
            let target = if freeze_after.is_some_and(|after| idle >= after) {
                Power::Frozen
            } else if throttle_after.is_some_and(|after| idle >= after) {
                Power::Throttled
            } else {
                continue;
            };
            let current = power.map_or(Power::Normal, |power| power.level);
            if depth(target) <= depth(current) {
                continue;
            }
            if target == Power::Frozen && !knock.seen.contains_key(reference) {
                // Without a baseline the first read after the freeze would
                // take a connection made meanwhile as old, and it would hang.
                knock.last = Some(Instant::now());
                self.knock(&lanes, knock).await;
                knock.seen.entry(reference.clone()).or_default();
            }
            if let Err(error) = self.make_idle(project, lane, target, Some(last)).await {
                // Every tick for a busy or `keep_awake` lane, so not at debug.
                tracing::trace!(lane = %reference, code = %error.code, "lane stays awake");
            }
        }
    }

    /// Throttles or freezes `lane` now. `seen` is the last activity the caller
    /// decided on; anything newer thaws the lane again right away. Refuses a
    /// lane that is busy, runs no service, has orphans or runs a `keep_awake`
    /// service. A runner failure undoes what was applied and emits
    /// `lane.idle_failed`.
    pub(crate) async fn make_idle(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        target: Power,
        seen: Option<u64>,
    ) -> Result<(), RpcError> {
        let reference = lane.st().reference(&project.name());
        let board = &self.opts.idle;
        let seen = seen.unwrap_or_else(|| board.mono());
        let applied = self
            .apply_idle(project, lane, &reference, target, seen)
            .await?;
        // The locks are released: activity that arrived while processes were
        // being suspended thaws the lane now instead of at the next tick.
        if applied && board.slot(&reference).last.load(Ordering::SeqCst) > seen {
            self.wake_lane(project, lane, None).await;
        }
        Ok(())
    }

    /// The body of [`Inner::make_idle`] under the lane's `op` and power locks.
    /// Returns whether the lane changed level.
    async fn apply_idle(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        reference: &str,
        target: Power,
        seen: u64,
    ) -> Result<bool, RpcError> {
        let board = &self.opts.idle;
        // Holding `op` keeps starts, stops and cleans out while processes change
        // level; one that holds it now means the lane is busy.
        let Ok(_op) = lane.op.try_lock() else {
            return Err(refusal("lane_busy", json!({"lane": reference})));
        };
        let _power = lane.power.lock().await;
        let (units, current, previous, containers) = {
            let st = lane.st();
            let units = idle_units(&st, reference)?;
            let current = st.power.as_ref().map_or(Power::Normal, |power| power.level);
            // Lanes without their own containers skip the Docker round trip.
            let containers = st.index().filter(|_| own_containers(&st));
            (units, current, st.power.clone(), containers)
        };
        if depth(target) <= depth(current) {
            return Ok(false);
        }
        let slot = board.slot(reference);
        let since_ms = now_ms();
        let results = set_all(&units, target).await;
        let failed: Vec<(String, String)> = results
            .iter()
            .filter_map(|(name, result)| {
                result
                    .as_ref()
                    .err()
                    .map(|error| (name.clone(), error.to_string()))
            })
            .collect();
        if !failed.is_empty() {
            let refused = |result: &Result<(), devlanes_core::Error>| {
                matches!(result, Err(devlanes_core::Error::Unsupported(_)))
            };
            // Failed units too: a timed-out change may still finish in the
            // runner, and going back to the current level is harmless otherwise.
            // Nothing needs undoing when every unit refused outright.
            if !results.iter().all(|(_, result)| refused(result)) {
                set_all(&units, current).await;
            }
            let unsupported = results
                .iter()
                .filter(|(_, result)| result.is_err())
                .all(|(_, result)| refused(result));
            if target == Power::Throttled && unsupported {
                // Linux has no reversible way to lower priority without
                // privileges; idle lanes there go straight to frozen.
                board.no_throttle.store(true, Ordering::Relaxed);
                return Err(refusal("throttle_unsupported", json!({"lane": reference})));
            }
            slot.failed_at.store(seen, Ordering::Release);
            let message = failed
                .iter()
                .map(|(name, error)| format!("{name}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            tracing::warn!(lane = %reference, level = target.word(), %message, "lane did not go idle");
            self.bus.emit(
                events::LANE_IDLE_FAILED,
                json!({"lane": reference, "level": target.word(), "code": "idle_failed", "message": message}),
            );
            return Err(refusal(
                "idle_failed",
                json!({"lane": reference, "level": target.word(), "message": message}),
            ));
        }
        let mut paused_containers = previous
            .as_ref()
            .is_some_and(|power| power.paused_containers);
        if target == Power::Frozen
            && let Some(index) = containers
        {
            match bounded(self.integ.infra.pause_lane(&project.name(), index)).await {
                Ok(paused) => paused_containers |= paused > 0,
                // The lane's processes are frozen either way. A pause that failed
                // partway may still have paused some containers, so the thaw
                // unpauses, which is harmless for the rest.
                Err(error) => {
                    paused_containers = true;
                    tracing::warn!(lane = %reference, code = %error.code, "lane containers not paused");
                }
            }
        }
        lane.st().power = Some(LanePowerSt {
            level: target,
            since_ms,
            seen,
            last_activity: board.last(reference).map(|(ms, kind)| (ms, kind.word())),
            paused_containers,
        });
        board.set_power(reference, target);
        tracing::info!(lane = %reference, level = target.word(), "lane idle");
        self.bus.emit(
            events::LANE_IDLE,
            json!({"lane": reference, "level": target.word()}),
        );
        self.emit_lane(project, lane);
        Ok(true)
    }

    /// Records `activity` (when given) and runs a throttled or frozen lane
    /// normally again: containers first, so services find their database
    /// ready, then every live unit. Returns whether the lane was idle and now
    /// runs normally. When a unit does not resume, the lane stays marked idle
    /// and the next activity or tick tries again.
    pub(crate) async fn wake_lane(
        self: &Arc<Self>,
        project: &ProjectRt,
        lane: &LaneRt,
        activity: Option<Activity>,
    ) -> bool {
        let board = &self.opts.idle;
        let reference = lane.st().reference(&project.name());
        if let Some(activity) = activity {
            board.activity(&reference, activity);
        }
        // Always under the lock: a freeze in progress holds it until the lane's
        // state and the board agree, and the board is only written under it.
        let _power = lane.power.lock().await;
        let (state, containers) = {
            let st = lane.st();
            let Some(state) = st.power.clone() else {
                drop(st);
                board.set_power(&reference, Power::Normal);
                return false;
            };
            let containers = st
                .index()
                .filter(|_| state.paused_containers || own_containers(&st));
            (state, containers)
        };
        if let Some(index) = containers
            && let Err(error) = bounded(self.integ.infra.unpause_lane(&project.name(), index)).await
        {
            // The services resume anyway; a start unpauses the containers first.
            tracing::warn!(lane = %reference, code = %error.code, "lane containers not unpaused");
        }
        let units: Vec<Named> = lane
            .st()
            .units
            .iter()
            .filter(|(_, rec)| rec.host.is_alive())
            .map(|(name, rec)| (name.clone(), rec.managed.clone()))
            .collect();
        let failed: Vec<String> = set_all(&units, Power::Normal)
            .await
            .into_iter()
            .filter_map(|(name, result)| result.err().map(|error| format!("{name}: {error}")))
            .collect();
        if !failed.is_empty() {
            // The lane stays marked idle, so status does not claim it runs and
            // the next activity or tick tries again.
            let message = failed.join("; ");
            tracing::warn!(lane = %reference, %message, "lane not thawed");
            lane.st().power = Some(LanePowerSt {
                paused_containers: false,
                seen: board.slot(&reference).last.load(Ordering::SeqCst),
                ..state
            });
            board.wake_failed(&reference);
            self.bus.emit(
                events::LANE_IDLE_FAILED,
                json!({"lane": reference, "level": "normal", "code": "thaw_failed", "message": message}),
            );
            self.emit_lane(project, lane);
            return false;
        }
        lane.st().power = None;
        board.set_power(&reference, Power::Normal);
        let woke_by = activity
            .or_else(|| board.last(&reference).map(|(_, kind)| kind))
            .map(Activity::word);
        tracing::info!(lane = %reference, from = state.level.word(), activity = woke_by, "lane thawed");
        self.bus.emit(
            events::LANE_THAWED,
            json!({"lane": reference, "from": state.level.word(), "activity": woke_by}),
        );
        self.emit_lane(project, lane);
        true
    }

    /// Thaws every lane, before a daemon shutdown or state reset stops them.
    pub(crate) async fn wake_all(self: &Arc<Self>) {
        for (project, lane) in self.all_lanes() {
            self.wake_lane(&project, &lane, Some(Activity::Stop)).await;
        }
    }

    /// Reads the TCP table and records activity for lanes with new connections
    /// to their ports since the previous read.
    async fn knock(self: &Arc<Self>, lanes: &[(String, LanePair)], knock: &mut Knock) {
        if !self.opts.idle.connection_checks.load(Ordering::Relaxed) {
            return;
        }
        let mut owners: BTreeMap<u16, String> = BTreeMap::new();
        for (reference, (_, lane)) in lanes {
            let st = lane.st();
            if !st.units.values().any(|rec| rec.host.is_alive()) {
                continue;
            }
            for rec in st.units.values().filter(|rec| rec.host.is_alive()) {
                for port in rec.ports.values().copied().chain(rec.literal_port) {
                    owners.insert(port, reference.clone());
                }
            }
            for container in crate::model::container_summaries(&st) {
                // Shared infra serves other lanes too; its traffic says nothing
                // about this one.
                if !container.shared {
                    for port in container.ports.values() {
                        owners.insert(*port, reference.clone());
                    }
                }
            }
        }
        if owners.is_empty() {
            knock.seen.clear();
            return;
        }
        let read = tokio::task::spawn_blocking(devlanes_sys::tcp_connections).await;
        let connections = match read {
            Ok(Ok(connections)) => connections,
            failed => {
                let error = match failed {
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => error.to_string(),
                    Ok(Ok(_)) => String::new(),
                };
                // Without the table only proxy traffic, CPU and commands thaw a
                // frozen lane, which is worth one warning.
                if knock.warned {
                    tracing::debug!(%error, "TCP table unreadable");
                } else {
                    knock.warned = true;
                    tracing::warn!(%error, "TCP table unreadable; connections no longer thaw idle lanes");
                }
                return;
            }
        };
        let mut current: HashMap<String, HashSet<(SocketAddr, SocketAddr)>> = HashMap::new();
        for connection in connections {
            // The server side of a connection to a lane port.
            if let Some(reference) = owners.get(&connection.local.port()) {
                current
                    .entry(reference.clone())
                    .or_default()
                    .insert((connection.local, connection.remote));
            }
        }
        for (reference, connections) in &current {
            let Some(previous) = knock.seen.get(reference) else {
                continue;
            };
            if connections.iter().any(|c| !previous.contains(c)) {
                self.opts.idle.activity(reference, Activity::Connection);
            }
        }
        // Lanes without connections now keep an empty baseline, so their next
        // connection counts as new.
        let mut seen = current;
        for reference in owners.values() {
            seen.entry(reference.clone()).or_default();
        }
        knock.seen = seen;
    }

    /// Records CPU activity for lanes whose services used more than
    /// [`CPU_ACTIVE_PERCENT`] together in a sample newer than the last one seen.
    /// Frozen lanes are skipped: their processes use no CPU, so a reading for
    /// them was taken before the freeze.
    fn cpu_activity(&self, lanes: &[(String, LanePair)], seen: &mut HashMap<String, u64>) {
        let names: Vec<(String, bool, Vec<String>)> = lanes
            .iter()
            .map(|(reference, (_, lane))| {
                let st = lane.st();
                let frozen = st
                    .power
                    .as_ref()
                    .is_some_and(|power| power.level == Power::Frozen);
                let units = st
                    .units
                    .iter()
                    .filter(|(_, rec)| rec.kind == Kind::Service && rec.host.is_alive())
                    .map(|(name, _)| name.clone())
                    .collect();
                (reference.clone(), frozen, units)
            })
            .collect();
        let busy: Vec<String> = {
            let metrics = lock(&self.metrics);
            let mut busy = Vec::new();
            for (reference, frozen, units) in names {
                let after = seen.get(&reference).copied().unwrap_or(0);
                let mut newest = after;
                let mut cpu = 0.0;
                for unit in units {
                    if let Some(reading) = metrics.cpu_readings.get(&(reference.clone(), unit))
                        && reading.seq > after
                    {
                        newest = newest.max(reading.seq);
                        cpu += reading.cpu_percent;
                    }
                }
                if newest > after {
                    seen.insert(reference.clone(), newest);
                    if cpu >= CPU_ACTIVE_PERCENT && !frozen {
                        busy.push(reference);
                    }
                }
            }
            busy
        };
        for reference in busy {
            self.opts.idle.activity(&reference, Activity::Cpu);
        }
    }

    /// `debug.freeze`: freezes the request's lane now, whatever its timers say.
    pub(crate) async fn debug_freeze_method(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let (project, lane) = self.target_lane(context).await?;
        self.make_idle(&project, &lane, Power::Frozen, None).await?;
        Ok(json!({"lane": lane.st().reference(&project.name()), "level": "frozen"}))
    }

    /// `debug.thaw`: runs the request's lane normally again; counts as activity.
    pub(crate) async fn debug_thaw_method(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let (project, lane) = self.target_lane(context).await?;
        let thawed = self
            .wake_lane(&project, &lane, Some(Activity::Command))
            .await;
        Ok(json!({"lane": lane.st().reference(&project.name()), "thawed": thawed}))
    }
}
