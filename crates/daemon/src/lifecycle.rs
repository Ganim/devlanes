//! Lane lifecycle: unit spawning, readiness, tasks and setup commands, the start
//! pipeline of ordered [`LanePhase`]s (decision 5), stop, restart and clean.
//! Nothing here stops a process implicitly (decision 16) or restarts one (decision 6).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use devlanes_config::{TaskWhen, Templated};
use devlanes_core::{
    ExitOutcome, LaunchSpec, StopPolicy, TerminalSize, Unit, contrib::PreparedFile,
    env::Environment,
};
use devlanes_proto::{
    Actor, Leftovers, LogQuery, RpcError, StartResult, UnitFailureData, UnitSummary, events,
};
use devlanes_resolve::{Launch, ReadySpec, ResolvedCommand, ResolvedLane, ResolvedService};
use devlanes_state::{DesiredState, TaskRunInput};
use futures_util::{
    StreamExt,
    future::{BoxFuture, join_all},
    stream::FuturesUnordered,
};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    Session,
    engine::{Inner, with_ownership},
    integrations::{InfraPortNeed, InfraSpec, ManagedUnit, PlannedUnit},
    model::{
        EnvMap, Kind, LaneRt, ProjectRt, StartCtl, UnitRec, core_actor, is_mine, unit_summary,
    },
    ready::Readiness,
    registry::{internal, store_error},
    units::{UnitHost, UnitInfo, UnitState, now_ms},
};

/// Log lines attached to every failure (cli-design 3.4).
pub(crate) const TAIL: usize = 20;
/// How long a restart waits for the old process's ports to close.
const PORT_RELEASE: Duration = Duration::from_secs(15);
/// How often a single-unit stop waiting for `op` looks for a start that took it.
const START_POLL: Duration = Duration::from_millis(25);
/// Nested `steps` deeper than this are treated as a cycle.
const MAX_STEP_DEPTH: usize = 16;

/// Whether the lane's `lane.stop` and `lane.clean` tasks can run: a lane whose
/// worktree is gone (or that never got an env or record) has no directory or
/// environment to run them in.
fn hooks_runnable(st: &crate::model::LaneSt) -> bool {
    st.present && st.env.is_some() && st.record.is_some()
}

/// Everything needed to spawn one unit.
pub(crate) struct SpawnRequest {
    pub name: String,
    pub kind: Kind,
    pub spec: LaunchSpec,
    pub env: Arc<Environment>,
    pub files: Vec<PreparedFile>,
    pub actor: Actor,
    pub ports: BTreeMap<String, u16>,
    pub urls: Vec<String>,
    pub tags: Vec<String>,
    pub hidden: bool,
    pub autostart: bool,
    pub stop: StopPolicy,
    pub literal_port: Option<u16>,
    pub preset: Option<String>,
    /// Palette name; `None` derives one from the name at spawn.
    pub color: Option<String>,
    /// Folders mirrored while the unit runs (services only).
    pub sync: Vec<devlanes_resolve::SyncSpec>,
}

/// Copies an environment with its provenance (the type is deliberately not `Clone`).
pub(crate) fn copy_env(env: &Environment) -> Environment {
    Environment {
        values: env.values.clone(),
        sources: env.sources.clone(),
        shell_env: env.shell_env.clone(),
    }
}

impl SpawnRequest {
    /// Request for one service replica.
    pub(crate) fn service(service: &ResolvedService, actor: &Actor) -> Self {
        let mut ports = service.ports.clone();
        ports.insert("default".into(), service.port);
        let mut urls = vec![service.url.clone()];
        urls.extend(service.public_url.clone());
        Self {
            name: service.unit.clone(),
            kind: Kind::Service,
            spec: service.launch.to_spec(service.pty, TerminalSize::default()),
            env: Arc::new(copy_env(&service.launch.env)),
            files: service.launch.files.clone(),
            actor: actor.clone(),
            ports,
            urls,
            tags: service.tags.clone(),
            hidden: service.hidden,
            autostart: service.autostart,
            stop: service.stop,
            literal_port: service.literal_port.then_some(service.port),
            preset: service.preset.clone(),
            color: Some(service.color.clone()),
            sync: service.sync.clone(),
        }
    }

    /// Request for a one-shot unit (task or setup/teardown entry).
    pub(crate) fn oneshot(
        name: &str,
        kind: Kind,
        launch: &Launch,
        pty: bool,
        extra: &EnvMap,
        actor: &Actor,
    ) -> Self {
        let mut spec = launch.to_spec(pty, TerminalSize::default());
        spec.env
            .extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        Self {
            name: name.to_owned(),
            kind,
            spec,
            env: Arc::new(copy_env(&launch.env)),
            files: launch.files.clone(),
            actor: actor.clone(),
            ports: BTreeMap::new(),
            urls: Vec::new(),
            tags: Vec::new(),
            hidden: false,
            autostart: false,
            stop: StopPolicy::default(),
            literal_port: None,
            preset: None,
            color: None,
            sync: Vec::new(),
        }
    }
}

/// Writes contributor files atomically. Paths must stay inside the lane worktree;
/// the files must not contain secrets because they land on disk.
async fn write_files(
    lane_path: std::path::PathBuf,
    files: Vec<PreparedFile>,
) -> Result<(), RpcError> {
    if files.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        for file in files {
            let path = if file.path.is_absolute() {
                file.path.clone()
            } else {
                lane_path.join(&file.path)
            };
            if !devlanes_core::paths::path_starts_with(&path, &lane_path)
                || path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(RpcError::new(
                    "prepared_file_outside_lane",
                    json!({"path": path}),
                    1,
                ));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| internal("io", e))?;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let temp = path.with_file_name(format!(".{name}.devlanes-tmp"));
            std::fs::write(&temp, &file.contents).map_err(|e| internal("io", e))?;
            std::fs::rename(&temp, &path).map_err(|e| internal("io", e))?;
        }
        Ok(())
    })
    .await
    .map_err(|e| internal("io", e))?
}

/// Maps a resolved readiness spec to the checker's form. `env` is the unit's launch
/// environment, which a `cmd` probe runs with through `shell`.
pub(crate) fn readiness(
    spec: &ReadySpec,
    env: BTreeMap<String, String>,
    shell: &devlanes_core::shell::Shell,
) -> Readiness {
    match spec {
        ReadySpec::Started => Readiness::Started,
        ReadySpec::Tcp { port } => Readiness::Tcp { port: *port },
        ReadySpec::Http { port, path, status } => Readiness::Http {
            port: *port,
            path: path.clone(),
            status: *status,
        },
        ReadySpec::Log { pattern } => Readiness::Log {
            pattern: pattern.clone(),
        },
        ReadySpec::Cmd { shell: text, cwd } => Readiness::Cmd {
            command: text.clone(),
            cwd: cwd.clone(),
            env,
            shell: shell.clone(),
        },
    }
}

/// True once the state no longer moves toward ready on its own.
fn settled(state: &UnitState) -> bool {
    matches!(
        state,
        UnitState::Ready | UnitState::Failed(_) | UnitState::Exited(_)
    )
}

/// Waits until every host is ready, failed or exited; false on deadline.
pub(crate) async fn wait_settled(hosts: &[UnitHost], deadline: Instant) -> bool {
    for host in hosts {
        let mut watch = host.watch_state();
        loop {
            if settled(&watch.borrow_and_update()) {
                break;
            }
            match tokio::time::timeout_at(deadline, watch.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => return false,
            }
        }
    }
    true
}

/// Failure data for a unit: exit code, reason and last log lines.
pub(crate) async fn failure_of(lane: &str, name: &str, host: &UnitHost) -> UnitFailureData {
    let reason = match host.state() {
        UnitState::Failed(reason) => reason,
        _ => None,
    };
    UnitFailureData {
        lane: lane.to_owned(),
        unit: name.to_owned(),
        exit_code: host.exit_outcome().and_then(|o| o.code),
        log_tail: host.tail(TAIL).await,
        reason,
    }
}

/// What the ended run behind `host` left behind; `None` when nothing was or a
/// newer run of `unit` replaced it.
pub(crate) fn ended_leftovers(lane: &LaneRt, unit: &str, host: &UnitHost) -> Option<Leftovers> {
    let st = lane.st();
    let rec = st.units.get(unit).filter(|rec| rec.host.same_run(host))?;
    Some(rec.leftovers(unit)).filter(|leftovers| !leftovers.is_empty())
}

/// Outcome of one pipeline phase.
pub(crate) enum PhaseResult {
    Done,
    /// Not applicable; the reason travels in the `lane.phase` event.
    Skipped(RpcError),
    /// Failed with failures recorded in the result; the start returns normally.
    Halt(RpcError),
    /// Failed in a way that turns the start into an error response.
    Error(RpcError),
}

impl PhaseResult {
    /// The `state` and `reason` of the phase's closing `lane.phase` event.
    fn event_state(&self) -> (&'static str, Option<&RpcError>) {
        match self {
            Self::Done => ("done", None),
            Self::Skipped(why) => ("skipped", Some(why)),
            Self::Halt(why) | Self::Error(why) => ("failed", Some(why)),
        }
    }
}

/// Mutable context threaded through the start pipeline.
pub(crate) struct StartCx {
    pub project: Arc<ProjectRt>,
    pub lane: Arc<LaneRt>,
    pub reference: String,
    pub actor: Actor,
    pub services: Vec<String>,
    /// Profiles in effect, in the order given.
    pub profiles: Vec<String>,
    /// False when the profiles turn infra off. Set from the selection before infra
    /// (literal values), then from [`ResolvedLane::infra_enabled`] once resolved.
    pub infra_enabled: bool,
    /// Some profile's `infra` is an expression, so only resolution can decide.
    pub infra_templated: bool,
    pub whole_lane: bool,
    pub setup_only: bool,
    pub session: Option<Session>,
    pub resolved: Option<Arc<ResolvedLane>>,
    pub deadline: Instant,
    pub started: Vec<String>,
    pub already_running: Vec<String>,
    pub failures: Vec<UnitFailureData>,
    pub skipped_tasks: Vec<String>,
    /// Tasks and setup commands of this start that left processes behind.
    pub leftovers: Vec<Leftovers>,
    pub infra_needs: Vec<InfraPortNeed>,
    pub timed_out: bool,
    /// Bypass the start gate (`Selection.all_at_once`).
    pub all_at_once: bool,
    /// On this start's `lane.phase` events and its result.
    pub start_id: u64,
    /// Fired by a stop, clean or reset of the lane (or daemon shutdown); phases
    /// end early and stop the task they run. `ctl.cancel`.
    pub cancel: CancellationToken,
    /// What single-unit stops and restarts did to this start: withdrawn units
    /// are skipped, not waited for and reported as stopped.
    pub ctl: Arc<StartCtl>,
    /// The config the services were selected from. Resolution uses it too, so a
    /// reload in the middle of the start cannot change what it plans.
    pub config: Arc<devlanes_config::ProjectConfig>,
    /// Place in the start gate's line, taken when the request arrived.
    pub arrival: Option<devlanes_core::gate::Arrival>,
}

/// One ordered step of the lane pipeline (plan 5.5).
#[async_trait]
pub(crate) trait LanePhase: Send + Sync {
    /// Name in `lane.phase` events; CI log groups use it.
    fn name(&self) -> &'static str;
    /// Internal steps (resolution) emit no events.
    fn visible(&self) -> bool {
        true
    }
    /// Runs the phase.
    async fn run(&self, engine: &Arc<Inner>, cx: &mut StartCx) -> PhaseResult;
}

/// Brings shared and per-lane infra up (skipped by `infra: false`).
struct InfraPhase;
/// Plans every unit once infra endpoints are known.
struct ResolvePhase;
/// `infra.setup`: first time per lane, named entries in parallel.
struct SetupPhase;
/// `lane.create`/`lane.start` tasks before or after services, in file order.
struct TasksPhase {
    after: bool,
}
/// All selected services together.
struct ServicesPhase;

/// The start pipeline in decision-5 order.
fn start_pipeline() -> Vec<Box<dyn LanePhase>> {
    vec![
        Box::new(InfraPhase),
        Box::new(ResolvePhase),
        Box::new(SetupPhase),
        Box::new(TasksPhase { after: false }),
        Box::new(ServicesPhase),
        Box::new(TasksPhase { after: true }),
    ]
}

/// A reason value for skipped phases.
fn reason(code: &str, data: Value) -> RpcError {
    RpcError::new(code, data, 0)
}

