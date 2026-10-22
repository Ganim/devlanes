//! In-memory registry of projects, lanes and units. SQLite holds the durable part
//! (identities, indices, ports, history); live process state and client
//! environments exist only here.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

use devlanes_config::ProjectConfig;
use devlanes_core::{ProjectId, StopPolicy, UnitRunId, WorktreeId, env::Environment};
use devlanes_proto::{Actor, ActorKind, LaneSummary, RpcError, UnitSummary};
use devlanes_resolve::RouteSpec;
use devlanes_state::{Allocation, DesiredState, Lane};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::units::{UnitHost, UnitState};

/// Client environments; values may hold tokens and stay in memory.
pub(crate) type EnvMap = BTreeMap<String, String>;

/// Locks a std mutex, recovering the data if a panicking holder poisoned it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What kind of process a unit is; clients group and filter on the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Service,
    Task,
    Exec,
}

impl Kind {
    /// Stable word for `UnitSummary.kind`.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Task => "task",
            Self::Exec => "exec",
        }
    }
}

/// Config of one lane's worktree.
#[derive(Clone)]
pub(crate) enum ConfigState {
    /// The worktree has no `devlanes.yaml` at the project's relative path.
    Missing,
    Loaded(Arc<ProjectConfig>),
    /// Parse or validation failure, already shaped as `config_error` data.
    Invalid(RpcError),
}

impl ConfigState {
    /// The loaded config, if any.
    pub(crate) fn loaded(&self) -> Option<&Arc<ProjectConfig>> {
        match self {
            Self::Loaded(config) => Some(config),
            _ => None,
        }
    }
}

/// One service replica as laid out for this lane (ports, URLs, routes).
#[derive(Debug, Clone)]
pub(crate) struct ServiceInfo {
    pub name: String,
    pub unit: String,
    pub port: u16,
    pub ports: BTreeMap<String, u16>,
    pub url: String,
    pub public_url: Option<String>,
    pub tags: Vec<String>,
    pub hidden: bool,
    pub autostart: bool,
    /// Palette name from the layout.
    pub color: String,
    /// `services.<name>.name`, for `UnitSummary.display_name`.
    pub display_name: Option<String>,
}

/// One `infra.hosts` route to a container's published host port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InfraRoute {
    pub host: String,
    pub container: String,
    pub port: u16,
    pub fallback: bool,
    pub host_header_upstream: bool,
}

/// Synchronous layout of a lane, available while it is idle.
#[derive(Debug, Clone)]
pub(crate) struct LaneView {
    pub domain: String,
    /// `lane.port`.
    pub port: u16,
    /// Evaluated `lanes.vars`, for infra expressions.
    pub vars: indexmap::IndexMap<String, devlanes_config::Scalar>,
    pub block: (u16, u16),
    pub services: Vec<ServiceInfo>,
    pub routes: Vec<RouteSpec>,
    /// Routes to containers from `infra.hosts`.
    pub infra_routes: Vec<InfraRoute>,
    pub lane_env: EnvMap,
    pub svc: Value,
}

/// A spawned unit and its static facts.
pub(crate) struct UnitRec {
    pub host: UnitHost,
    pub kind: Kind,
    pub run_id: Option<UnitRunId>,
    pub ports: BTreeMap<String, u16>,
    pub urls: Vec<String>,
    pub tags: Vec<String>,
    pub hidden: bool,
    pub autostart: bool,
    pub stop: StopPolicy,
    pub literal_port: Option<u16>,
    /// Effective environment with provenance for `env --explain`.
    pub env: Arc<Environment>,
    /// Last log lines captured when the unit failed, for proxy pages and errors.
    pub failure_tail: Vec<String>,
    pub preset: Option<String>,
    /// Contributor-prepared files, kept for cross-lane launch checks.
    pub files: Vec<devlanes_core::contrib::PreparedFile>,
    /// Palette name for `UnitSummary.color`.
    pub color: Option<String>,
    /// The runner's handle, for what the run left behind once it ended.
    pub managed: Arc<dyn crate::integrations::ManagedUnit>,
    /// `services.<name>.sync` entries mirrored while this run lasts.
    pub syncs: Vec<crate::local_sync::LocalSync>,
    /// `services.<name>.name` of a service run.
    pub display_name: Option<String>,
    /// The readiness check word of a service run (`started`, `tcp`, ...).
    pub ready_check: Option<&'static str>,
    /// `UnitSummary.role`.
    pub role: &'static str,
}

