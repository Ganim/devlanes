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
    Actor, LogQuery, RpcError, StartResult, UnitFailureData, UnitSummary, events,
};
use devlanes_resolve::{Launch, ReadySpec, ResolvedCommand, ResolvedLane, ResolvedService};
use devlanes_state::{DesiredState, TaskRunInput};
use futures_util::future::{BoxFuture, join_all};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{
    Session,
    engine::{Inner, with_ownership},
    integrations::{InfraPortNeed, ManagedUnit, PlannedUnit},
    model::{EnvMap, Kind, LaneRt, ProjectRt, UnitRec, core_actor, is_mine, unit_summary},
    ready::{Readiness, run_check},
    registry::{internal, store_error},
    units::{UnitHost, UnitInfo, UnitState, now_ms},
};

/// Log lines attached to every failure (cli-design 3.4).
pub(crate) const TAIL: usize = 20;
/// How long a restart waits for the old process's ports to close.
const PORT_RELEASE: Duration = Duration::from_secs(15);
/// Nested `steps` deeper than this are treated as a cycle.
const MAX_STEP_DEPTH: usize = 16;

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
}

/// Copies an environment with its provenance (the type is deliberately not `Clone`).
pub(crate) fn copy_env(env: &Environment) -> Environment {
    Environment {
        values: env.values.clone(),
        sources: env.sources.clone(),
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
            if !path.starts_with(&lane_path)
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

/// Maps a resolved readiness spec to the checker's form.
fn readiness(spec: &ReadySpec) -> Readiness {
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
        ReadySpec::Cmd { shell, cwd } => Readiness::Cmd {
            command: shell.clone(),
            cwd: cwd.clone(),
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

/// Mutable context threaded through the start pipeline.
pub(crate) struct StartCx {
    pub project: Arc<ProjectRt>,
    pub lane: Arc<LaneRt>,
    pub reference: String,
    pub actor: Actor,
    pub services: Vec<String>,
    pub profile: Option<String>,
    pub infra_enabled: bool,
    pub whole_lane: bool,
    pub setup_only: bool,
    pub session: Option<Session>,
    pub resolved: Option<Arc<ResolvedLane>>,
    pub deadline: Instant,
    pub started: Vec<String>,
    pub already_running: Vec<String>,
    pub failures: Vec<UnitFailureData>,
    pub skipped_tasks: Vec<String>,
    pub infra_needs: Vec<InfraPortNeed>,
    pub timed_out: bool,
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

/// Infra failures as RPC errors.
fn infra_error(error: crate::integrations::InfraError) -> RpcError {
    RpcError::new(error.code, json!({"message": error.message}), 1)
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
        if let Err(error) = engine.allocate_lane(&cx.project, &cx.lane, &needs).await {
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
            Some(reason(
                "profile_disables_infra",
                json!({"profile": cx.profile}),
            ))
        } else if !cx.whole_lane && !cx.setup_only {
            Some(reason("lane_running", json!({})))
        } else {
            None
        };
        let result = match &skip {
            Some(_) => engine.integ.infra.namespace(&spec).await,
            None => engine.integ.infra.up(&spec).await,
        };
        match result {
            Ok(up) => {
                let mut st = cx.lane.st();
                st.infra_ns = Some(up.namespace);
                st.setup_env = up.setup_env;
            }
            Err(error) if skip.is_some() => {
                tracing::debug!(code = %error.code, "infra namespace unavailable");
            }
            Err(error) => return PhaseResult::Error(infra_error(error)),
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
        engine.refresh_views(&cx.project).await;
        match engine.resolve_full(&cx.project, &cx.lane).await {
            Ok(resolved) => {
                cx.resolved = Some(resolved);
                PhaseResult::Done
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
            return PhaseResult::Skipped(reason(
                "profile_disables_infra",
                json!({"profile": cx.profile}),
            ));
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
        let runs = pending.iter().map(|(name, _)| {
            let engine = engine.clone();
            let project = cx.project.clone();
            let lane = cx.lane.clone();
            let actor = cx.actor.clone();
            let resolved = resolved.clone();
            let name = name.clone();
            async move {
                let Some(command) = resolved.setup.get(&name) else {
                    return (name, Err(RpcError::new("not_found", json!({}), 3)));
                };
                let result = engine
                    .run_command(&project, &lane, "setup", command, &actor)
                    .await;
                (name, result)
            }
        });
        let results = join_all(runs).await;
        let mut failed = Vec::new();
        for (name, result) in results {
            let ignore = resolved.setup.get(&name).is_some_and(|c| c.ignore_errors);
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
            if !wait_settled(&hosts, cx.deadline).await {
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
        for (name, hook) in chosen {
            let outcome = engine
                .run_task(
                    &cx.project,
                    &cx.lane,
                    &resolved,
                    &name,
                    hook,
                    &cx.actor,
                    cx.session.as_ref(),
                    0,
                )
                .await;
            match outcome {
                Ok(TaskOutcome { code: Some(0), .. }) => {}
                Ok(TaskOutcome { code, host }) => {
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
        // A locked vault fails the start as a whole: `devlanes unlock` fixes it.
        for name in &cx.services {
            if let Some(error) = resolved.errors.get(&format!("services.{name}")) {
                let error = crate::resolve::resolve_error(error);
                if error.code == "secrets_locked" {
                    return PhaseResult::Error(error);
                }
            }
        }
        let mut to_spawn: Vec<&ResolvedService> = Vec::new();
        for name in &cx.services {
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
        let spawns = to_spawn.iter().map(|service| {
            let engine = engine.clone();
            let project = cx.project.clone();
            let lane = cx.lane.clone();
            let request = SpawnRequest::service(service, &cx.actor);
            let ready = service.ready.clone();
            let unit = service.unit.clone();
            async move {
                let result = engine.spawn_unit(&project, &lane, request).await;
                if let Ok((host, managed)) = &result {
                    let host = host.clone();
                    let managed = managed.clone();
                    tokio::spawn(async move {
                        run_check(
                            &host,
                            readiness(&ready.check),
                            ready.timeout,
                            || async move { managed.listening_ports().await },
                        )
                        .await;
                    });
                }
                (unit, result.map(|(host, _)| host))
            }
        });
        for (unit, result) in join_all(spawns).await {
            match result {
                Ok(_) => cx.started.push(unit),
                Err(error) => cx.failures.push(UnitFailureData {
                    lane: cx.reference.clone(),
                    unit,
                    reason: Some(error),
                    ..UnitFailureData::default()
                }),
            }
        }
        PhaseResult::Done
    }
}

/// Exit of a task: its code (`None` when killed or timed out) and its last unit.
pub(crate) struct TaskOutcome {
    pub code: Option<i32>,
    pub host: Option<UnitHost>,
}

/// Options of one start request.
pub(crate) struct StartOptions {
    pub selectors: Vec<String>,
    pub profile: Option<String>,
    pub env: Option<Arc<EnvMap>>,
    pub overrides: EnvMap,
    pub actor: Actor,
    pub timeout: Duration,
    pub session: Option<Session>,
    pub setup_only: bool,
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
        let (lane_path, record, reference, previous) = {
            let st = lane.st();
            let reference = st.reference(&project.name());
            let record = st
                .record
                .clone()
                .ok_or_else(|| RpcError::new("unassigned", json!({"lane": reference}), 1))?;
            let previous = st
                .units
                .get(&request.name)
                .map(|rec| (rec.run_id, rec.host.clone()));
            (st.path.clone(), record, reference, previous)
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
            if let Some(id) = run_id {
                let outcome = host.exit_outcome().unwrap_or(ExitOutcome {
                    code: None,
                    signal: None,
                });
                let _ = self.store.finish_unit_run(id, outcome).await;
            }
        }
        write_files(lane_path, request.files.clone()).await?;
        let managed = self
            .integ
            .runner
            .spawn(request.spec)
            .await
            .map_err(|error| {
                RpcError::new(
                    "spawn_failed",
                    json!({"unit": request.name, "message": error.to_string()}),
                    1,
                )
            })?;
        let identity = managed.identity();
        let log_dir = self
            .opts
            .logs
            .join(project.name())
            .join(record.index.to_string())
            .join(request.name.replace('/', "_"));
        let info = UnitInfo {
            name: request.name.clone(),
            lane: reference.clone(),
            kind: request.kind.word(),
            actor: request.actor.clone(),
            started_at_ms: now_ms(),
            log_dir: log_dir.clone(),
        };
        let unit: Arc<dyn Unit> = managed.clone();
        let host = match UnitHost::start(info, unit, identity) {
            Ok(host) => host,
            Err(error) => {
                // Without a host nobody could see or stop it; this spawn is ours to undo.
                let _ = managed.stop(request.stop).await;
                return Err(error);
            }
        };
        let run_id = match identity {
            Some(process) if process.pid > 0 && process.sid > 0 => self
                .store
                .record_unit_run(devlanes_state::UnitRunInput {
                    lane_id: record.id,
                    unit: request.name.clone(),
                    process,
                    actor: core_actor(&request.actor),
                    log_path: Some(log_dir),
                })
                .await
                .map(|run| run.id)
                .map_err(|error| tracing::warn!(%error, "unit run not recorded"))
                .ok(),
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
            },
        );
        self.watch_unit(
            project.clone(),
            lane.clone(),
            request.name,
            host.clone(),
            run_id,
        );
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
            if let Some(id) = run_id {
                let _ = engine.store.finish_unit_run(id, outcome).await;
            }
            engine.emit_unit(&project, &lane, &name, &host);
        });
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
                .map(|rec| unit_summary(&st.reference(&project.name()), name, rec))
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

    /// Emits a `lane.phase` event.
    fn phase_event(&self, lane: &str, phase: &str, state: &str, reason: Option<&RpcError>) {
        let mut data = json!({"lane": lane, "phase": phase, "state": state});
        if let Some(reason) = reason {
            data["reason"] = json!({"code": reason.code, "data": reason.data});
        }
        self.bus.emit(events::PHASE, data);
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
    ) -> Result<(ExitOutcome, UnitHost), RpcError> {
        let extra = lane.st().setup_env.clone();
        let name = format!("{phase}.{}", command.name);
        let request =
            SpawnRequest::oneshot(&name, Kind::Task, &command.launch, false, &extra, actor);
        let (host, _) = self.spawn_unit(project, lane, request).await?;
        let outcome = finish(&host, command.timeout).await;
        Ok((outcome, host))
    }

    /// Runs a task (or its steps) to completion, recording `task_runs`.
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
            let mut outcome = TaskOutcome {
                code: Some(0),
                host: None,
            };
            match &task.launch {
                None => {
                    for step in &task.steps {
                        let step_outcome = if let Some(entry) = step.strip_prefix("infra.setup.") {
                            self.step_command(project, lane, resolved, "setup", entry, actor)
                                .await?
                        } else if let Some(entry) = step.strip_prefix("infra.teardown.") {
                            self.step_command(project, lane, resolved, "teardown", entry, actor)
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
                                depth + 1,
                            )
                            .await?
                        };
                        if step_outcome.code != Some(0) {
                            outcome = step_outcome;
                            break;
                        }
                    }
                }
                Some(launch) => {
                    let extra = lane.st().setup_env.clone();
                    let request =
                        SpawnRequest::oneshot(name, Kind::Task, launch, task.pty, &extra, actor);
                    let (host, _) = self.spawn_unit(project, lane, request).await?;
                    let exit = match session {
                        Some(session) => {
                            let follower = stream_logs(host.clone(), session.clone());
                            let (exit, ()) = tokio::join!(finish(&host, task.timeout), follower);
                            exit
                        }
                        None => finish(&host, task.timeout).await,
                    };
                    outcome = TaskOutcome {
                        code: exit.code,
                        host: Some(host),
                    };
                }
            }
            let _ = self.store.finish_task_run(run.id, outcome.code).await;
            let state = if outcome.code == Some(0) {
                "done"
            } else {
                "failed"
            };
            self.task_event(&reference, name, state, outcome.code, None);
            Ok(outcome)
        })
    }

    /// Runs a setup/teardown entry as a task step and updates its done-record.
    async fn step_command(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        resolved: &ResolvedLane,
        phase: &str,
        entry: &str,
        actor: &Actor,
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
            .run_command(project, lane, phase, command, actor)
            .await?;
        let ok = outcome.code == Some(0) || command.ignore_errors;
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
        Ok(TaskOutcome {
            code: if ok { Some(0) } else { outcome.code },
            host: Some(host),
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
                    1,
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
            if options.profile.is_some() {
                st.profile = options.profile.clone();
            }
        }
        if let Some(env) = options.env.clone() {
            project.st().last_env = Some(env);
        }
        self.apply_move(project, lane).await.map_err(fail)?;
        self.refresh_views(project).await;
        let reference = lane.st().reference(&project.name());
        let fail = |error: RpcError| with_ownership(error, &reference, &[]);
        let config = lane
            .st()
            .config
            .loaded()
            .cloned()
            .ok_or_else(|| fail(RpcError::new("no_config", json!({"lane": reference}), 1)))?;
        let profile = lane.st().profile.clone();
        let selection = devlanes_resolve::select(&config, profile.as_deref(), &options.selectors)
            .map_err(|error| fail(crate::resolve::resolve_error(&error)))?;
        let infra_enabled = !matches!(
            selection
                .profile_options
                .as_ref()
                .and_then(|o| o.infra.as_ref()),
            Some(Templated::Literal(false))
        );
        let whole_lane = !lane.st().any_alive();
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
            profile: selection.profile.clone(),
            infra_enabled,
            whole_lane,
            setup_only: options.setup_only,
            session: options.session.clone(),
            resolved: None,
            deadline: Instant::now() + options.timeout,
            started: Vec::new(),
            already_running: Vec::new(),
            failures: Vec::new(),
            skipped_tasks: Vec::new(),
            infra_needs: Vec::new(),
            timed_out: false,
        };
        let mut hard_error = None;
        for phase in start_pipeline() {
            if phase.visible() {
                self.phase_event(&reference, phase.name(), "started", None);
            }
            let result = phase.run(self, &mut cx).await;
            let (state, why) = match &result {
                PhaseResult::Done => ("done", None),
                PhaseResult::Skipped(why) => ("skipped", Some(why)),
                PhaseResult::Halt(why) | PhaseResult::Error(why) => ("failed", Some(why)),
            };
            if phase.visible() {
                self.phase_event(&reference, phase.name(), state, why);
            }
            match result {
                PhaseResult::Done | PhaseResult::Skipped(_) => {}
                PhaseResult::Halt(_) => break,
                PhaseResult::Error(error) => {
                    hard_error = Some(error);
                    break;
                }
            }
        }
        let record = lane.st().record.clone();
        if let Some(record) = record {
            let desired = if options.setup_only {
                record.desired_state
            } else {
                DesiredState::Running
            };
            let _ = self
                .store
                .set_lane_desired(record.id, desired, selection.profile.clone())
                .await;
            if let Some(updated) = self.store.lane(record.id).await.ok().flatten() {
                lane.st().record = Some(updated);
            }
        }
        let _ = self.store.mark_project_started(project.id).await;
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
        Ok(result)
    }

    /// Waits for started units and assembles the [`StartResult`].
    async fn finish_start(self: &Arc<Self>, mut cx: StartCx, whole_lane: bool) -> StartResult {
        let selected: Vec<String> = cx
            .started
            .iter()
            .chain(&cx.already_running)
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
        let branch = cx.lane.st().branch.clone();
        StartResult {
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
            units,
        }
    }

    /// Decision 9: refuse while a running project with the same name has another identity.
    fn check_name_conflict(&self, project: &ProjectRt) -> Result<(), RpcError> {
        let name = project.name();
        for other in self.all_projects() {
            if other.id == project.id || other.name() != name {
                continue;
            }
            if other.lanes().iter().any(|lane| lane.st().any_alive()) {
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
        let _op = lane.op.lock().await;
        let reference = lane.st().reference(&project.name());
        let whole = selectors.as_ref().is_none_or(Vec::is_empty) && mine.is_none();
        let runnable = {
            let st = lane.st();
            st.present && st.env.is_some() && st.record.is_some()
        };
        if whole && runnable {
            self.run_hook(project, lane, TaskWhen::Stop, &Actor::default())
                .await;
        }
        let targets: Vec<String> = {
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
            let names = match selectors.as_ref().filter(|s| !s.is_empty()) {
                Some(selectors) => crate::engine::match_units(selectors, &candidates, &reference)?,
                None => candidates.into_iter().map(|(name, _)| name).collect(),
            };
            names
                .into_iter()
                .filter(|name| {
                    st.units.get(name).is_some_and(|rec| {
                        rec.host.is_alive()
                            && mine.is_none_or(|caller| is_mine(&rec.host.info().actor, caller))
                    })
                })
                .collect()
        };
        if !targets.is_empty() {
            lane.st().busy = Some("stopping");
            self.emit_lane(project, lane);
        }
        let stops: Vec<(UnitHost, StopPolicy)> = {
            let st = lane.st();
            targets
                .iter()
                .filter_map(|name| st.units.get(name).map(|rec| (rec.host.clone(), rec.stop)))
                .collect()
        };
        join_all(stops.iter().map(|(host, policy)| async move {
            if let Err(error) = host.stop(*policy).await {
                tracing::warn!(code = %error.code, "unit stop failed");
            }
        }))
        .await;
        let record = lane.st().record.clone();
        if whole && let Some(record) = record {
            let _ = self
                .store
                .set_lane_desired(record.id, DesiredState::Stopped, record.profile.clone())
                .await;
            if let Some(updated) = self.store.lane(record.id).await.ok().flatten() {
                lane.st().record = Some(updated);
            }
        }
        lane.st().busy = None;
        self.emit_lane(project, lane);
        let st = lane.st();
        Ok(targets
            .iter()
            .filter_map(|name| {
                st.units
                    .get(name)
                    .map(|rec| unit_summary(&reference, name, rec))
            })
            .collect())
    }

    /// Runs `lane.stop` or `lane.clean` tasks in file order; failures are reported only.
    async fn run_hook(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        hook: TaskWhen,
        actor: &Actor,
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
            let outcome = self
                .run_task(project, lane, &resolved, name, trigger, actor, None, 0)
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
    pub(crate) async fn restart_units(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        selectors: &[String],
        options: StartOptions,
    ) -> Result<StartResult, RpcError> {
        let op = lane.op.lock().await;
        let reference = lane.st().reference(&project.name());
        if let Some(env) = options.env.clone() {
            lane.st().env = Some(env.clone());
            project.st().last_env = Some(env);
        }
        if !options.overrides.is_empty() {
            lane.st().overrides = options.overrides.clone();
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
        let resolved = self
            .resolve_full(project, lane)
            .await
            .map_err(|e| with_ownership(e, &reference, &[]))?;
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
            profile: lane.st().profile.clone(),
            infra_enabled: true,
            whole_lane: false,
            setup_only: false,
            session: options.session.clone(),
            resolved: Some(resolved),
            deadline: Instant::now() + options.timeout,
            started: Vec::new(),
            already_running: Vec::new(),
            failures: Vec::new(),
            skipped_tasks: Vec::new(),
            infra_needs: Vec::new(),
            timed_out: false,
        };
        let result = ServicesPhase.run(self, &mut cx).await;
        // Replicas not named by the selectors stay as they are.
        cx.already_running.retain(|unit| units.contains(unit));
        drop(op);
        if let PhaseResult::Error(error) = result {
            let started: Vec<String> = cx
                .started
                .iter()
                .map(|u| format!("{reference}:{u}"))
                .collect();
            return Err(with_ownership(error, &reference, &started));
        }
        Ok(self.finish_start(cx, false).await)
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
        let stopped = self.stop_lane(project, lane, None, None).await?;
        let _op = lane.op.lock().await;
        let mut tasks = Vec::new();
        let mut teardown = Vec::new();
        let runnable = {
            let st = lane.st();
            st.present && st.env.is_some() && st.record.is_some()
        };
        if runnable {
            tasks = self.run_hook(project, lane, TaskWhen::Clean, &actor).await;
            if let Ok(resolved) = self.resolve_full(project, lane).await {
                for (name, command) in &resolved.teardown {
                    let result = self
                        .run_command(project, lane, "teardown", command, &actor)
                        .await;
                    teardown.push(match result {
                        Ok((outcome, _)) => json!({"entry": name, "exit_code": outcome.code, "ignored": command.ignore_errors}),
                        Err(error) => json!({"entry": name, "error": {"code": error.code}}),
                    });
                }
            }
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
            // Every run must be finished before the lane's records may be released.
            let runs: Vec<_> = lane
                .st()
                .units
                .values()
                .filter_map(|rec| rec.run_id.map(|id| (id, rec.host.exit_outcome())))
                .collect();
            for (id, outcome) in runs {
                let outcome = outcome.unwrap_or(ExitOutcome {
                    code: None,
                    signal: None,
                });
                let _ = self.store.finish_unit_run(id, outcome).await;
            }
            self.store
                .forget_lane_lifecycle(record.id)
                .await
                .map_err(store_error)?;
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
                profile: None,
                env,
                overrides: EnvMap::new(),
                actor: Actor::default(),
                timeout: self.opts.start_timeout,
                session: None,
                setup_only,
            },
        )
        .await
        .map(drop)
    }

    /// Daemon shutdown: stops every live unit with its policy; returns the count.
    pub(crate) async fn stop_everything(self: &Arc<Self>) -> usize {
        let mut stops = Vec::new();
        for (_, lane) in self.all_lanes() {
            let st = lane.st();
            for rec in st.units.values().filter(|rec| rec.host.is_alive()) {
                stops.push((rec.host.clone(), rec.stop));
            }
        }
        let count = stops.len();
        join_all(stops.iter().map(|(host, policy)| host.stop(*policy))).await;
        count
    }

    /// The lane's profile and the services it selects, for `why`.
    pub(crate) fn profile_services(lane: &LaneRt) -> Option<(Option<String>, BTreeSet<String>)> {
        let st = lane.st();
        let config = st.config.loaded()?;
        let selection = devlanes_resolve::select(config, st.profile.as_deref(), &[]).ok()?;
        Some((selection.profile, selection.services.into_iter().collect()))
    }
}

/// Waits for a one-shot unit, stopping it after `timeout`.
async fn finish(host: &UnitHost, timeout: Option<Duration>) -> ExitOutcome {
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
    let mut cursor: Option<String> = None;
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