/// Unit names running `task` can spawn: the task itself, its steps' tasks and
/// the setup and teardown entries they name.
fn task_units(resolved: &ResolvedLane, task: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    let mut pending = vec![task.to_owned()];
    while let Some(name) = pending.pop() {
        // Also ends step cycles, which `run_task` refuses by depth.
        if units.contains(&name) {
            continue;
        }
        let steps = resolved.tasks.get(&name).map(|t| t.steps.as_slice());
        for step in steps.unwrap_or_default() {
            if let Some(entry) = step.strip_prefix("infra.setup.") {
                units.push(format!("setup.{entry}"));
            } else if let Some(entry) = step.strip_prefix("infra.teardown.") {
                units.push(format!("teardown.{entry}"));
            } else {
                pending.push(step.clone());
            }
        }
        units.push(name);
    }
    units
}

/// The reason on a unit a `stop` (`by`: `stop`) or `restart` took out of a start.
pub(crate) fn withdrawn_reason(lane: &str, unit: &str, by: &str) -> RpcError {
    RpcError::new(
        "cancelled_by_stop",
        json!({"lane": lane, "unit": unit, "by": by}),
        0,
    )
}

/// A stop, clean or reset of the lane (or daemon shutdown) interrupted the start.
fn start_cancelled(cx: &StartCx) -> RpcError {
    RpcError::new("start_cancelled", json!({"lane": cx.reference}), 1)
}

/// Why infra and setup were skipped for the lane's profiles.
fn profile_disables_infra(cx: &StartCx) -> RpcError {
    reason(
        "profile_disables_infra",
        json!({"profile": cx.profiles.first(), "profiles": cx.profiles}),
    )
}

/// Infra failures as RPC errors.
fn infra_error(error: crate::integrations::InfraError) -> RpcError {
    RpcError::from(error)
}

#[async_trait]
impl LanePhase for InfraPhase {
    fn name(&self) -> &'static str {
        "infra"
    }

    async fn run(&self, engine: &Arc<Inner>, cx: &mut StartCx) -> PhaseResult {
        let empty_spec = match engine.infra_spec(&cx.project, &cx.lane, &[]).await {
            Ok(spec) => spec,
            Err(error) => return PhaseResult::Error(error),
        };
        if empty_spec.is_empty() {
            return PhaseResult::Skipped(reason("no_infra", json!({})));
        }
        let needs = match engine.integ.infra.port_needs(&empty_spec).await {
            Ok(needs) => needs,
            Err(error) => return PhaseResult::Error(infra_error(error)),
        };
        if let Err(error) = engine
            .allocate_lane_held(&cx.project, &cx.lane, &needs)
            .await
        {
            return PhaseResult::Error(error);
        }
        cx.infra_needs = needs;
        let spec = match engine
            .infra_spec(&cx.project, &cx.lane, &cx.infra_needs)
            .await
        {
            Ok(spec) => spec,
            Err(error) => return PhaseResult::Error(error),
        };
        let skip = if !cx.infra_enabled {
            Some(profile_disables_infra(cx))
        } else if !cx.whole_lane && !cx.setup_only {
            Some(reason("lane_running", json!({})))
        } else if cx.infra_templated && !engine.profiles_enable_infra(cx, &spec).await {
            cx.infra_enabled = false;
            Some(profile_disables_infra(cx))
        } else {
            None
        };
        // The containers' state last reported on `progress` events.
        let mut shown = Vec::new();
        let result = match &skip {
            Some(_) => engine.integ.infra.namespace(&spec).await,
            // A hung `up` must not keep a stop waiting for the lane.
            None => tokio::select! {
                up = engine.with_container_progress(cx, &spec, &mut shown, engine.integ.infra.up(&spec)) => up,
                () = cx.cancel.cancelled() => return PhaseResult::Error(start_cancelled(cx)),
            },
        };
        match result {
            Ok(up) => {
                let mut st = cx.lane.st();
                st.infra_ns = Some(up.namespace);
                st.infra_containers = up.containers;
                st.setup_env = up.setup_env;
            }
            Err(error) if skip.is_some() => {
                tracing::debug!(code = %error.code, "infra namespace unavailable");
            }
            Err(error) => return PhaseResult::Error(infra_error(error)),
        }
        if skip.is_none() {
            // Setup and services rely on the containers, so their `ready` checks
            // finish the phase; a stop still interrupts the wait.
            tokio::select! {
                result = engine.with_container_progress(cx, &spec, &mut shown, engine.containers_ready(&cx.lane, &spec)) => {
                    if let Err(error) = result {
                        return PhaseResult::Error(error);
                    }
                }
                () = cx.cancel.cancelled() => return PhaseResult::Error(start_cancelled(cx)),
            }
        }
        match skip {
            Some(reason) => PhaseResult::Skipped(reason),
            None => PhaseResult::Done,
        }
    }
}

#[async_trait]
impl LanePhase for ResolvePhase {
    fn name(&self) -> &'static str {
        "resolve"
    }

    fn visible(&self) -> bool {
        false
    }

    async fn run(&self, engine: &Arc<Inner>, cx: &mut StartCx) -> PhaseResult {
        engine.refresh_views_held(&cx.project, &cx.lane).await;
        match engine
            .resolve_config(&cx.project, &cx.lane, cx.config.clone())
            .await
        {
            Ok(resolved) => {
                // Infra came up already when it was on; this governs `infra.setup`.
                cx.infra_enabled &= resolved.infra_enabled;
                cx.resolved = Some(resolved);
                PhaseResult::Done
            }
            // The edited config needs ports the running lane's block cannot hold:
            // say so, rather than naming the missing offset.
            Err(error) if error.code == "port_error" && cx.lane.st().ports_pending => {
                PhaseResult::Error(RpcError::new(
                    "ports_pending",
                    json!({
                        "lane": cx.reference,
                        "unit": error.data.get("unit"),
                        "port": error.data.get("port"),
                        "message": format!(
                            "the edited config needs ports the running lane's block cannot hold; stop the lane, then start it again (devlanes stop --lane {})",
                            cx.reference
                        ),
                    }),
                    1,
                ))
            }
            Err(error) => PhaseResult::Error(error),
        }
    }
}

#[async_trait]
impl LanePhase for SetupPhase {
    fn name(&self) -> &'static str {
        "setup"
    }

    async fn run(&self, engine: &Arc<Inner>, cx: &mut StartCx) -> PhaseResult {
        let Some(resolved) = cx.resolved.clone() else {
            return PhaseResult::Skipped(reason("not_resolved", json!({})));
        };
        if resolved.setup.is_empty() {
            return PhaseResult::Skipped(reason("no_setup", json!({})));
        }
        if !cx.infra_enabled {
            return PhaseResult::Skipped(profile_disables_infra(cx));
        }
        let Some(lane_id) = cx.lane.st().record.as_ref().map(|r| r.id) else {
            return PhaseResult::Error(RpcError::new(
                "unassigned",
                json!({"lane": cx.reference}),
                1,
            ));
        };
        let records = match engine.store.setup_records(lane_id).await {
            Ok(records) => records,
            Err(error) => return PhaseResult::Error(store_error(error)),
        };
        let spec = engine
            .infra_spec(&cx.project, &cx.lane, &cx.infra_needs)
            .await
            .ok();
        let mut pending: Vec<(String, Option<String>)> = Vec::new();
        for (name, command) in &resolved.setup {
            let started_at = match (&command.container, &spec) {
                (Some(container), Some(spec)) => engine
                    .integ
                    .infra
                    .container_started_at(spec, container)
                    .await
                    .ok()
                    .flatten(),
                _ => None,
            };
            let done = records.iter().find(|r| &r.entry == name);
            let current = match (done, &command.container) {
                (None, _) => false,
                (Some(_), None) => true,
                // A restarted backing container lost the entry's effect (tmpfs data).
                (Some(record), Some(_)) => record.container_started_at == started_at,
            };
            if !current {
                pending.push((name.clone(), started_at));
            }
        }
        if pending.is_empty() {
            return PhaseResult::Skipped(reason("already_done", json!({})));
        }
        // A stop naming one of these entries interrupts the start.
        let _running = cx.ctl.running_tasks(
            pending
                .iter()
                .map(|(name, _)| format!("setup.{name}"))
                .collect(),
        );
        let runs = pending.iter().map(|(name, _)| {
            let engine = engine.clone();
            let project = cx.project.clone();
            let lane = cx.lane.clone();
            let actor = cx.actor.clone();
            let resolved = resolved.clone();
            let name = name.clone();
            let cancel = cx.cancel.clone();
            async move {
                let Some(command) = resolved.setup.get(&name) else {
                    return (name, Err(RpcError::new("not_found", json!({}), 3)));
                };
                let result = engine
                    .run_command(&project, &lane, "setup", command, &actor, &cancel)
                    .await;
                (name, result)
            }
        });
        // Entries run together; progress names one still running and counts the finished.
        let mut runs: FuturesUnordered<_> = runs.collect();
        let count = pending.len();
        let mut running: Vec<&String> = pending.iter().map(|(name, _)| name).collect();
        if let Some(first) = running.first() {
            engine.phase_step(cx, "setup", first, 1, count);
        }
        let mut results = Vec::with_capacity(count);
        while let Some((name, result)) = runs.next().await {
            running.retain(|running| **running != name);
            results.push((name, result));
            if let Some(next) = running.first() {
                engine.phase_step(cx, "setup", next, results.len() + 1, count);
            }
        }
        drop(runs);
        // Back in config order, as the failures are reported.
        results.sort_by_key(|(name, _)| pending.iter().position(|(entry, _)| entry == name));
        let mut failed = Vec::new();
        for (name, result) in results {
            if let Ok((_, host)) = &result {
                cx.leftovers
                    .extend(ended_leftovers(&cx.lane, &format!("setup.{name}"), host));
            }
            // An entry a stop interrupted did not run, whatever `ignore_errors` says.
            let ignore = resolved.setup.get(&name).is_some_and(|c| c.ignore_errors)
                && !cx.cancel.is_cancelled();
            let ok = matches!(&result, Ok((outcome, _)) if outcome.code == Some(0));
            if ok || ignore {
                let started_at = pending
                    .iter()
                    .find(|(n, _)| *n == name)
                    .and_then(|(_, at)| at.clone());
                if let Err(error) = engine
                    .store
                    .record_setup(lane_id, name.clone(), started_at)
                    .await
                {
                    return PhaseResult::Error(store_error(error));
                }
                continue;
            }
            let unit = format!("setup.{name}");
            match result {
                Ok((_, host)) => cx
                    .failures
                    .push(failure_of(&cx.reference, &unit, &host).await),
                Err(error) => cx.failures.push(UnitFailureData {
                    lane: cx.reference.clone(),
                    unit,
                    reason: Some(error),
                    ..UnitFailureData::default()
                }),
            }
            failed.push(name);
        }
        if failed.is_empty() {
            PhaseResult::Done
        } else {
            PhaseResult::Halt(reason("setup_failed", json!({"entries": failed})))
        }
    }
}