impl UnitRec {
    /// Processes the ended run left running or that escaped its final sweep;
    /// empty while it runs.
    pub(crate) fn leftovers(&self, unit: &str) -> devlanes_proto::Leftovers {
        let details = self.managed.exit_details();
        devlanes_proto::Leftovers {
            unit: unit.to_owned(),
            left_running: details
                .as_ref()
                .map(|d| d.left_running.clone())
                .unwrap_or_default(),
            escaped: details.map(|d| d.escaped).unwrap_or_default(),
        }
    }
}

/// A process a previous daemon started that still runs (identity verified at
/// startup). Not supervised: no output, input or exit status, only liveness by
/// identity, until a stop terminates it or it exits on its own.
#[derive(Debug, Clone)]
pub(crate) struct Orphan {
    pub run_id: UnitRunId,
    pub process: devlanes_core::ProcessIdentity,
    /// When the previous daemon started it (Unix ms).
    pub started_at_ms: u64,
    /// When this daemon found it (Unix ms).
    pub since_ms: u64,
    pub actor: Actor,
    /// Descendants that left the leader's session, as the previous daemon recorded
    /// them; signalled with the leader and counted for liveness.
    pub members: Vec<devlanes_core::ProcessIdentity>,
}

/// Mutable state of one worktree lane.
pub(crate) struct LaneSt {
    pub identity: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub present: bool,
    /// `None` while unassigned (no index, ports or hosts).
    pub record: Option<Lane>,
    pub unassigned: Option<RpcError>,
    pub config: ConfigState,
    pub config_changed: bool,
    pub env: Option<Arc<EnvMap>>,
    pub overrides: EnvMap,
    /// Profiles of the last start, in the order given; empty means the default.
    pub profiles: Vec<String>,
    pub units: BTreeMap<String, UnitRec>,
    pub started_at_ms: Option<u64>,
    pub busy: Option<&'static str>,
    pub view: Option<Arc<LaneView>>,
    pub allocation: Option<Allocation>,
    pub infra_ns: Option<Value>,
    /// Running containers behind `infra_ns`, for Docker stats.
    pub infra_containers: Vec<crate::integrations::InfraContainer>,
    pub setup_env: EnvMap,
    pub last_error: Option<RpcError>,
    pub skipped_tasks: Vec<String>,
    /// Per-unit peaks for the current lane start: `(memory, cpu)`.
    pub peaks: BTreeMap<String, (Option<u64>, Option<f64>)>,
    /// Set by the moves check: the index this lane takes at its next start.
    pub moves_to: Option<u32>,
    /// Why a selected unit did not start: a lane phase before services failed.
    /// Shown as the unit's `reason` until a start reaches the services phase.
    pub phase_reasons: BTreeMap<String, RpcError>,
    /// Units left running by a previous daemon, by unit name.
    pub orphans: BTreeMap<String, Orphan>,
    /// The start (or restart) running on this lane, while it runs. Stops reach it
    /// here instead of waiting for `op`, which it holds. Each start installs its
    /// own, so nothing done to one reaches a start that begins later.
    pub start: Option<Arc<StartCtl>>,
    /// Spawns in flight ([`SpawnGuard`]). A stop waits until none is left before
    /// it picks its targets, so no unit appears after it looked.
    pub spawning: usize,
    /// Woken whenever `spawning` drops.
    pub spawned: Arc<Notify>,
    /// The config needs ports the block lacks (or another block size or start)
    /// while the lane runs something; the block moves at the next whole-lane
    /// start (plan 5.8).
    pub ports_pending: bool,
    /// `host_conflict` warnings from the latest route snapshot: hostnames of this
    /// lane another lane routes.
    pub host_conflicts: Vec<devlanes_proto::LaneWarning>,
    /// A block or view refresh waits for the operation holding `op` (normally a
    /// start between taking it and its last spawn); a task holding `op` next
    /// runs it.
    pub refresh_pending: bool,
    /// Held shared by `exec` and `run` (they take no `op`, so a start never waits
    /// for a task) and exclusively by `lane clean` and `daemon reset-state` while
    /// they release the lane, so no ad-hoc unit appears in a lane whose records
    /// are going away.
    pub adhoc_gate: Arc<tokio::sync::RwLock<()>>,
    /// Parent of the tokens of this lane's `run`s; a clean or reset fires it.
    pub adhoc_cancel: Option<CancellationToken>,
    /// Set while a clean or reset waits for or holds `adhoc_gate`.
    pub closing: bool,
    /// The config files as of the last reload; `None` before the first one.
    pub config_stamp: Option<crate::registry::ConfigStamp>,
    /// Set while an explicit daemon shutdown runs this lane's `lane.stop`
    /// tasks: their spawns pass the refusal that starts with the shutdown.
    pub shutdown_hooks: bool,
    /// devlanes ran in this worktree (or `lane new` made it), so it gets a lane.
    /// Worktrees that merely exist get none, except the primary one.
    pub wanted: bool,
    /// A clean of this lane, whose worktree is gone, is under way or scheduled.
    pub sweeping: bool,
    /// When a sweep first saw the folder gone; cleared once it is back.
    pub missing_since: Option<std::time::Instant>,
}

