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
    /// Routes to containers from `infra.hosts`: `(host, container, port)`.
    pub infra_routes: Vec<(String, String, u16)>,
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
}

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
        }
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
    /// Stops the config watcher when the lane goes away.
    pub watch: tokio_util::sync::CancellationToken,
}

impl LaneRt {
    /// Locks the lane state for a short critical section.
    pub(crate) fn st(&self) -> MutexGuard<'_, LaneSt> {
        lock(&self.st)
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
        cpu_percent: None,
        memory_bytes: None,
        color: unit.color.clone(),
        orphaned_since_ms: None,
        left_running: leftovers.left_running,
        escaped: leftovers.escaped,
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
            UnitSummary {
                name: name.to_owned(),
                lane: reference.to_owned(),
                kind: if task { "task" } else { "exec" }.into(),
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
        (!lane.present).then(|| RpcError::new("worktree_missing", json!({"path": lane.path}), 1));
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
            }
        })
        .collect()
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