#[async_trait]
impl LanePhase for TasksPhase {
    fn name(&self) -> &'static str {
        if self.after {
            "after-services"
        } else {
            "tasks"
        }
    }

    async fn run(&self, engine: &Arc<Inner>, cx: &mut StartCx) -> PhaseResult {
        let Some(resolved) = cx.resolved.clone() else {
            return PhaseResult::Skipped(reason("not_resolved", json!({})));
        };
        if !cx.whole_lane && !cx.setup_only {
            return PhaseResult::Skipped(reason("lane_running", json!({})));
        }
        if self.after && cx.setup_only {
            return PhaseResult::Skipped(reason("setup_only", json!({})));
        }
        let Some(lane_id) = cx.lane.st().record.as_ref().map(|r| r.id) else {
            return PhaseResult::Error(RpcError::new(
                "unassigned",
                json!({"lane": cx.reference}),
                1,
            ));
        };
        let mut chosen = Vec::new();
        for (name, task) in &resolved.tasks {
            let hook = match task.when {
                Some(TaskWhen::Create) => "lane.create",
                Some(TaskWhen::Start) if !cx.setup_only => "lane.start",
                _ => continue,
            };
            if task.after_services != self.after || !task.active {
                continue;
            }
            if hook == "lane.create" {
                match engine.store.task_succeeded(lane_id, name.clone()).await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => return PhaseResult::Error(store_error(error)),
                }
            }
            chosen.push((name.clone(), hook));
        }
        if chosen.is_empty() {
            return PhaseResult::Skipped(reason("no_tasks", json!({})));
        }
        if self.after {
            let hosts: Vec<UnitHost> = {
                let st = cx.lane.st();
                cx.started
                    .iter()
                    .chain(&cx.already_running)
                    .filter_map(|unit| st.units.get(unit).map(|rec| rec.host.clone()))
                    .collect()
            };
            let settled = tokio::select! {
                settled = wait_settled(&hosts, cx.deadline) => settled,
                () = cx.cancel.cancelled() => return PhaseResult::Error(start_cancelled(cx)),
            };
            if !settled {
                cx.timed_out = true;
                for (name, _) in &chosen {
                    cx.skipped_tasks.push(name.clone());
                    engine.task_event(
                        &cx.reference,
                        name,
                        "skipped",
                        None,
                        Some(json!({"code": "timed_out"})),
                    );
                }
                return PhaseResult::Skipped(reason("timed_out", json!({})));
            }
            let failed: Vec<String> = {
                let st = cx.lane.st();
                cx.started
                    .iter()
                    .chain(&cx.already_running)
                    .filter(|unit| {
                        st.units
                            .get(*unit)
                            .is_some_and(|rec| matches!(rec.host.state(), UnitState::Failed(_)))
                    })
                    .cloned()
                    .collect()
            };
            if !failed.is_empty() || !cx.failures.is_empty() {
                let mut units = failed;
                units.extend(cx.failures.iter().map(|f| f.unit.clone()));
                for (name, _) in &chosen {
                    cx.skipped_tasks.push(name.clone());
                    engine.task_event(
                        &cx.reference,
                        name,
                        "skipped",
                        None,
                        Some(json!({"code": "service_failed", "units": units})),
                    );
                }
                return PhaseResult::Skipped(reason("service_failed", json!({"units": units})));
            }
        }
        let count = chosen.len();
        for (index, (name, hook)) in chosen.into_iter().enumerate() {
            engine.phase_step(cx, self.name(), &name, index + 1, count);
            // A stop naming the task or one of its steps interrupts the start.
            let _running = cx.ctl.running_tasks(task_units(&resolved, &name));
            let outcome = engine
                .run_task(
                    &cx.project,
                    &cx.lane,
                    &resolved,
                    &name,
                    hook,
                    &cx.actor,
                    cx.session.as_ref(),
                    &cx.cancel,
                    0,
                )
                .await;
            if cx.cancel.is_cancelled() {
                return PhaseResult::Error(start_cancelled(cx));
            }
            if let Ok(outcome) = &outcome {
                cx.leftovers.extend(outcome.leftovers.iter().cloned());
            }
            match outcome {
                Ok(TaskOutcome { code: Some(0), .. }) => {}
                Ok(TaskOutcome { code, host, .. }) => {
                    let mut failure = match &host {
                        Some(host) => failure_of(&cx.reference, &name, host).await,
                        None => UnitFailureData {
                            lane: cx.reference.clone(),
                            unit: name.clone(),
                            ..UnitFailureData::default()
                        },
                    };
                    failure.exit_code = code;
                    failure.reason.get_or_insert_with(|| {
                        RpcError::new("task_failed", json!({"task": name, "exit_code": code}), 1)
                    });
                    cx.failures.push(failure);
                    return PhaseResult::Halt(reason("task_failed", json!({"task": name})));
                }
                Err(error) => {
                    cx.failures.push(UnitFailureData {
                        lane: cx.reference.clone(),
                        unit: name.clone(),
                        reason: Some(error),
                        ..UnitFailureData::default()
                    });
                    return PhaseResult::Halt(reason("task_failed", json!({"task": name})));
                }
            }
        }
        PhaseResult::Done
    }
}

#[async_trait]
impl LanePhase for ServicesPhase {
    fn name(&self) -> &'static str {
        "services"
    }

    async fn run(&self, engine: &Arc<Inner>, cx: &mut StartCx) -> PhaseResult {
        if cx.setup_only {
            return PhaseResult::Skipped(reason("setup_only", json!({})));
        }
        let Some(resolved) = cx.resolved.clone() else {
            return PhaseResult::Skipped(reason("not_resolved", json!({})));
        };
        if cx.services.is_empty() {
            return PhaseResult::Skipped(reason("no_services", json!({})));
        }
        // A locked vault fails the start as a whole: `devlanes secrets unlock` fixes it.
        for name in &cx.services {
            if let Some(error) = resolved.errors.get(&format!("services.{name}")) {
                let error = crate::resolve::resolve_error(error);
                if error.code == "secrets_locked" {
                    return PhaseResult::Error(error);
                }
            }
        }
        let mut to_spawn: Vec<&ResolvedService> = Vec::new();
        // A copy, because the paced spawn below needs `cx` mutably.
        let selected = cx.services.clone();
        for name in &selected {
            if let Some(error) = resolved.errors.get(&format!("services.{name}")) {
                cx.failures.push(UnitFailureData {
                    lane: cx.reference.clone(),
                    unit: name.clone(),
                    reason: Some(crate::resolve::resolve_error(error)),
                    ..UnitFailureData::default()
                });
                continue;
            }
            for service in resolved.units_of(name) {
                // A stop or restart took it out of this start; the result says so.
                if cx.ctl.withdrawn_by(&service.unit).is_some() {
                    continue;
                }
                // Never adopted: the user stops it explicitly; other units go on.
                let orphaned = cx.lane.st().orphan_error(&cx.project.name(), &service.unit);
                if let Some(error) = orphaned {
                    cx.failures.push(UnitFailureData {
                        lane: cx.reference.clone(),
                        unit: service.unit.clone(),
                        reason: Some(error),
                        ..UnitFailureData::default()
                    });
                    continue;
                }
                let alive = cx
                    .lane
                    .st()
                    .units
                    .get(&service.unit)
                    .is_some_and(|rec| rec.host.is_alive());
                if alive {
                    cx.already_running.push(service.unit.clone());
                    continue;
                }
                if service.literal_port
                    && let Some((lane, unit)) = engine.literal_port_holder(service.port, &cx.lane)
                {
                    cx.failures.push(UnitFailureData {
                        lane: cx.reference.clone(),
                        unit: service.unit.clone(),
                        reason: Some(RpcError::new(
                            "port_in_use_by_lane",
                            json!({"port": service.port, "lane": lane, "unit": unit}),
                            1,
                        )),
                        ..UnitFailureData::default()
                    });
                    continue;
                }
                // A connected box's mirror may hold one of these ports on loopback,
                // where a ready check would reach the box instead of this service.
                let ports: BTreeSet<u16> = service
                    .ports
                    .values()
                    .copied()
                    .chain([service.port])
                    .filter(|port| *port != 0)
                    .collect();
                if let Err(error) = engine.release_for_local(&ports).await {
                    cx.failures.push(UnitFailureData {
                        lane: cx.reference.clone(),
                        unit: service.unit.clone(),
                        reason: Some(error),
                        ..UnitFailureData::default()
                    });
                    continue;
                }
                to_spawn.push(service);
            }
        }
        if !engine.integ.launch_checks.is_empty() {
            let starting: Vec<PlannedUnit> = to_spawn
                .iter()
                .map(|service| PlannedUnit {
                    label: format!("{}:{}", cx.reference, service.unit),
                    preset: service.preset.clone(),
                    files: service.launch.files.clone(),
                })
                .collect();
            let running = engine.running_planned(&cx.project);
            for check in &engine.integ.launch_checks {
                if let Err(message) = check.check(&starting, &running) {
                    return PhaseResult::Error(RpcError::new(
                        "launch_check_failed",
                        json!({"message": message}),
                        1,
                    ));
                }
            }
        }
        // Each spawn waits for a start slot; see `pacing`.
        engine.spawn_services(cx, &to_spawn, &resolved).await;
        PhaseResult::Done
    }
}

/// Exit of a task: its code (`None` when killed or timed out) and its last unit.
pub(crate) struct TaskOutcome {
    pub code: Option<i32>,
    pub host: Option<UnitHost>,
    /// Every unit of the task (steps included) that left processes behind.
    pub leftovers: Vec<Leftovers>,
}

/// Options of one start request.
pub(crate) struct StartOptions {
    pub selectors: Vec<String>,
    /// Requested profiles; empty keeps the lane's current ones.
    pub profiles: Vec<String>,
    pub env: Option<Arc<EnvMap>>,
    pub overrides: EnvMap,
    pub actor: Actor,
    pub timeout: Duration,
    pub session: Option<Session>,
    pub setup_only: bool,
    /// Bypass the start gate for this request.
    pub all_at_once: bool,
    /// The `start_id` to use, chosen by a caller that streams this start's
    /// events; `None` takes the next one.
    pub start_id: Option<u64>,
    /// Place in the start gate's line, taken when the request arrived; `None`
    /// takes one when the services phase begins.
    pub arrival: Option<devlanes_core::gate::Arrival>,
}

type Spawned = (UnitHost, Arc<dyn ManagedUnit>);

impl Inner {
    /// Spawns a unit, records its run and starts its state watcher.
    pub(crate) async fn spawn_unit(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        request: SpawnRequest,
    ) -> Result<Spawned, RpcError> {
        // Counted before the shutdown check: once shutdown saw no spawn in flight,
        // every later one sees the cancelled token and refuses. The lane's own
        // `lane.stop` tasks of an explicit shutdown are the exception.
        let _spawning = lane.spawn_guard();
        if self.shutdown.is_cancelled() && !lane.st().shutdown_hooks {
            return Err(crate::engine::daemon_stopping());
        }
        let (lane_path, record, reference, previous, color) = {
            let st = lane.st();
            let color = request
                .color
                .clone()
                .unwrap_or_else(|| crate::model::derived_color(&st, &request.name));
            let reference = st.reference(&project.name());
            if let Some(error) = st.orphan_error(&project.name(), &request.name) {
                return Err(error);
            }
            let record = st
                .record
                .clone()
                .ok_or_else(|| RpcError::new("unassigned", json!({"lane": reference}), 1))?;
            let previous = st
                .units
                .get(&request.name)
                .map(|rec| (rec.run_id, rec.host.clone()));
            (st.path.clone(), record, reference, previous, color)
        };
        if let Some((run_id, host)) = previous {
            if host.is_alive() {
                return Err(RpcError::new(
                    "unit_running",
                    json!({"lane": reference, "unit": request.name}),
                    1,
                ));
            }
            // The watcher may not have recorded the exit yet; finishing is idempotent.
            // While the old row stays unfinished, a new run could not be recorded.
            if let Some(id) = run_id {
                let outcome = host.exit_outcome().unwrap_or(ExitOutcome {
                    code: None,
                    signal: None,
                });
                if !self.finish_run(id, outcome).await {
                    return Err(RpcError::new(
                        "state_error",
                        json!({"lane": reference, "unit": request.name, "message": "the previous run could not be recorded as finished in state.db"}),
                        1,
                    ));
                }
            }
        }
        write_files(lane_path, request.files.clone()).await?;
        // The process exists before its run row does. A daemon crash between the
        // spawn and `record_unit_run` below leaves a process no later daemon knows
        // about: reconcile cannot list it as an orphan and nothing stops it. The
        // row needs the pid and start time, so it cannot be written first.
        let runner = &self.integ.runner;
        let attempt = crate::spawn_error::Attempt {
            unit: request.name.clone(),
            program: request.spec.program.clone(),
            cwd: request.spec.cwd.clone(),
            // Windows spells it `Path`; the lookup ignores case there.
            path: devlanes_core::env::get(&request.spec.env, "PATH").cloned(),
        };
        // Keys only: values can hold secrets.
        tracing::debug!(
            unit = %request.name,
            program = %attempt.program,
            args = request.spec.args.len(),
            cwd = %attempt.cwd.display(),
            pty = request.spec.pty,
            env_keys = %request.spec.env.keys().map(String::as_str).collect::<Vec<_>>().join(","),
            "spawning"
        );
        let spawned = if request.kind == Kind::Task {
            runner.spawn_task(request.spec).await
        } else {
            runner.spawn(request.spec).await
        };
        let log_dir = self
            .opts
            .logs
            .join(devlanes_core::paths::disk_name(&project.name()))
            .join(record.index.to_string())
            .join(devlanes_core::paths::disk_name(&request.name));
        let managed = match spawned {
            Ok(managed) => managed,
            Err(error) => {
                return Err(self.spawn_failed(attempt, error, log_dir, &reference).await);
            }
        };
        let identity = managed.identity();
        tracing::debug!(unit = %request.name, pid = identity.map(|i| i.pid), "spawned");
        let info = UnitInfo {
            name: request.name.clone(),
            lane: reference.clone(),
            kind: request.kind.word(),
            actor: request.actor.clone(),
            started_at_ms: now_ms(),
            log_dir: log_dir.clone(),
            retention: crate::units::LogRetention::from_config(&self.opts.user.logs.retention),
        };
        let unit: Arc<dyn Unit> = managed.clone();
        let host = match UnitHost::start(info, unit, identity).await {
            Ok(host) => host,
            Err(error) => {
                // Without a host nobody could see or stop it; this spawn is ours to undo.
                let _ = managed.stop(request.stop).await;
                return Err(error);
            }
        };
        let run_id = match identity {
            Some(process) if process.pid > 0 && process.sid > 0 => {
                let boot_id = tokio::task::spawn_blocking(devlanes_sys::boot_id)
                    .await
                    .ok()
                    .and_then(Result::ok);
                let recorded = self
                    .store
                    .record_unit_run(devlanes_state::UnitRunInput {
                        lane_id: record.id,
                        unit: request.name.clone(),
                        process,
                        actor: core_actor(&request.actor),
                        log_path: Some(log_dir),
                        boot_id,
                    })
                    .await;
                match recorded {
                    Ok(run) => Some(run.id),
                    Err(error) => {
                        // An unrecorded process would survive a daemon crash with no
                        // way to find or stop it, so it does not run at all.
                        tracing::warn!(%error, unit = %request.name, "unit run not recorded; stopping it");
                        let _ = host.stop(request.stop).await;
                        return Err(store_error(error));
                    }
                }
            }
            _ => None,
        };
        lane.st().units.insert(
            request.name.clone(),
            UnitRec {
                host: host.clone(),
                kind: request.kind,
                run_id,
                ports: request.ports,
                urls: request.urls,
                tags: request.tags,
                hidden: request.hidden,
                autostart: request.autostart,
                stop: request.stop,
                literal_port: request.literal_port,
                env: request.env,
                failure_tail: Vec::new(),
                preset: request.preset,
                files: request.files,
                color: Some(color),
                managed: managed.clone(),
                syncs: Vec::new(),
            },
        );
        if request.kind == Kind::Service {
            crate::local_sync::start(
                lane.clone(),
                request.name.clone(),
                host.clone(),
                request.sync,
            );
        }
        self.watch_unit(
            project.clone(),
            lane.clone(),
            request.name,
            host.clone(),
            run_id,
        );
        if let Some(run) = run_id {
            self.track_escaped(host.clone(), run);
        }
        Ok((host, managed))
    }