/// Marks a worktree identity as an extra lane of another worktree's folder:
/// `<base identity>#lane=<name>`.
pub(crate) const ALIAS_MARK: &str = "#lane=";

impl LaneSt {
    /// A lane in its initial, unassigned state.
    pub(crate) fn new(
        identity: String,
        path: PathBuf,
        branch: Option<String>,
        present: bool,
    ) -> Self {
        Self {
            identity,
            path,
            branch,
            present,
            record: None,
            unassigned: None,
            config: ConfigState::Missing,
            config_changed: false,
            env: None,
            overrides: EnvMap::new(),
            profiles: Vec::new(),
            units: BTreeMap::new(),
            started_at_ms: None,
            busy: None,
            view: None,
            allocation: None,
            infra_ns: None,
            infra_containers: Vec::new(),
            setup_env: EnvMap::new(),
            last_error: None,
            skipped_tasks: Vec::new(),
            peaks: BTreeMap::new(),
            moves_to: None,
            phase_reasons: BTreeMap::new(),
            orphans: BTreeMap::new(),
            start: None,
            spawning: 0,
            spawned: Arc::new(Notify::new()),
            ports_pending: false,
            host_conflicts: Vec::new(),
            refresh_pending: false,
            adhoc_gate: Arc::new(tokio::sync::RwLock::new(())),
            adhoc_cancel: None,
            closing: false,
            config_stamp: None,
            shutdown_hooks: false,
            wanted: false,
            sweeping: false,
            missing_since: None,
        }
    }

    /// For an extra lane of another worktree's folder (`lane new --name`): the
    /// base worktree's identity and this lane's name.
    pub(crate) fn alias(&self) -> Option<(&str, &str)> {
        self.identity.rsplit_once(ALIAS_MARK)
    }

    /// Whether status, lane lists and events show this lane: it has a lane
    /// record, or devlanes ran in its present worktree and it waits for one
    /// (`lanes.max`, a full port pool, a config error). A worktree that merely
    /// exists, or whose folder is gone without a lane, is not a lane.
    pub(crate) fn listed(&self) -> bool {
        self.record.is_some() || (self.present && self.wanted)
    }

    /// Lane index while assigned.
    pub(crate) fn index(&self) -> Option<u32> {
        self.record.as_ref().map(|lane| lane.index)
    }

    /// True while any unit's process has not exited.
    pub(crate) fn any_alive(&self) -> bool {
        self.units.values().any(|unit| unit.host.is_alive())
    }

    /// True while a supervised unit or a previous daemon's orphan runs here.
    pub(crate) fn any_running(&self) -> bool {
        self.any_alive() || !self.orphans.is_empty()
    }

    /// `ports_pending` and `host_conflict` warnings for the lane's summary.
    pub(crate) fn warnings(&self, reference: &str) -> Vec<devlanes_proto::LaneWarning> {
        let mut warnings = Vec::new();
        if self.ports_pending {
            warnings.push(devlanes_proto::LaneWarning {
                code: "ports_pending".into(),
                data: json!({
                    "lane": reference,
                    "message": format!(
                        "the edited config needs a different port block; running units keep their ports, and the lane gets the new block when it starts again after `devlanes stop --lane {reference}`"
                    ),
                }),
            });
        }
        warnings.extend(self.host_conflicts.iter().cloned());
        warnings
    }

    /// `unit_orphaned` when `name` is a previous daemon's process still running:
    /// it names the pid and the command that stops it.
    pub(crate) fn orphan_error(&self, project: &str, name: &str) -> Option<RpcError> {
        let orphan = self.orphans.get(name)?;
        let lane = self.reference(project);
        Some(RpcError::new(
            "unit_orphaned",
            json!({
                "unit": name,
                "lane": lane,
                "pid": orphan.process.pid,
                "started_at_ms": orphan.started_at_ms,
                "orphaned_since_ms": orphan.since_ms,
                "command": format!("devlanes stop {name} --lane {lane}"),
            }),
            1,
        ))
    }