    /// Publishes every state transition and records the exit.
    fn watch_unit(
        self: &Arc<Self>,
        project: Arc<ProjectRt>,
        lane: Arc<LaneRt>,
        name: String,
        host: UnitHost,
        run_id: Option<devlanes_core::UnitRunId>,
    ) {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut watch = host.watch_state();
            loop {
                let state = watch.borrow_and_update().clone();
                if let UnitState::Failed(_) = state {
                    let tail = host.tail(TAIL).await;
                    if let Some(rec) = lane
                        .st()
                        .units
                        .get_mut(&name)
                        .filter(|r| r.host.same_run(&host))
                    {
                        rec.failure_tail = tail;
                    }
                }
                engine.emit_unit(&project, &lane, &name, &host);
                if matches!(state, UnitState::Exited(_)) || !host.is_alive() {
                    break;
                }
                if watch.changed().await.is_err() {
                    break;
                }
            }
            let outcome = host.wait_exit().await;
            // A sync mirrors a running service's output; it ends with the run.
            crate::local_sync::stop(&lane, &name, &host).await;
            // On failure the lane's record keeps `run_id`; the next start of this
            // unit finishes the row again before recording a new run.
            if let Some(id) = run_id {
                engine.finish_run(id, outcome).await;
            }
            engine.emit_unit(&project, &lane, &name, &host);
            if let Some(id) = run_id {
                // The log closes right after the exit; its segments are final then.
                for segment in host.run_segments(Duration::from_secs(5)).await {
                    let _ = engine
                        .store
                        .record_log_segment(devlanes_state::LogSegmentInput {
                            unit_run_id: id,
                            first_seq: segment.first_seq,
                            last_seq: segment.last_seq,
                            min_ts_ms: segment.min_timestamp_ms,
                            max_ts_ms: segment.max_timestamp_ms,
                            path: segment.path,
                            compressed: segment.compressed,
                            bytes: segment.bytes,
                        })
                        .await;
                }
            }
        });
    }

    /// Records why the selected units that did not start stayed idle (`phase_failed`)
    /// and emits `unit.changed` for each, so clients see it on the unit itself.
    fn mark_unstarted(
        &self,
        project: &ProjectRt,
        lane: &LaneRt,
        cx: &StartCx,
        phase: &str,
        error: &RpcError,
    ) {
        let reason = RpcError::new(
            "phase_failed",
            json!({
                "lane": cx.reference,
                "phase": phase,
                "error": {"code": error.code, "data": error.data},
            }),
            1,
        );
        let summaries = {
            let mut st = lane.st();
            let units: Vec<String> = st
                .view
                .as_ref()
                .map(|view| {
                    view.services
                        .iter()
                        .filter(|service| cx.services.contains(&service.name))
                        .map(|service| service.unit.clone())
                        .filter(|unit| {
                            !cx.started.contains(unit)
                                && !cx.already_running.contains(unit)
                                && cx.ctl.withdrawn_by(unit).is_none()
                        })
                        .collect()
                })
                .unwrap_or_default();
            for unit in &units {
                st.phase_reasons.insert(unit.clone(), reason.clone());
            }
            let summary = crate::model::lane_summary(&project.name(), &st);
            summary
                .units
                .into_iter()
                .filter(|unit| units.contains(&unit.name))
                .collect::<Vec<_>>()
        };
        for summary in summaries {
            self.bus
                .emit(events::UNIT_CHANGED, json!({"unit": summary}));
        }
    }

    /// Emits `unit.changed` and `lane.changed` for one unit.
    pub(crate) fn emit_unit(
        &self,
        project: &ProjectRt,
        lane: &LaneRt,
        name: &str,
        host: &UnitHost,
    ) {
        let summary = {
            let st = lane.st();
            st.units
                .get(name)
                .filter(|rec| rec.host.same_run(host))
                .map(|rec| {
                    let mut summary = unit_summary(&st.reference(&project.name()), name, rec);
                    crate::model::apply_phase_reason(&st, &mut summary);
                    summary
                })
        };
        if let Some(summary) = summary {
            self.bus
                .emit(events::UNIT_CHANGED, json!({"unit": summary}));
        }
        self.emit_lane(project, lane);
    }

    /// Emits a `task` event.
    pub(crate) fn task_event(
        &self,
        lane: &str,
        task: &str,
        state: &str,
        exit_code: Option<i32>,
        reason: Option<Value>,
    ) {
        let mut data = json!({"lane": lane, "task": task, "state": state});
        if let Some(code) = exit_code {
            data["exit_code"] = json!(code);
        }
        if let Some(reason) = reason {
            data["reason"] = reason;
        }
        self.bus.emit(events::TASK, data);
    }

    /// Emits a `lane.phase` event of the start `cx`; `units` are the units the
    /// services phase is about to spawn.
    fn phase_event(
        &self,
        cx: &StartCx,
        phase: &str,
        state: &str,
        reason: Option<&RpcError>,
        units: Option<Vec<String>>,
    ) {
        let mut data = json!({
            "lane": cx.reference,
            "phase": phase,
            "state": state,
            "start_id": cx.start_id,
        });
        if let Some(reason) = reason {
            data["reason"] = json!({"code": reason.code, "data": reason.data});
        }
        if let Some(units) = units {
            data["units"] = json!(units);
        }
        self.bus.emit(events::PHASE, data);
    }

    /// A fresh `start_id`.
    pub(crate) fn new_start_id(&self) -> u64 {
        self.next_start
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The units the services phase of `cx` will spawn: every replica of the
    /// selected services that neither runs already nor is a previous daemon's
    /// orphan. Units already up are not part of this start's progress.
    fn planned_units(cx: &StartCx) -> Vec<String> {
        let Some(resolved) = &cx.resolved else {
            return Vec::new();
        };
        let st = cx.lane.st();
        cx.services
            .iter()
            .flat_map(|name| resolved.units_of(name))
            .map(|service| service.unit.clone())
            .filter(|unit| {
                !st.orphans.contains_key(unit)
                    && !st.units.get(unit).is_some_and(|rec| rec.host.is_alive())
                    && cx.ctl.withdrawn_by(unit).is_none()
            })
            .collect()
    }

    /// The other lane and unit currently running a literal `port`.
    fn literal_port_holder(&self, port: u16, except: &Arc<LaneRt>) -> Option<(String, String)> {
        for (project, lane) in self.all_lanes() {
            if Arc::ptr_eq(&lane, except) {
                continue;
            }
            let st = lane.st();
            for (name, rec) in &st.units {
                if rec.literal_port == Some(port) && rec.host.is_alive() {
                    return Some((st.reference(&project.name()), name.clone()));
                }
            }
        }
        None
    }

    /// Running units of a project as launch-check input.
    fn running_planned(&self, project: &ProjectRt) -> Vec<PlannedUnit> {
        let name = project.name();
        project
            .lanes()
            .iter()
            .flat_map(|lane| {
                let st = lane.st();
                let reference = st.reference(&name);
                st.units
                    .iter()
                    .filter(|(_, rec)| rec.host.is_alive())
                    .map(|(unit, rec)| PlannedUnit {
                        label: format!("{reference}:{unit}"),
                        preset: rec.preset.clone(),
                        files: rec.files.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Runs one `infra.setup`/`infra.teardown` entry to completion.
    pub(crate) async fn run_command(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        phase: &str,
        command: &ResolvedCommand,
        actor: &Actor,
        cancel: &CancellationToken,
    ) -> Result<(ExitOutcome, UnitHost), RpcError> {
        let extra = lane.st().setup_env.clone();
        let name = format!("{phase}.{}", command.name);
        let request =
            SpawnRequest::oneshot(&name, Kind::Task, &command.launch, false, &extra, actor);
        let (host, _) = self.spawn_unit(project, lane, request).await?;
        let outcome = finish(&host, command.timeout, cancel).await;
        Ok((outcome, host))
    }

    /// Runs a task (or its steps) to completion, recording `task_runs`. When
    /// `cancel` fires, the running unit is stopped and no further step starts;
    /// the outcome then has no exit code. The row is finished and the closing
    /// `task` event emitted on every path, a dropped future included.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_task<'a>(
        self: &'a Arc<Self>,
        project: &'a Arc<ProjectRt>,
        lane: &'a Arc<LaneRt>,
        resolved: &'a ResolvedLane,
        name: &'a str,
        trigger: &'a str,
        actor: &'a Actor,
        session: Option<&'a Session>,
        cancel: &'a CancellationToken,
        depth: usize,
    ) -> BoxFuture<'a, Result<TaskOutcome, RpcError>> {
        Box::pin(async move {
            let reference = lane.st().reference(&project.name());
            if depth > MAX_STEP_DEPTH {
                return Err(RpcError::new(
                    "config_error",
                    json!({"path": format!("tasks.{name}.steps"), "message": "steps nest too deep"}),
                    1,
                ));
            }
            if let Some(error) = resolved.errors.get(&format!("tasks.{name}")) {
                return Err(crate::resolve::resolve_error(error));
            }
            let task = resolved.tasks.get(name).ok_or_else(|| {
                RpcError::new(
                    "not_found",
                    json!({"reference": name, "candidates": resolved.tasks.keys().collect::<Vec<_>>()}),
                    3,
                )
            })?;
            let lane_id = lane
                .st()
                .record
                .as_ref()
                .map(|r| r.id)
                .ok_or_else(|| RpcError::new("unassigned", json!({"lane": reference}), 1))?;
            self.task_event(&reference, name, "started", None, None);
            let run = self
                .store
                .start_task_run(TaskRunInput {
                    lane_id,
                    task: name.to_owned(),
                    trigger: trigger.to_owned(),
                    actor: core_actor(actor),
                })
                .await
                .map_err(store_error)?;
            // Every exit path finishes the row: an unfinished task run would block
            // `lane clean` until the next daemon start abandons it. The guard covers
            // a future dropped before the end (a client's limit, a cancelled request).
            let mut guard = TaskRunGuard {
                engine: self.clone(),
                run: Some(run.id),
                reference: reference.clone(),
                task: name.to_owned(),
            };
            let result: Result<TaskOutcome, RpcError> = async {
                let mut outcome = TaskOutcome {
                    code: Some(0),
                    host: None,
                    leftovers: Vec::new(),
                };
                let mut leftovers = Vec::new();
                match &task.launch {
                    None => {
                        for step in &task.steps {
                            if cancel.is_cancelled() {
                                outcome.code = None;
                                break;
                            }
                            let step_outcome =
                                if let Some(entry) = step.strip_prefix("infra.setup.") {
                                    self.step_command(
                                        project, lane, resolved, "setup", entry, actor, cancel,
                                    )
                                    .await?
                                } else if let Some(entry) = step.strip_prefix("infra.teardown.") {
                                    self.step_command(
                                        project, lane, resolved, "teardown", entry, actor, cancel,
                                    )
                                    .await?
                                } else {
                                    self.run_task(
                                        project,
                                        lane,
                                        resolved,
                                        step,
                                        "step",
                                        actor,
                                        session,
                                        cancel,
                                        depth + 1,
                                    )
                                    .await?
                                };
                            leftovers.extend(step_outcome.leftovers.iter().cloned());
                            if step_outcome.code != Some(0) {
                                outcome = step_outcome;
                                break;
                            }
                        }
                    }
                    Some(launch) => {
                        let extra = lane.st().setup_env.clone();
                        let mut request = SpawnRequest::oneshot(
                            name,
                            Kind::Task,
                            launch,
                            task.pty,
                            &extra,
                            actor,
                        );
                        request.color = Some(task.color.clone());
                        let (host, _) = self.spawn_unit(project, lane, request).await?;
                        let exit = match session {
                            Some(session) => {
                                let follower = stream_logs(host.clone(), session.clone());
                                let (exit, ()) =
                                    tokio::join!(finish(&host, task.timeout, cancel), follower);
                                exit
                            }
                            None => finish(&host, task.timeout, cancel).await,
                        };
                        leftovers.extend(ended_leftovers(lane, name, &host));
                        outcome = TaskOutcome {
                            code: exit.code,
                            host: Some(host),
                            leftovers: Vec::new(),
                        };
                    }
                }
                outcome.leftovers = leftovers;
                Ok(outcome)
            }
            .await;
            let code = result.as_ref().ok().and_then(|outcome| outcome.code);
            let (state, reason) = match &result {
                Ok(outcome) if outcome.code == Some(0) => ("done", None),
                Ok(_) if cancel.is_cancelled() => ("failed", Some(json!({"code": "cancelled"}))),
                Ok(_) => ("failed", None),
                Err(error) => (
                    "failed",
                    Some(json!({"code": error.code, "data": error.data})),
                ),
            };
            self.close_task_run(run.id, &reference, name, code, state, reason)
                .await;
            guard.run = None;
            result
        })
    }

    /// Finishes a task run row and emits its closing `task` event.
    async fn close_task_run(
        &self,
        id: devlanes_state::TaskRunId,
        reference: &str,
        task: &str,
        code: Option<i32>,
        state: &str,
        reason: Option<Value>,
    ) {
        if let Err(error) = self.store.finish_task_run(id, code).await {
            tracing::warn!(%error, task, "task run not recorded as finished");
        }
        self.task_event(reference, task, state, code, reason);
    }

    /// Runs a setup/teardown entry as a task step and updates its done-record.
    #[allow(clippy::too_many_arguments)]
    async fn step_command(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        resolved: &ResolvedLane,
        phase: &str,
        entry: &str,
        actor: &Actor,
        cancel: &CancellationToken,
    ) -> Result<TaskOutcome, RpcError> {
        let commands = if phase == "setup" {
            &resolved.setup
        } else {
            &resolved.teardown
        };
        let command = commands.get(entry).ok_or_else(|| {
            RpcError::new(
                "not_found",
                json!({"reference": format!("infra.{phase}.{entry}")}),
                3,
            )
        })?;
        let (outcome, host) = self
            .run_command(project, lane, phase, command, actor, cancel)
            .await?;
        // An interrupted entry did not do its work, whatever `ignore_errors` says.
        let ok = !cancel.is_cancelled() && (outcome.code == Some(0) || command.ignore_errors);
        let lane_id = lane.st().record.as_ref().map(|r| r.id);
        if let Some(lane_id) = lane_id {
            if phase == "setup" && ok {
                let _ = self
                    .store
                    .record_setup(lane_id, entry.to_owned(), None)
                    .await;
            } else if phase == "teardown" && ok {
                let _ = self
                    .store
                    .clear_setup(lane_id, Some(entry.to_owned()))
                    .await;
            }
        }
        let leftovers = ended_leftovers(lane, &format!("{phase}.{entry}"), &host)
            .into_iter()
            .collect();
        Ok(TaskOutcome {
            code: if ok { Some(0) } else { outcome.code },
            host: Some(host),
            leftovers,
        })
    }

    /// Brings a lane up (decision 5). Idempotent: running units stay untouched.
    pub(crate) async fn start_lane(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        options: StartOptions,
    ) -> Result<StartResult, RpcError> {
        let op = lane.op.lock().await;
        // Installed once `op` is ours, until the start answered: a whole-lane stop
        // fires its token and waits for `op` next; a single-unit stop withdraws
        // units through it without waiting. A later start installs its own.
        let slot = lane.begin_start(&self.shutdown);
        let cancel = slot.ctl.cancel.clone();
        let initial = lane.st().reference(&project.name());
        let fail = |error: RpcError| with_ownership(error, &initial, &[]);
        {
            let st = lane.st();
            if let Some(reason) = &st.unassigned {
                return Err(fail(reason.clone()));
            }
            if let crate::model::ConfigState::Invalid(error) = &st.config {
                return Err(fail(error.clone()));
            }
            if st.record.is_none() {
                return Err(fail(RpcError::new(
                    "unassigned",
                    json!({"lane": initial}),
                    1,
                )));
            }
            if !st.present {
                return Err(fail(RpcError::new(
                    "worktree_missing",
                    json!({"path": st.path}),
                    3,
                )));
            }
        }
        self.check_name_conflict(project).map_err(fail)?;
        {
            let mut st = lane.st();
            if let Some(env) = options.env.clone() {
                st.env = Some(env);
            }
            if st.env.is_none() {
                return Err(fail(RpcError::new(
                    "needs_env",
                    json!({"lane": initial}),
                    1,
                )));
            }
            st.overrides = options.overrides.clone();
        }
        if let Some(env) = options.env.clone() {
            project.st().last_env = Some(env);
        }
        self.apply_move(project, lane).await.map_err(fail)?;
        self.refresh_views_held(project, lane).await;
        let reference = lane.st().reference(&project.name());
        let fail = |error: RpcError| with_ownership(error, &reference, &[]);
        let config = lane
            .st()
            .config
            .loaded()
            .cloned()
            .ok_or_else(|| fail(RpcError::new("no_config", json!({"lane": reference}), 1)))?;
        let config_dir = lane.st().path.join(&project.config_dir);
        check_compose_files(&config, config_dir)
            .await
            .map_err(fail)?;
        let requested = if options.profiles.is_empty() {
            lane.st().profiles.clone()
        } else {
            options.profiles.clone()
        };
        let selection = devlanes_resolve::select_profiles(&config, &requested, &options.selectors)
            .map_err(|error| fail(crate::resolve::resolve_error(&error)))?;
        // Kept only once valid, so a typo does not stick to the lane.
        lane.st().profiles = selection.profiles.clone();
        let infra_enabled = !matches!(
            selection
                .profile_options
                .as_ref()
                .and_then(|o| o.infra.as_ref()),
            Some(Templated::Literal(false))
        );
        let infra_templated = selection.profiles.iter().any(|name| {
            matches!(
                config.profiles.get(name),
                Some(devlanes_config::ProfileConfig::Object(options))
                    if matches!(options.infra, Some(Templated::Expression(_)))
            )
        });
        // A previous daemon's orphan is part of the lane: with one running, this
        // start only adds units, like with a live unit.
        let whole_lane = !lane.st().any_running();
        {
            let mut st = lane.st();
            st.busy = Some("starting");
            st.last_error = None;
            if whole_lane {
                st.started_at_ms = Some(now_ms());
                st.peaks.clear();
                st.config_changed = false;
                st.skipped_tasks.clear();
            }
        }
        self.emit_lane(project, lane);
        let mut cx = StartCx {
            project: project.clone(),
            lane: lane.clone(),
            reference: reference.clone(),
            actor: options.actor.clone(),
            services: selection.services.clone(),
            profiles: selection.profiles.clone(),
            infra_enabled,
            infra_templated,
            whole_lane,
            setup_only: options.setup_only,
            session: options.session.clone(),
            resolved: None,
            deadline: Instant::now() + options.timeout,
            started: Vec::new(),
            already_running: Vec::new(),
            failures: Vec::new(),
            skipped_tasks: Vec::new(),
            leftovers: Vec::new(),
            infra_needs: Vec::new(),
            timed_out: false,
            all_at_once: options.all_at_once,
            start_id: options.start_id.unwrap_or_else(|| self.new_start_id()),
            cancel,
            ctl: slot.ctl.clone(),
            config: config.clone(),
            arrival: options.arrival,
        };
        tracing::debug!(
            lane = %reference,
            whole_lane,
            profiles = %cx.profiles.join(","),
            services = %cx.services.join(","),
            infra_enabled,
            timeout_ms = options.timeout.as_millis() as u64,
            start_id = cx.start_id,
            "start planned"
        );
        let mut hard_error = None;
        let mut failed_phase: Option<(&'static str, RpcError)> = None;
        for phase in start_pipeline() {
            if cx.cancel.is_cancelled() {
                let error = start_cancelled(&cx);
                failed_phase = Some((phase.name(), error.clone()));
                hard_error = Some(error);
                break;
            }
            if phase.visible() {
                // Resolution ran before services, so the plan is known by now.
                let units = (phase.name() == "services" && !cx.setup_only)
                    .then(|| Self::planned_units(&cx));
                self.phase_event(&cx, phase.name(), "started", None, units);
            }
            // Reaching services is what makes earlier phase failures stale.
            if phase.name() == "services" && !cx.setup_only {
                lane.st().phase_reasons.clear();
            }
            let span = tracing::debug_span!("phase", name = phase.name(), lane = %cx.reference);
            let mut result = tracing::Instrument::instrument(phase.run(self, &mut cx), span).await;
            // Whatever the phase made of it, an interrupted start ends here.
            if cx.cancel.is_cancelled() && !matches!(result, PhaseResult::Error(_)) {
                result = PhaseResult::Error(start_cancelled(&cx));
            }
            if phase.name() == "services" {
                // Nothing more spawns: a waiting restart of a withdrawn unit may go.
                cx.ctl.services_done.cancel();
            }
            let (state, why) = result.event_state();
            if phase.visible() {
                self.phase_event(&cx, phase.name(), state, why, None);
            }
            match result {
                PhaseResult::Done | PhaseResult::Skipped(_) => {}
                PhaseResult::Halt(why) => {
                    failed_phase = Some((phase.name(), why));
                    break;
                }
                PhaseResult::Error(error) => {
                    failed_phase = Some((phase.name(), error.clone()));
                    hard_error = Some(error);
                    break;
                }
            }
        }
        if let Some((phase, error)) = &failed_phase
            && !cx.setup_only
        {
            self.mark_unstarted(project, lane, &cx, phase, error);
        }
        let record = lane.st().record.clone();
        if let Some(record) = record {
            let desired = if options.setup_only {
                record.desired_state
            } else {
                DesiredState::Running
            };
            self.save_desired(lane, &record, desired, selection.profiles.clone())
                .await;
        }
        persist("project start time", || {
            self.store.mark_project_started(project.id)
        })
        .await;
        {
            let mut st = lane.st();
            st.busy = None;
            st.skipped_tasks = cx.skipped_tasks.clone();
        }
        drop(op);
        let started: Vec<String> = cx
            .started
            .iter()
            .map(|u| format!("{reference}:{u}"))
            .collect();
        if let Some(error) = hard_error {
            lane.st().last_error = Some(error.clone());
            self.emit_lane(project, lane);
            return Err(with_ownership(error, &reference, &started));
        }
        let result = self.finish_start(cx, whole_lane).await;
        self.emit_lane(project, lane);
        // Answered: stops no longer reach this start.
        drop(slot);
        Ok(result)
    }

    /// Waits for started units and assembles the [`StartResult`]. Units a stop or
    /// restart withdrew are neither waited for nor failures: they are listed as
    /// `stopped` with a `cancelled_by_stop` reason.
    async fn finish_start(self: &Arc<Self>, mut cx: StartCx, whole_lane: bool) -> StartResult {
        let kept = |unit: &String| cx.ctl.withdrawn_by(unit).is_none();
        let selected: Vec<String> = cx
            .started
            .iter()
            .chain(&cx.already_running)
            .filter(|unit| kept(unit))
            .cloned()
            .collect();
        let hosts: Vec<UnitHost> = {
            let st = cx.lane.st();
            selected
                .iter()
                .filter_map(|unit| st.units.get(unit).map(|rec| rec.host.clone()))
                .collect()
        };
        if !wait_settled(&hosts, cx.deadline).await {
            cx.timed_out = true;
        }
        // `ready: started` calls a unit ready at once; one that exits right after
        // its spawn (a bad flag, a missing file) failed to start all the same.
        let fresh: Vec<UnitHost> = {
            let st = cx.lane.st();
            cx.started
                .iter()
                .filter_map(|unit| st.units.get(unit).map(|rec| rec.host.clone()))
                .collect()
        };
        settle_early_exits(&fresh, cx.deadline).await;
        // Withdrawals may have come in while waiting.
        let withdrawn = cx.ctl.withdrawn();
        let selected: Vec<String> = selected
            .into_iter()
            .filter(|unit| !withdrawn.contains_key(unit))
            .collect();
        let mut units: Vec<UnitSummary> = Vec::new();
        let mut failed_hosts = Vec::new();
        {
            let st = cx.lane.st();
            for unit in &selected {
                if let Some(rec) = st.units.get(unit) {
                    units.push(unit_summary(&cx.reference, unit, rec));
                    if matches!(rec.host.state(), UnitState::Failed(_))
                        || (!rec.host.is_alive() && !rec.host.stop_was_requested())
                    {
                        failed_hosts.push((unit.clone(), rec.host.clone()));
                    }
                }
            }
        }
        for (unit, host) in failed_hosts {
            if !cx.failures.iter().any(|f| f.unit == unit) {
                cx.failures
                    .push(failure_of(&cx.reference, &unit, &host).await);
            }
        }
        let all_ready = units.iter().all(|u| u.state == "ready");
        if !withdrawn.is_empty() {
            units.extend(self.withdrawn_summaries(&cx, &withdrawn));
            cx.started.retain(|unit| !withdrawn.contains_key(unit));
            cx.already_running
                .retain(|unit| !withdrawn.contains_key(unit));
            cx.failures
                .retain(|failure| !withdrawn.contains_key(&failure.unit));
        }
        let branch = cx.lane.st().branch.clone();
        let warnings = self.ca_warnings(&cx.reference, &cx.lane.st());
        for warning in &warnings {
            self.bus.emit(
                events::LANE_WARNING,
                json!({"lane": cx.reference, "code": warning.code, "data": warning.data}),
            );
        }
        StartResult {
            warnings,
            started: cx
                .started
                .iter()
                .map(|u| format!("{}:{u}", cx.reference))
                .collect(),
            already_running: cx
                .already_running
                .iter()
                .map(|u| format!("{}:{u}", cx.reference))
                .collect(),
            lane: cx.reference,
            branch,
            whole_lane,
            ready: cx.failures.is_empty() && !cx.timed_out && all_ready,
            timed_out: cx.timed_out,
            failures: cx.failures,
            skipped_tasks: cx.skipped_tasks,
            leftovers: cx.leftovers,
            units,
            start_id: Some(cx.start_id),
        }
    }

    /// `stopped` summaries of the units of `cx` that a stop or restart withdrew.
    fn withdrawn_summaries(
        &self,
        cx: &StartCx,
        withdrawn: &BTreeMap<String, &'static str>,
    ) -> Vec<UnitSummary> {
        let mut ours: BTreeSet<String> = cx
            .started
            .iter()
            .chain(&cx.already_running)
            .cloned()
            .collect();
        if let Some(resolved) = &cx.resolved {
            for name in &cx.services {
                ours.extend(resolved.units_of(name).map(|service| service.unit.clone()));
            }
        }
        let st = cx.lane.st();
        withdrawn
            .iter()
            .filter(|(unit, _)| ours.contains(*unit))
            .map(|(unit, by)| {
                let service = st
                    .view
                    .as_ref()
                    .and_then(|view| view.services.iter().find(|s| &s.unit == unit));
                let mut summary = match (st.units.get(unit), service) {
                    (Some(rec), _) => unit_summary(&cx.reference, unit, rec),
                    (None, Some(service)) => crate::model::idle_summary(&cx.reference, service),
                    (None, None) => UnitSummary {
                        name: unit.clone(),
                        lane: cx.reference.clone(),
                        ..UnitSummary::default()
                    },
                };
                summary.state = "stopped".into();
                summary.reason = Some(withdrawn_reason(&cx.reference, unit, by));
                summary
            })
            .collect()
    }

    /// Decision 9: refuse while a running project with the same name has another identity.
    fn check_name_conflict(&self, project: &ProjectRt) -> Result<(), RpcError> {
        let name = project.name();
        for other in self.all_projects() {
            if other.id == project.id || other.name() != name {
                continue;
            }
            if other.lanes().iter().any(|lane| lane.st().any_running()) {
                let path = other.st().root.clone();
                return Err(RpcError::new(
                    "project_running_elsewhere",
                    json!({"project": name, "path": path}),
                    1,
                ));
            }
        }
        Ok(())
    }

    /// Stops units of a lane. Without selectors (and without `mine`) the whole lane
    /// stops after its `lane.stop` tasks. Returns the stopped units.
    pub(crate) async fn stop_lane(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        selectors: Option<Vec<String>>,
        mine: Option<&Actor>,
    ) -> Result<Vec<UnitSummary>, RpcError> {
        // Before the lock: a start holding it may be waiting on these queued units,
        // or on a task that never ends (a whole-lane stop interrupts that start).
        let queued_reference = lane.st().reference(&project.name());
        self.cancel_queued(lane, &queued_reference, selectors.as_deref(), mine);
        let named = selectors.as_ref().is_some_and(|s| !s.is_empty());
        if !named {
            // A whole-lane stop interrupts the start; `--mine` alone waits for it.
            if mine.is_none() {
                lane.cancel_start();
            }
            let _op = lane.op.lock().await;
            return self.stop_locked(project, lane, selectors, mine).await;
        }
        // Single units never wait for a start: they leave it through its control.
        // `op` held by anything else (clean, a whole stop) is waited for, with a
        // look at the lane now and then for a start that took it meanwhile.
        loop {
            if let Some(start) = lane.running_start() {
                return self
                    .stop_in_start(project, lane, &start, selectors, mine, "stop")
                    .await;
            }
            tokio::select! {
                op = lane.op.lock() => {
                    let _op = op;
                    return self.stop_locked(project, lane, selectors, mine).await;
                }
                () = tokio::time::sleep(START_POLL) => {}
            }
        }
    }

    /// Stops single units of a lane whose start (or restart) is running, without
    /// its `op`: the start skips units it has not spawned, stops waiting for the
    /// rest and reports them as stopped. Naming a task the start runs right now
    /// interrupts the start, as a whole-lane stop does. `by` is `stop` or
    /// `restart`.
    async fn stop_in_start(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        start: &Arc<StartCtl>,
        selectors: Option<Vec<String>>,
        mine: Option<&Actor>,
        by: &'static str,
    ) -> Result<Vec<UnitSummary>, RpcError> {
        let reference = lane.st().reference(&project.name());
        let mut names = Self::matching_units(lane, &reference, selectors.as_deref())?;
        if let Some(caller) = mine {
            // Only the caller's running units: the start's own were not theirs yet.
            let st = lane.st();
            names.retain(|name| {
                st.units.get(name).is_some_and(|rec| {
                    rec.host.is_alive() && is_mine(&rec.host.info().actor, caller)
                })
            });
        }
        start.withdraw(&names, by);
        // Again: one may have joined the queue before the withdrawal was visible.
        self.cancel_queued(lane, &reference, selectors.as_deref(), mine);
        if names.iter().any(|name| start.runs_task(name)) {
            start.cancel.cancel();
        }
        // A spawn past its last withdrawal check is recorded before the targets
        // are picked; every later one sees the withdrawal.
        lane.spawns_settled().await;
        let mut stopped = self
            .stop_named(project, lane, &reference, &names, mine, false, false)
            .await;
        // Units the start had not spawned yet are stopped too, as far as it goes.
        let st = lane.st();
        for name in &names {
            if stopped.iter().any(|summary| &summary.name == name) {
                continue;
            }
            let Some(service) = st
                .view
                .as_ref()
                .and_then(|view| view.services.iter().find(|s| &s.unit == name))
            else {
                continue;
            };
            if st.units.get(name).is_some_and(|rec| rec.host.is_alive()) {
                continue;
            }
            let mut summary = crate::model::idle_summary(&reference, service);
            summary.state = "stopped".into();
            summary.reason = Some(withdrawn_reason(&reference, name, by));
            stopped.push(summary);
        }
        Ok(stopped)
    }

    /// [`Inner::stop_lane`] for a caller that holds `op` and already withdrew the
    /// queued starts it covers.
    async fn stop_locked(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        selectors: Option<Vec<String>>,
        mine: Option<&Actor>,
    ) -> Result<Vec<UnitSummary>, RpcError> {
        // A unit between its last cancellation check and its record would be
        // missed by the target list below.
        lane.spawns_settled().await;
        let reference = lane.st().reference(&project.name());
        let whole = selectors.as_ref().is_none_or(Vec::is_empty) && mine.is_none();
        let runnable = hooks_runnable(&lane.st());
        if whole && runnable {
            // Daemon shutdown interrupts a hook that never ends; it waits for this lane.
            let cancel = self.shutdown.child_token();
            self.run_hook(project, lane, TaskWhen::Stop, &Actor::default(), &cancel)
                .await;
        }
        let names = Self::matching_units(lane, &reference, selectors.as_deref())?;
        Ok(self
            .stop_named(project, lane, &reference, &names, mine, whole, true)
            .await)
    }

    /// The lane's units (running, configured or orphaned) that `selectors`
    /// match; every one without selectors.
    fn matching_units(
        lane: &LaneRt,
        reference: &str,
        selectors: Option<&[String]>,
    ) -> Result<Vec<String>, RpcError> {
        let st = lane.st();
        let mut candidates: Vec<(String, Vec<String>)> = st
            .units
            .iter()
            .map(|(name, rec)| (name.clone(), rec.tags.clone()))
            .collect();
        if let Some(view) = &st.view {
            for service in &view.services {
                if !candidates.iter().any(|(n, _)| *n == service.unit) {
                    candidates.push((service.unit.clone(), service.tags.clone()));
                }
            }
        }
        for name in st.orphans.keys() {
            if !candidates.iter().any(|(n, _)| n == name) {
                candidates.push((name.clone(), Vec::new()));
            }
        }
        match selectors.filter(|s| !s.is_empty()) {
            Some(selectors) => crate::engine::match_units(selectors, &candidates, reference),
            None => Ok(candidates.into_iter().map(|(name, _)| name).collect()),
        }
    }

    /// Stops the live units and orphans among `names` (for `mine`, the caller's
    /// only) and returns them. `whole` settles crashed services and records the
    /// lane as stopped; `owns_lane` (the caller holds `op`) shows it `stopping`
    /// meanwhile, which a running start's `starting` must not be replaced by.
    #[allow(clippy::too_many_arguments)]
    async fn stop_named(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        reference: &str,
        names: &[String],
        mine: Option<&Actor>,
        whole: bool,
        owns_lane: bool,
    ) -> Vec<UnitSummary> {
        let (targets, orphans): (Vec<String>, Vec<String>) = {
            let st = lane.st();
            let names = names.to_vec();
            let orphans = names
                .iter()
                .filter(|name| {
                    st.orphans
                        .get(*name)
                        .is_some_and(|o| mine.is_none_or(|caller| is_mine(&o.actor, caller)))
                })
                .cloned()
                .collect();
            let targets = names
                .into_iter()
                .filter(|name| {
                    st.units.get(name).is_some_and(|rec| {
                        rec.host.is_alive()
                            && mine.is_none_or(|caller| is_mine(&rec.host.info().actor, caller))
                    })
                })
                .collect();
            (targets, orphans)
        };
        if owns_lane && (!targets.is_empty() || !orphans.is_empty()) {
            lane.st().busy = Some("stopping");
            self.emit_lane(project, lane);
        }
        let stops: Vec<(&String, UnitHost, StopPolicy)> = {
            let st = lane.st();
            targets
                .iter()
                .filter_map(|name| {
                    st.units
                        .get(name)
                        .map(|rec| (name, rec.host.clone(), rec.stop))
                })
                .collect()
        };
        let supervised = join_all(stops.iter().map(|(name, host, policy)| async move {
            if let Err(error) = host.stop(*policy).await {
                tracing::warn!(code = %error.code, "unit stop failed");
            }
            // The unit's watcher stops its syncs too, but only after this answer:
            // a stopped service must not have its folder mirrored any more.
            if !host.is_alive() {
                crate::local_sync::stop(lane, name, host).await;
            }
        }));
        let (_, terminated) =
            tokio::join!(supervised, self.terminate_orphans(project, lane, &orphans));
        if whole {
            // An explicit full stop settles crashed services: the lane reads idle.
            // Failed tasks keep their state, since a stop hook may just have failed.
            let st = lane.st();
            for rec in st.units.values().filter(|rec| rec.kind == Kind::Service) {
                rec.host.acknowledge_failure();
            }
        }
        let record = lane.st().record.clone();
        if whole && let Some(record) = record {
            self.save_desired(
                lane,
                &record,
                DesiredState::Stopped,
                record.profiles.clone(),
            )
            .await;
        }
        if owns_lane {
            lane.st().busy = None;
        }
        self.emit_lane(project, lane);
        let st = lane.st();
        let mut stopped: Vec<UnitSummary> = targets
            .iter()
            .filter_map(|name| {
                st.units
                    .get(name)
                    .map(|rec| unit_summary(reference, name, rec))
            })
            .collect();
        stopped.extend(terminated);
        stopped
    }

    /// Runs `lane.stop` or `lane.clean` tasks in file order until `cancel`
    /// fires; failures are reported only.
    async fn run_hook(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        hook: TaskWhen,
        actor: &Actor,
        cancel: &CancellationToken,
    ) -> Vec<Value> {
        let Ok(resolved) = self.resolve_full(project, lane).await else {
            return Vec::new();
        };
        let trigger = match hook {
            TaskWhen::Stop => "lane.stop",
            TaskWhen::Clean => "lane.clean",
            TaskWhen::Create => "lane.create",
            TaskWhen::Start => "lane.start",
        };
        let mut results = Vec::new();
        for (name, task) in &resolved.tasks {
            if task.when != Some(hook) || !task.active {
                continue;
            }
            // An interrupted hook starts nothing more; the rest read as cancelled.
            if cancel.is_cancelled() {
                results.push(json!({"task": name, "error": {"code": "cancelled", "data": null}}));
                continue;
            }
            let outcome = self
                .run_task(
                    project, lane, &resolved, name, trigger, actor, None, cancel, 0,
                )
                .await;
            results.push(match outcome {
                Ok(outcome) => json!({"task": name, "exit_code": outcome.code}),
                Err(error) => {
                    json!({"task": name, "error": {"code": error.code, "data": error.data}})
                }
            });
        }
        results
    }

    /// Restarts selected services: stop, wait for their ports, spawn with fresh plans.
    ///
    /// With a start of the lane running, the units leave that start first (as
    /// with `stop`), and the restart spawns them once its services phase is over:
    /// under `op` when the start is done by then, else alongside its remaining
    /// phases, without `op`.
    pub(crate) async fn restart_units(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        selectors: &[String],
        options: StartOptions,
    ) -> Result<StartResult, RpcError> {
        let (op, alongside) = loop {
            if let Some(start) = lane.running_start() {
                self.stop_in_start(
                    project,
                    lane,
                    &start,
                    Some(selectors.to_vec()),
                    None,
                    "restart",
                )
                .await?;
                start.services_done.cancelled().await;
                break match lane.op.try_lock() {
                    Ok(op) => (Some(op), None),
                    Err(_) => (None, Some(start)),
                };
            }
            tokio::select! {
                op = lane.op.lock() => break (Some(op), None),
                () = tokio::time::sleep(START_POLL) => {}
            }
        };
        // Alongside a start: counted as a spawn in flight throughout, so a whole
        // stop (which waits for spawns) sees what this restart spawns.
        let spawning = alongside.as_ref().map(|_| lane.spawn_guard());
        let (slot, ctl) = match &alongside {
            None => {
                let slot = lane.begin_start(&self.shutdown);
                let ctl = slot.ctl.clone();
                (Some(slot), ctl)
            }
            // Ends with the start it runs beside.
            Some(start) => (None, Arc::new(StartCtl::new(&start.cancel))),
        };
        let reference = lane.st().reference(&project.name());
        if let Some(env) = options.env.clone() {
            lane.st().env = Some(env.clone());
            project.st().last_env = Some(env);
        }
        if !options.overrides.is_empty() {
            lane.st().overrides = options.overrides.clone();
        }
        if !options.profiles.is_empty() {
            let config = lane.st().config.loaded().cloned();
            if let Some(config) = config {
                let selection = devlanes_resolve::select_profiles(&config, &options.profiles, &[])
                    .map_err(|error| {
                        with_ownership(crate::resolve::resolve_error(&error), &reference, &[])
                    })?;
                lane.st().profiles = selection.profiles.clone();
                let record = lane.st().record.clone();
                if let Some(record) = record {
                    self.save_desired(
                        lane,
                        &record,
                        record.desired_state,
                        selection.profiles.clone(),
                    )
                    .await;
                }
            }
        }
        let candidates: Vec<(String, Vec<String>)> = {
            let st = lane.st();
            st.view
                .as_ref()
                .map(|view| {
                    view.services
                        .iter()
                        .map(|s| (s.unit.clone(), s.tags.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let units = crate::engine::match_units(selectors, &candidates, &reference)?;
        let config = lane.st().config.loaded().cloned().ok_or_else(|| {
            with_ownership(
                RpcError::new("no_config", json!({"lane": reference}), 1),
                &reference,
                &[],
            )
        })?;
        let resolved = self
            .resolve_config(project, lane, config.clone())
            .await
            .map_err(|e| with_ownership(e, &reference, &[]))?;
        // A previous daemon's process is replaced like a running one: terminate it
        // first, so its ports are free for the new run.
        let orphans = self.terminate_orphans(project, lane, &units).await;
        for orphan in &orphans {
            for port in orphan.ports.values() {
                if !wait_port_free(*port, PORT_RELEASE).await {
                    let pid = self.integ.ports.holder(*port).await;
                    return Err(with_ownership(
                        RpcError::new(
                            "port_in_use",
                            json!({"port": port, "pid": pid, "unit": orphan.name}),
                            1,
                        ),
                        &reference,
                        &[],
                    ));
                }
            }
        }
        let old: Vec<(String, UnitHost, StopPolicy, Vec<u16>)> = {
            let st = lane.st();
            units
                .iter()
                .filter_map(|unit| {
                    st.units
                        .get(unit)
                        .filter(|rec| rec.host.is_alive())
                        .map(|rec| {
                            (
                                unit.clone(),
                                rec.host.clone(),
                                rec.stop,
                                rec.ports.values().copied().collect(),
                            )
                        })
                })
                .collect()
        };
        join_all(old.iter().map(|(_, host, policy, _)| host.stop(*policy))).await;
        for (unit, _, _, ports) in &old {
            for port in ports {
                if !wait_port_free(*port, PORT_RELEASE).await {
                    let pid = self.integ.ports.holder(*port).await;
                    return Err(with_ownership(
                        RpcError::new(
                            "port_in_use",
                            json!({"port": port, "pid": pid, "unit": unit}),
                            1,
                        ),
                        &reference,
                        &[],
                    ));
                }
            }
        }
        let mut services: Vec<String> = Vec::new();
        for unit in &units {
            if let Some(service) = resolved.services.get(unit)
                && !services.contains(&service.name)
            {
                services.push(service.name.clone());
            }
        }
        let mut cx = StartCx {
            project: project.clone(),
            lane: lane.clone(),
            reference: reference.clone(),
            actor: options.actor.clone(),
            services,
            profiles: lane.st().profiles.clone(),
            infra_enabled: true,
            infra_templated: false,
            whole_lane: false,
            setup_only: false,
            session: options.session.clone(),
            resolved: Some(resolved),
            deadline: Instant::now() + options.timeout,
            started: Vec::new(),
            already_running: Vec::new(),
            failures: Vec::new(),
            skipped_tasks: Vec::new(),
            leftovers: Vec::new(),
            infra_needs: Vec::new(),
            timed_out: false,
            all_at_once: options.all_at_once,
            start_id: options.start_id.unwrap_or_else(|| self.new_start_id()),
            cancel: ctl.cancel.clone(),
            ctl,
            config,
            arrival: options.arrival,
        };
        // A restart is a services phase alone; its events carry the plan as a start's do.
        let phase = ServicesPhase.name();
        self.phase_event(&cx, phase, "started", None, Some(Self::planned_units(&cx)));
        // A whole-lane stop, clean or reset came first: nothing is spawned.
        let result = if cx.cancel.is_cancelled() {
            PhaseResult::Error(start_cancelled(&cx))
        } else {
            ServicesPhase.run(self, &mut cx).await
        };
        cx.ctl.services_done.cancel();
        let (state, why) = result.event_state();
        self.phase_event(&cx, phase, state, why, None);
        // Replicas not named by the selectors stay as they are.
        cx.already_running.retain(|unit| units.contains(unit));
        drop(op);
        drop(spawning);
        if let PhaseResult::Error(error) = result {
            let started: Vec<String> = cx
                .started
                .iter()
                .map(|u| format!("{reference}:{u}"))
                .collect();
            return Err(with_ownership(error, &reference, &started));
        }
        let result = self.finish_start(cx, false).await;
        drop(slot);
        Ok(result)
    }

    /// `lane clean`: stop, `lane.clean` tasks, teardown, remove per-lane containers,
    /// free the block and records. Other lanes and shared infra stay untouched.
    pub(crate) async fn clean_lane(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        actor: Option<&Actor>,
    ) -> Result<Value, RpcError> {
        let actor = actor.cloned().unwrap_or_default();
        let reference = lane.st().reference(&project.name());
        // One hold of `op` from the stop to the release of the records: a start
        // waiting for the lane must not run in between.
        self.cancel_queued(lane, &reference, None, None);
        lane.cancel_start();
        let _op = lane.op.lock().await;
        // `exec` and `run` take no `op`: interrupt runs and wait for spawns in
        // flight, so the stop below sees every unit and none appears until the
        // records are gone.
        let _closed = lane.close_adhoc().await;
        let stopped = self.stop_locked(project, lane, None, None).await?;
        // A unit whose stop timed out (or an orphan that would not end) still runs;
        // releasing the lane would leave it untracked.
        self.refuse_running(project, lane)?;
        let mut tasks = Vec::new();
        let mut teardown = Vec::new();
        let runnable = hooks_runnable(&lane.st());
        if runnable {
            // Daemon shutdown interrupts a task or entry that never ends.
            let cancel = self.shutdown.child_token();
            tasks = self
                .run_hook(project, lane, TaskWhen::Clean, &actor, &cancel)
                .await;
            if let Ok(resolved) = self.resolve_full(project, lane).await {
                for (name, command) in &resolved.teardown {
                    let result = self
                        .run_command(project, lane, "teardown", command, &actor, &cancel)
                        .await;
                    teardown.push(match result {
                        Ok((outcome, _)) => json!({"entry": name, "exit_code": outcome.code, "ignored": command.ignore_errors}),
                        Err(error) => json!({"entry": name, "error": {"code": error.code}}),
                    });
                }
            }
            // A clean task or teardown entry may have left a unit that would not stop.
            self.refuse_running(project, lane)?;
        }
        let index = lane.st().index();
        let infra = match index {
            Some(index) => match self.integ.infra.remove_lane(&project.name(), index).await {
                Ok(value) => value,
                Err(error) => {
                    json!({"error": {"code": error.code, "message": error.message}})
                }
            },
            None => Value::Null,
        };
        let record = lane.st().record.clone();
        if let Some(record) = &record {
            // Every run must be finished before the lane's records may be released;
            // none is alive by now, so each row records a real exit.
            let runs: Vec<_> = lane
                .st()
                .units
                .values()
                .filter(|rec| !rec.host.is_alive())
                .filter_map(|rec| rec.run_id.map(|id| (id, rec.host.exit_outcome())))
                .collect();
            for (id, outcome) in runs {
                let outcome = outcome.unwrap_or(ExitOutcome {
                    code: None,
                    signal: None,
                });
                // A row left unfinished makes `clean_lane` refuse with `active_runs`.
                self.finish_run(id, outcome).await;
            }
            // Setup records, once-only markers and task history go in the same
            // transaction as the lane's block.
            self.store
                .clean_lane(record.id)
                .await
                .map_err(store_error)?;
        }
        let path = lane.st().path.clone();
        self.secrets.forget(Some(&path));
        let present = {
            let mut st = lane.st();
            st.record = None;
            st.allocation = None;
            st.view = None;
            st.units.clear();
            st.started_at_ms = None;
            st.infra_ns = None;
            st.infra_containers.clear();
            st.peaks.clear();
            st.moves_to = None;
            st.present
        };
        self.bus
            .emit(events::LANE_REMOVED, json!({"lane": reference}));
        if !present {
            project.st().lanes.remove(&lane.worktree);
            lane.watch.cancel();
        }
        self.publish_routes();
        Ok(json!({
            "lane": reference,
            "stopped": stopped.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
            "tasks": tasks,
            "teardown": teardown,
            "infra": infra,
        }))
    }

    /// Worktree auto-start or setup with the project's last request env.
    pub(crate) async fn auto_start(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        setup_only: bool,
    ) -> Result<(), RpcError> {
        let env = project.st().last_env.clone();
        self.start_lane(
            project,
            lane,
            StartOptions {
                selectors: Vec::new(),
                profiles: Vec::new(),
                env,
                overrides: EnvMap::new(),
                actor: Actor::default(),
                timeout: self.opts.start_timeout,
                session: None,
                setup_only,
                all_at_once: false,
                start_id: None,
                arrival: None,
            },
        )
        .await
        .map(drop)
    }

    /// Daemon shutdown: stops every live unit with its policy. With `hooks` (an
    /// explicit shutdown) each lane with live units first runs its `lane.stop`
    /// tasks, as a whole-lane stop does, until `hooks` fires. Returns the
    /// stopped units (`{lane, unit}`) and the task results.
    pub(crate) async fn stop_everything(
        self: &Arc<Self>,
        hooks: Option<&CancellationToken>,
    ) -> (Vec<Value>, Vec<Value>) {
        let lanes = self.all_lanes();
        let _ops = self.lock_lanes(&lanes).await;
        let tasks = match hooks {
            Some(cancel) => self.shutdown_hooks(&lanes, cancel).await,
            None => Vec::new(),
        };
        (Self::stop_lanes_locked(&lanes).await, tasks)
    }

    /// The `lane.stop` tasks of an explicit daemon shutdown, for every lane in
    /// `lanes` with live units (the caller holds their `op`): lanes run
    /// concurrently, each lane's tasks in file order. Each result is
    /// `{lane, task, exit_code | error}`; a failed task is logged and reported
    /// and never keeps the other tasks or the unit stops from running.
    async fn shutdown_hooks(
        self: &Arc<Self>,
        lanes: &[crate::engine::LanePair],
        cancel: &CancellationToken,
    ) -> Vec<Value> {
        let runs = lanes
            .iter()
            .filter(|(_, lane)| {
                let st = lane.st();
                hooks_runnable(&st) && st.any_alive()
            })
            .map(|(project, lane)| async move {
                lane.st().shutdown_hooks = true;
                let results = self
                    .run_hook(project, lane, TaskWhen::Stop, &Actor::default(), cancel)
                    .await;
                lane.st().shutdown_hooks = false;
                let reference = lane.st().reference(&project.name());
                results
                    .into_iter()
                    .map(|mut result| {
                        if result.get("exit_code").and_then(Value::as_i64) != Some(0) {
                            tracing::warn!(lane = %reference, task = %result["task"], result = %result, "lane.stop task failed during daemon shutdown");
                        }
                        result["lane"] = json!(reference);
                        result
                    })
                    .collect::<Vec<_>>()
            });
        join_all(runs).await.into_iter().flatten().collect()
    }

    /// Withdraws every queued start, interrupts running starts and takes the `op`
    /// lock of each of `lanes`, in order, so nothing starts in them meanwhile.
    pub(crate) async fn lock_lanes<'a>(
        &self,
        lanes: &'a [crate::engine::LanePair],
    ) -> Vec<tokio::sync::MutexGuard<'a, ()>> {
        self.pacing.cancel_all();
        for (_, lane) in lanes {
            lane.cancel_start();
        }
        let mut guards = Vec::with_capacity(lanes.len());
        for (_, lane) in lanes {
            guards.push(lane.op.lock().await);
        }
        guards
    }

    /// Stops every live unit of `lanes`, whose `op` locks the caller holds, once
    /// their spawns in flight landed; all at once, so the longest stop timeout
    /// bounds the wait. Returns the stopped units as `{lane, unit}`.
    pub(crate) async fn stop_lanes_locked(lanes: &[crate::engine::LanePair]) -> Vec<Value> {
        let mut stops = Vec::new();
        let mut stopped = Vec::new();
        for (project, lane) in lanes {
            lane.spawns_settled().await;
            let st = lane.st();
            let reference = st.reference(&project.name());
            for (name, rec) in st.units.iter().filter(|(_, rec)| rec.host.is_alive()) {
                stops.push((rec.host.clone(), rec.stop));
                stopped.push(json!({"lane": reference, "unit": name}));
            }
        }
        join_all(stops.iter().map(|(host, policy)| host.stop(*policy))).await;
        stopped
    }

    /// `lane_running` while a unit or a previous daemon's orphan still runs in `lane`.
    fn refuse_running(&self, project: &ProjectRt, lane: &LaneRt) -> Result<(), RpcError> {
        let st = lane.st();
        let mut units: Vec<String> = st
            .units
            .iter()
            .filter(|(_, rec)| rec.host.is_alive())
            .map(|(name, _)| name.clone())
            .collect();
        units.extend(st.orphans.keys().cloned());
        if units.is_empty() {
            return Ok(());
        }
        Err(RpcError::new(
            "lane_running",
            json!({"lane": st.reference(&project.name()), "units": units}),
            1,
        ))
    }

    /// Stores the lane's desired state and profiles, then refreshes its record.
    async fn save_desired(
        &self,
        lane: &LaneRt,
        record: &devlanes_state::Lane,
        desired: DesiredState,
        profiles: Vec<String>,
    ) {
        let id = record.id;
        let saved = persist("lane desired state", || {
            self.store
                .set_lane_desired_profiles(id, desired, profiles.clone())
        })
        .await;
        if saved && let Some(updated) = self.store.lane(id).await.ok().flatten() {
            lane.st().record = Some(updated);
        }
    }

    /// The lane's profiles and the services they select, for `why`.
    pub(crate) fn profile_services(lane: &LaneRt) -> Option<(Vec<String>, BTreeSet<String>)> {
        let st = lane.st();
        let config = st.config.loaded()?;
        let selection = devlanes_resolve::select_profiles(config, &st.profiles, &[]).ok()?;
        Some((selection.profiles, selection.services.into_iter().collect()))
    }

    /// Runs the `ready` checks of the lane's plain containers concurrently; the
    /// first failure is returned.
    async fn containers_ready(&self, lane: &LaneRt, spec: &InfraSpec) -> Result<(), RpcError> {
        let mut env: EnvMap = lane.st().env.as_deref().cloned().unwrap_or_default();
        env.extend(spec.env.clone());
        let checks = spec.containers.iter().filter_map(|container| {
            let ready = container.ready.as_ref()?;
            let ports: Vec<(String, u16)> = container
                .ports
                .iter()
                .filter_map(|(name, _)| {
                    let key = format!("infra.{}/{name}", container.name);
                    spec.ports.get(&key).map(|port| (name.clone(), *port))
                })
                .collect();
            let env = &env;
            Some(async move {
                let logs = || async {
                    self.integ
                        .infra
                        .container_logs(spec, &container.name, 200)
                        .await
                        .unwrap_or_default()
                };
                crate::ready::container_ready(
                    &container.name,
                    ready,
                    &ports,
                    env,
                    &self.integ.shell,
                    logs,
                )
                .await
            })
        });
        join_all(checks).await.into_iter().collect()
    }

    /// Settles a templated profile `infra` by planning the lane against what already
    /// runs. A planning failure keeps infra on; the resolve phase reports it.
    async fn profiles_enable_infra(self: &Arc<Self>, cx: &StartCx, spec: &InfraSpec) -> bool {
        if let Ok(up) = self.integ.infra.namespace(spec).await {
            let mut st = cx.lane.st();
            st.infra_ns = Some(up.namespace);
            st.infra_containers = up.containers;
            st.setup_env = up.setup_env;
        }
        self.resolve_config(&cx.project, &cx.lane, cx.config.clone())
            .await
            .ok()
            .is_none_or(|resolved| resolved.infra_enabled)
    }
}

/// `config_error`, located like `config check`'s, when an `infra.compose` file is
/// missing from `config_dir`: the daemon parses the config from text, so the
/// loader's file check never ran, and Docker would only report it later and
/// less clearly.
async fn check_compose_files(
    config: &Arc<devlanes_config::ProjectConfig>,
    config_dir: std::path::PathBuf,
) -> Result<(), RpcError> {
    if config.infra.compose.is_none() {
        return Ok(());
    }
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        devlanes_config::check_compose_files(&config, &config_dir).map_err(|error| {
            let mut shaped = crate::model::config_error(&error);
            let file = if error.location.file == "devlanes.local.yaml" {
                "devlanes.local.yaml"
            } else {
                "devlanes.yaml"
            };
            shaped.data["file"] = json!(config_dir.join(file));
            shaped
        })
    })
    .await
    .map_err(|error| internal("config_error", error))?
}

/// Runs a state.db write, retrying briefly as [`Inner::finish_run`] does; a
/// write that still fails is logged. True once written.
pub(crate) async fn persist<T, F, Fut>(what: &str, write: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, devlanes_state::Error>>,
{
    const ATTEMPTS: u32 = 3;
    let mut delay = Duration::from_millis(100);
    for attempt in 1..=ATTEMPTS {
        match write().await {
            Ok(_) => return true,
            // Shutdown closed the store; nothing more can be written.
            Err(devlanes_state::Error::Closed) => return false,
            Err(error) if attempt == ATTEMPTS => {
                tracing::warn!(%error, what, "state.db write failed");
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
    false
}

/// Finishes the row and emits the closing `task` event of a task run whose
/// future was dropped before it got there.
struct TaskRunGuard {
    engine: Arc<Inner>,
    /// `None` once the run closed normally.
    run: Option<devlanes_state::TaskRunId>,
    reference: String,
    task: String,
}

impl Drop for TaskRunGuard {
    /// Drop cannot wait for state.db, so the row is finished on the runtime.
    fn drop(&mut self) {
        let Some(id) = self.run.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let engine = self.engine.clone();
        let reference = std::mem::take(&mut self.reference);
        let task = std::mem::take(&mut self.task);
        runtime.spawn(async move {
            engine
                .close_task_run(
                    id,
                    &reference,
                    &task,
                    None,
                    "failed",
                    Some(json!({"code": "cancelled"})),
                )
                .await;
        });
    }
}

/// Waits for a one-shot unit, stopping it after `timeout` or once `cancel`
/// fires; an interrupted unit has no exit code.
async fn finish(
    host: &UnitHost,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
) -> ExitOutcome {
    tokio::select! {
        // An exit that is already there wins over a cancellation.
        biased;
        outcome = finish_within(host, timeout) => outcome,
        () = cancel.cancelled() => {
            if let Err(error) = host.stop(StopPolicy::default()).await {
                tracing::warn!(code = %error.code, unit = %host.info().name, "interrupted unit did not stop");
            }
            ExitOutcome {
                code: None,
                signal: host.exit_outcome().and_then(|o| o.signal),
            }
        }
    }
}

/// Waits for a one-shot unit, stopping it after `timeout`.
async fn finish_within(host: &UnitHost, timeout: Option<Duration>) -> ExitOutcome {
    match timeout {
        Some(limit) => match tokio::time::timeout(limit, host.wait_exit()).await {
            Ok(outcome) => outcome,
            Err(_) => {
                let _ = host.stop(StopPolicy::default()).await;
                ExitOutcome {
                    code: None,
                    signal: host.exit_outcome().and_then(|o| o.signal),
                }
            }
        },
        None => host.wait_exit().await,
    }
}

/// How long after its spawn a unit's exit still counts as a failed start.
const EARLY_EXIT: Duration = Duration::from_millis(500);

/// Waits until each live unit of `hosts` has run for [`EARLY_EXIT`] (or the
/// start's deadline passed), returning early once all of them exited.
async fn settle_early_exits(hosts: &[UnitHost], deadline: Instant) {
    let now = now_ms();
    let youngest = hosts
        .iter()
        .filter(|host| host.is_alive())
        .map(|host| host.info().started_at_ms)
        .max();
    let Some(started) = youngest else {
        return;
    };
    let early = u64::try_from(EARLY_EXIT.as_millis()).unwrap_or(u64::MAX);
    let left = Duration::from_millis((started + early).saturating_sub(now));
    let until = (Instant::now() + left).min(deadline);
    let all_exited = join_all(hosts.iter().map(UnitHost::wait_exit));
    let _ = tokio::time::timeout_at(until, all_exited).await;
}

/// Polls a TCP port until nothing accepts on it.
async fn wait_port_free(port: u16, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !crate::ready::tcp_open(port).await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Sends a unit's log lines as `log` events until it exits or the client leaves.
pub(crate) async fn stream_logs(host: UnitHost, session: Session) {
    let name = host.info().name.clone();
    let disconnected = session.disconnected();
    // Earlier runs of the unit share its log; only this run's lines are streamed.
    let mut cursor: Option<String> = host.run_cursor();
    loop {
        // The exit is published after output drained, so this pass is the last one.
        let alive = host.is_alive();
        let query = LogQuery {
            cursor: cursor.clone(),
            max_bytes: Some(256 * 1024),
            ..LogQuery::default()
        };
        if let Ok(page) = host.logs(query).await {
            for line in page.items {
                cursor = Some(format!("{}#{}", line.unit, line.seq));
                let event = devlanes_proto::Event {
                    seq: 0,
                    event: events::LOG.into(),
                    data: json!({"unit": name, "line": line}),
                };
                if session.event(event).await.is_err() {
                    return;
                }
            }
        }
        if !alive || disconnected.is_cancelled() {
            return;
        }
        tokio::select! {
            () = disconnected.cancelled() => return,
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    /// A failing write is retried up to three times; a closed store is not retried.
    async fn persist_retries_then_gives_up() {
        let calls = AtomicU32::new(0);
        let flaky = persist("test", || {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if call < 2 {
                    Err(devlanes_state::Error::Invalid("busy".into()))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(flaky);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        calls.store(0, Ordering::SeqCst);
        let failing = persist("test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(devlanes_state::Error::Invalid("busy".into())) }
        })
        .await;
        assert!(!failing);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        calls.store(0, Ordering::SeqCst);
        let closed = persist("test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(devlanes_state::Error::Closed) }
        })
        .await;
        assert!(!closed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