    /// True while any service unit is alive.
    pub(crate) fn services_alive(&self) -> bool {
        self.units
            .values()
            .any(|unit| unit.kind == Kind::Service && unit.host.is_alive())
    }

    /// Canonical reference: `project@index`, or `project@<dir>` while unassigned.
    pub(crate) fn reference(&self, project: &str) -> String {
        match self.index() {
            Some(index) => format!("{project}@{index}"),
            None => format!("{project}@{}", dir_name(&self.path)),
        }
    }

    /// Lifecycle word for `LaneSummary.state`.
    pub(crate) fn state_word(&self) -> &'static str {
        if self.record.is_none() && matches!(self.config, ConfigState::Missing) {
            return "no-config";
        }
        if self.record.is_none() {
            return "unassigned";
        }
        if let Some(busy) = self.busy {
            return busy;
        }
        let failed = self.units.values().any(|unit| {
            unit.kind == Kind::Service && matches!(unit.host.state(), UnitState::Failed(_))
        });
        if failed {
            return "failed";
        }
        if self.any_running() {
            "running"
        } else {
            "idle"
        }
    }

    /// Whether `start`/`resume` from a shell is needed before services can run.
    pub(crate) fn needs_env(&self) -> bool {
        self.env.is_none()
            && !self.any_running()
            && self
                .record
                .as_ref()
                .is_some_and(|lane| lane.desired_state == DesiredState::Running)
    }
}

/// Color of a unit without config tags of its own (exec units, containers,
/// infra commands): from its name and the project's `tags:` colors.
pub(crate) fn derived_color(lane: &LaneSt, name: &str) -> String {
    let overrides = lane
        .config
        .loaded()
        .map(|config| devlanes_resolve::tag_colors(config))
        .unwrap_or_default();
    devlanes_core::color::unit_color(&[], name, None, &overrides).to_owned()
}

/// Directory name used in references of unassigned lanes.
pub(crate) fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// One worktree's lane plus the lock that serializes start/stop/clean on it.
pub(crate) struct LaneRt {
    pub worktree: WorktreeId,
    pub op: tokio::sync::Mutex<()>,
    pub st: Mutex<LaneSt>,
    /// Held while a `lane.changed` summary is built and emitted, so summaries
    /// reach the bus in the order they were taken.
    pub publish: Mutex<()>,
    /// Stops the config watcher when the lane goes away.
    pub watch: tokio_util::sync::CancellationToken,
}

impl LaneRt {
    /// Locks the lane state for a short critical section.
    pub(crate) fn st(&self) -> MutexGuard<'_, LaneSt> {
        lock(&self.st)
    }

    /// Interrupts the start running on this lane, if any. Called before waiting
    /// for `op`, which that start holds.
    pub(crate) fn cancel_start(&self) {
        if let Some(start) = &self.st().start {
            start.cancel.cancel();
        }
    }

    /// The start running on this lane, if any.
    pub(crate) fn running_start(&self) -> Option<Arc<StartCtl>> {
        self.st().start.clone()
    }

    /// Installs a start's control for as long as the returned slot lives. Called
    /// right after the start took `op`.
    pub(crate) fn begin_start(self: &Arc<Self>, shutdown: &CancellationToken) -> StartSlot {
        let ctl = Arc::new(StartCtl::new(shutdown));
        self.st().start = Some(ctl.clone());
        StartSlot {
            lane: self.clone(),
            ctl,
        }
    }

    /// Counts a spawn in flight until the guard drops.
    pub(crate) fn spawn_guard(self: &Arc<Self>) -> SpawnGuard {
        self.st().spawning += 1;
        SpawnGuard(self.clone())
    }

    /// For `exec` and `run`: waits out a clean or reset of this lane, then holds
    /// the ad-hoc gate shared. The token fires when a clean or reset of the lane
    /// begins, or with `shutdown`.
    pub(crate) async fn enter_adhoc(
        &self,
        shutdown: &CancellationToken,
    ) -> (tokio::sync::OwnedRwLockReadGuard<()>, CancellationToken) {
        loop {
            let gate = self.st().adhoc_gate.clone();
            let guard = gate.read_owned().await;
            {
                let mut st = self.st();
                if !st.closing {
                    let parent = st
                        .adhoc_cancel
                        .get_or_insert_with(|| shutdown.child_token());
                    return (guard, parent.child_token());
                }
            }
            // A closer marked the lane but has not queued for the gate yet; once it
            // has, the next read waits for it to finish.
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// For `lane clean` and `daemon reset-state`, which hold `op`: interrupts the
    /// lane's runs, then waits until no `exec` or `run` holds the ad-hoc gate.
    /// The lane opens again when the returned value drops.
    pub(crate) async fn close_adhoc(self: &Arc<Self>) -> AdhocClosed {
        let gate = {
            let mut st = self.st();
            st.closing = true;
            if let Some(token) = st.adhoc_cancel.take() {
                token.cancel();
            }
            st.adhoc_gate.clone()
        };
        AdhocClosed {
            lane: self.clone(),
            _guard: gate.write_owned().await,
        }
    }

    /// Waits until no spawn is in flight on this lane.
    pub(crate) async fn spawns_settled(&self) {
        loop {
            let spawned = self.st().spawned.clone();
            let notified = spawned.notified();
            tokio::pin!(notified);
            // Registered before the check, so a drop in between still wakes us.
            notified.as_mut().enable();
            if self.st().spawning == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// A lane closed to `exec` and `run` ([`LaneRt::close_adhoc`]).
pub(crate) struct AdhocClosed {
    lane: Arc<LaneRt>,
    _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
}

impl Drop for AdhocClosed {
    /// Lets `exec` and `run` in again.
    fn drop(&mut self) {
        self.lane.st().closing = false;
    }
}

/// One spawn in flight on a lane, from just before its last cancellation check
/// until its unit is recorded (or the spawn gave up).
pub(crate) struct SpawnGuard(Arc<LaneRt>);

impl Drop for SpawnGuard {
    /// Uncounts the spawn and wakes waiting stops.
    fn drop(&mut self) {
        let spawned = {
            let mut st = self.0.st();
            st.spawning = st.spawning.saturating_sub(1);
            st.spawned.clone()
        };
        spawned.notify_waiters();
    }
}

/// What a `stop` or `restart` of single units can do to the start (or restart)
/// running on their lane without waiting for its `op`.
pub(crate) struct StartCtl {
    /// Ends the start as `start_cancelled`: a whole-lane stop, clean, reset or
    /// shutdown fires it, and so does a stop of the task the start runs.
    pub cancel: CancellationToken,
    /// Fires once the start spawns no more services: its services phase is over
    /// or the start ended.
    pub services_done: CancellationToken,
    /// Units a stop (`"stop"`) or restart (`"restart"`) took out of the start:
    /// it neither spawns nor waits for them and reports them as stopped.
    withdrawn: Mutex<BTreeMap<String, &'static str>>,
    /// Task units the start runs right now (hook tasks, their steps, setup
    /// entries), counted.
    tasks: Mutex<BTreeMap<String, usize>>,
}

impl StartCtl {
    /// A control whose token fires with `parent`.
    pub(crate) fn new(parent: &CancellationToken) -> Self {
        Self {
            cancel: parent.child_token(),
            services_done: CancellationToken::new(),
            withdrawn: Mutex::new(BTreeMap::new()),
            tasks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Takes `units` out of the start on behalf of `by`.
    pub(crate) fn withdraw(&self, units: &[String], by: &'static str) {
        let mut withdrawn = lock(&self.withdrawn);
        for unit in units {
            withdrawn.entry(unit.clone()).or_insert(by);
        }
    }

    /// Who took `unit` out of the start, if anybody did.
    pub(crate) fn withdrawn_by(&self, unit: &str) -> Option<&'static str> {
        lock(&self.withdrawn).get(unit).copied()
    }

    /// Every withdrawn unit and who withdrew it.
    pub(crate) fn withdrawn(&self) -> BTreeMap<String, &'static str> {
        lock(&self.withdrawn).clone()
    }

    /// Marks task units as running in this start until the guard drops.
    pub(crate) fn running_tasks(self: &Arc<Self>, units: Vec<String>) -> TaskGuard {
        let mut tasks = lock(&self.tasks);
        for unit in &units {
            *tasks.entry(unit.clone()).or_default() += 1;
        }
        TaskGuard {
            ctl: self.clone(),
            units,
        }
    }

    /// Whether the start runs a task unit named `unit` right now.
    pub(crate) fn runs_task(&self, unit: &str) -> bool {
        lock(&self.tasks).contains_key(unit)
    }
}

/// Task units a start runs, until dropped ([`StartCtl::running_tasks`]).
pub(crate) struct TaskGuard {
    ctl: Arc<StartCtl>,
    units: Vec<String>,
}

impl Drop for TaskGuard {
    /// Uncounts the units.
    fn drop(&mut self) {
        let mut tasks = lock(&self.ctl.tasks);
        for unit in &self.units {
            if let Some(count) = tasks.get_mut(unit) {
                *count -= 1;
                if *count == 0 {
                    tasks.remove(unit);
                }
            }
        }
    }
}

/// A start's control installed on its lane ([`LaneRt::begin_start`]).
pub(crate) struct StartSlot {
    lane: Arc<LaneRt>,
    pub ctl: Arc<StartCtl>,
}

impl Drop for StartSlot {
    /// Uninstalls the control, unless a later start replaced it, and releases
    /// restarts waiting for the services phase.
    fn drop(&mut self) {
        self.ctl.services_done.cancel();
        let mut st = self.lane.st();
        if st
            .start
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.ctl))
        {
            st.start = None;
        }
    }
}

/// Mutable state of a project.
pub(crate) struct ProjectSt {
    pub name: String,
    pub root: PathBuf,
    pub common_dir: Option<PathBuf>,
    pub primary_branch: String,
    /// Environment of the last request that carried one, for auto-started worktrees.
    pub last_env: Option<Arc<EnvMap>>,
    pub lanes: BTreeMap<WorktreeId, Arc<LaneRt>>,
    /// Names `project:` had before a rename in this daemon's lifetime, so a
    /// reference with an old name can point at the new one.
    pub former_names: Vec<String>,
}

/// A project: worktrees sharing a git common directory (or one non-git root).
pub(crate) struct ProjectRt {
    pub id: ProjectId,
    /// `devlanes.yaml`'s directory relative to each worktree root.
    pub config_dir: PathBuf,
    pub git: bool,
    /// Serializes re-listing and registration.
    pub sync: tokio::sync::Mutex<()>,
    pub st: Mutex<ProjectSt>,
    pub watching: AtomicBool,
    pub watch: tokio_util::sync::CancellationToken,
}

impl ProjectRt {
    /// Locks the project state for a short critical section.
    pub(crate) fn st(&self) -> MutexGuard<'_, ProjectSt> {
        lock(&self.st)
    }

    /// Display name from the config.
    pub(crate) fn name(&self) -> String {
        self.st().name.clone()
    }

    /// Lanes in worktree order.
    pub(crate) fn lanes(&self) -> Vec<Arc<LaneRt>> {
        self.st().lanes.values().cloned().collect()
    }

    /// Claims the watcher slot; true for the first caller only.
    pub(crate) fn claim_watch(&self) -> bool {
        !self.watching.swap(true, Ordering::SeqCst)
    }

    /// The lane holding `index`, if any.
    pub(crate) fn lane_by_index(&self, index: u32) -> Option<Arc<LaneRt>> {
        self.lanes()
            .into_iter()
            .find(|lane| lane.st().index() == Some(index))
    }
}

/// Converts proto attribution to the core type stored in SQLite.
pub(crate) fn core_actor(actor: &Actor) -> devlanes_core::Actor {
    devlanes_core::Actor {
        kind: match actor.kind {
            ActorKind::Human => devlanes_core::ActorKind::Human,
            ActorKind::Agent => devlanes_core::ActorKind::Agent,
        },
        agent_id: actor.agent_id.clone(),
        session_id: actor.session_id.clone(),
    }
}

/// Converts a stored actor back to the proto type.
pub(crate) fn proto_actor(actor: &devlanes_core::Actor) -> Actor {
    Actor {
        kind: match actor.kind {
            devlanes_core::ActorKind::Human => ActorKind::Human,
            devlanes_core::ActorKind::Agent => ActorKind::Agent,
        },
        agent_id: actor.agent_id.clone(),
        session_id: actor.session_id.clone(),
    }
}

/// True when `unit` was started by the caller's agent session (`--mine`).
pub(crate) fn is_mine(owner: &Actor, caller: &Actor) -> bool {
    match (&owner.session_id, &caller.session_id) {
        (Some(owner), Some(caller)) => owner == caller,
        (None, None) => owner.kind == caller.kind && owner.agent_id == caller.agent_id,
        _ => false,
    }
}

/// Gives a unit that is not running the reason its last start stopped short, so
/// clients can say why it stayed idle without looking at the lane.
pub(crate) fn apply_phase_reason(lane: &LaneSt, summary: &mut UnitSummary) {
    let running = lane
        .units
        .get(&summary.name)
        .is_some_and(|unit| unit.host.is_alive());
    if !running && let Some(reason) = lane.phase_reasons.get(&summary.name) {
        summary.reason = Some(reason.clone());
    }
}

/// Builds the summary of one live or finished unit.
pub(crate) fn unit_summary(lane: &str, name: &str, unit: &UnitRec) -> UnitSummary {
    let state = unit.host.state();
    let exit = unit.host.exit_outcome();
    let word = match (&state, unit.kind) {
        // One-shot units that finished on their own read as `exited`.
        (UnitState::Exited(outcome), Kind::Task | Kind::Exec)
            if !unit.host.stop_was_requested() && outcome.code == Some(0) =>
        {
            "exited"
        }
        _ => state.word(),
    };
    let reason = match &state {
        UnitState::Failed(reason) => reason.clone(),
        _ => None,
    };
    let info = unit.host.info();
    let leftovers = unit.leftovers(name);
    UnitSummary {
        name: name.to_owned(),
        lane: lane.to_owned(),
        state: word.into(),
        pid: unit.host.identity().map(|id| id.pid),
        exit_code: exit.and_then(|outcome| outcome.code),
        ports: unit.ports.clone(),
        urls: unit.urls.clone(),
        actor: info.actor.clone(),
        reason,
        kind: unit.kind.word().into(),
        autostart: unit.autostart,
        hidden: unit.hidden,
        tags: unit.tags.clone(),
        started_at_ms: Some(info.started_at_ms),
        finished_at_ms: unit.host.ended_at_ms(),
        cpu_percent: None,
        memory_bytes: None,
        color: unit.color.clone(),
        orphaned_since_ms: None,
        left_running: leftovers.left_running,
        escaped: leftovers.escaped,
        display_name: unit.display_name.clone(),
        ready_check: unit.ready_check.map(str::to_owned),
        pty: info.pty,
        role: Some(unit.role.to_owned()),
    }
}

/// Summary of a previous daemon's process: `orphaned`, with the configured
/// service's ports and URLs when it is one.
pub(crate) fn orphan_summary(
    lane: &LaneSt,
    reference: &str,
    name: &str,
    orphan: &Orphan,
) -> UnitSummary {
    let service = lane
        .view
        .as_ref()
        .and_then(|view| view.services.iter().find(|s| s.unit == name));
    let mut summary = match service {
        Some(service) => idle_summary(reference, service),
        None => {
            let task = lane
                .config
                .loaded()
                .is_some_and(|config| config.tasks.contains_key(name));
            let kind = if task { "task" } else { "exec" };
            // A previous daemon's setup or teardown entry is known only by its name.
            let role = match name.split_once('.') {
                Some(("setup", _)) => "setup",
                Some(("teardown", _)) => "teardown",
                _ => kind,
            };
            UnitSummary {
                name: name.to_owned(),
                lane: reference.to_owned(),
                role: Some(role.into()),
                kind: kind.into(),
                color: Some(derived_color(lane, name)),
                ..UnitSummary::default()
            }
        }
    };
    summary.state = devlanes_proto::ORPHANED.into();
    summary.pid = Some(orphan.process.pid);
    summary.actor = orphan.actor.clone();
    summary.started_at_ms = Some(orphan.started_at_ms);
    summary.orphaned_since_ms = Some(orphan.since_ms);
    summary
}

/// Summary of a configured service that has not run since the daemon started.
pub(crate) fn idle_summary(lane: &str, service: &ServiceInfo) -> UnitSummary {
    let mut ports = service.ports.clone();
    ports.insert("default".into(), service.port);
    let mut urls = vec![service.url.clone()];
    urls.extend(service.public_url.clone());
    UnitSummary {
        name: service.unit.clone(),
        lane: lane.to_owned(),
        state: "idle".into(),
        ports,
        urls,
        kind: "service".into(),
        autostart: service.autostart,
        hidden: service.hidden,
        tags: service.tags.clone(),
        color: Some(service.color.clone()),
        display_name: service.display_name.clone(),
        role: Some("service".into()),
        ..UnitSummary::default()
    }
}

/// Builds a lane summary; units list configured services (idle or live) then
/// one-shot units.
pub(crate) fn lane_summary(project: &str, lane: &LaneSt) -> LaneSummary {
    let reference = lane.reference(project);
    let mut units = Vec::new();
    if let Some(view) = &lane.view {
        for service in &view.services {
            units.push(
                match (
                    lane.orphans.get(&service.unit),
                    lane.units.get(&service.unit),
                ) {
                    (Some(orphan), _) => orphan_summary(lane, &reference, &service.unit, orphan),
                    (None, Some(unit)) => unit_summary(&reference, &service.unit, unit),
                    (None, None) => idle_summary(&reference, service),
                },
            );
        }
    }
    for (name, orphan) in &lane.orphans {
        if !units.iter().any(|summary| &summary.name == name) {
            units.push(orphan_summary(lane, &reference, name, orphan));
        }
    }
    for (name, unit) in &lane.units {
        if !units.iter().any(|summary| &summary.name == name) {
            units.push(unit_summary(&reference, name, unit));
        }
    }
    for summary in &mut units {
        apply_phase_reason(lane, summary);
    }
    let missing =
        (!lane.present).then(|| RpcError::new("worktree_missing", json!({"path": lane.path}), 3));
    let reason = missing
        .or_else(|| lane.unassigned.clone())
        .or_else(|| match &lane.config {
            ConfigState::Invalid(error) => Some(error.clone()),
            _ => None,
        })
        .or_else(|| lane.last_error.clone());
    let profiles = if lane.profiles.is_empty() {
        lane.record
            .as_ref()
            .map(|r| r.profiles.clone())
            .unwrap_or_default()
    } else {
        lane.profiles.clone()
    };
    LaneSummary {
        warnings: lane.warnings(&reference),
        reference,
        project: project.to_owned(),
        index: lane.index(),
        branch: lane.branch.clone(),
        path: lane.path.clone(),
        state: lane.state_word().into(),
        config_changed: lane.config_changed,
        units,
        profile: profiles.first().cloned(),
        profiles,
        moves_to: lane.moves_to,
        domain: lane.view.as_ref().map(|view| view.domain.clone()),
        port_block: lane.view.as_ref().map(|view| {
            (
                view.block.0,
                view.block.0.saturating_add(view.block.1.saturating_sub(1)),
            )
        }),
        reason,
        needs_env: lane.needs_env(),
        containers: container_summaries(lane),
    }
}

/// Containers from the lane's infra namespace (`{name: {port, ports}}`), set while
/// its infra is up, with `infra.hidden` applied.
fn container_summaries(lane: &LaneSt) -> Vec<devlanes_proto::ContainerSummary> {
    let Some(Value::Object(namespace)) = &lane.infra_ns else {
        return Vec::new();
    };
    let hidden: &[String] = lane
        .config
        .loaded()
        .and_then(|config| config.infra.hidden.as_deref())
        .unwrap_or_default();
    namespace
        .iter()
        .map(|(name, entry)| {
            let mut ports: BTreeMap<String, u16> = entry
                .get("ports")
                .and_then(Value::as_object)
                .map(|ports| {
                    ports
                        .iter()
                        .filter_map(|(key, port)| {
                            let port = u16::try_from(port.as_u64()?).ok()?;
                            Some((key.clone(), port))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if ports.is_empty()
                && let Some(port) = entry
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
            {
                ports.insert("default".into(), port);
            }
            devlanes_proto::ContainerSummary {
                name: name.clone(),
                ports,
                hidden: hidden.contains(name),
                color: Some(derived_color(lane, name)),
                shared: lane
                    .infra_containers
                    .iter()
                    .any(|container| &container.name == name && container.shared),
                cpu_percent: None,
                memory_bytes: None,
                state: lane
                    .infra_containers
                    .iter()
                    .filter(|container| &container.name == name)
                    .filter_map(|container| container.state.clone())
                    .max_by_key(|state| state_badness(state)),
            }
        })
        .collect()
}

/// How bad a container state is, so a service's replicas report the worst.
fn state_badness(state: &str) -> u8 {
    match state.split(' ').next().unwrap_or_default() {
        "running" | "healthy" => 0,
        "starting" | "created" | "restarting" | "paused" => 1,
        _ => 2,
    }
}

/// Shapes a config failure for clients, keeping its source location.
pub(crate) fn config_error(error: &devlanes_config::ConfigError) -> RpcError {
    RpcError::new(
        "config_error",
        json!({
            "message": error.message,
            "file": error.location.file,
            "line": error.location.line,
            "column": error.location.column,
        }),
        1,
    )
}
