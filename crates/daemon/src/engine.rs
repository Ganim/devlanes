//! The engine: one [`Handler`] that owns projects, lanes, units and the event bus.
//! Concrete integrations arrive through [`Integrations`]; this module only
//! dispatches methods and resolves lane and unit references.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_config::UserConfig;
use devlanes_core::{
    LaneId, ProjectId,
    refs::{LaneCandidate, LaneRef, ReferenceError, ResolveContext, UnitRef},
};
use devlanes_proto::{Context, Method, Request, RpcError};
use devlanes_state::Store;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    Handler, Session, Shutdown, ShutdownReason,
    bus::EventBus,
    integrations::Integrations,
    model::{LaneRt, ProjectRt, lock},
    secrets::SecretsCache,
};

/// Paths and policy the engine needs; supplied by the composition root.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub home: PathBuf,
    pub socket: PathBuf,
    pub db_path: PathBuf,
    pub logs: PathBuf,
    pub user: UserConfig,
    /// Watch worktrees and config files; tests that only need RPCs may turn it off.
    pub watch: bool,
    /// Default wait for `start` when the selection has no timeout.
    pub start_timeout: Duration,
    /// Where the embedded DNS server listens under the `resolver` hostname
    /// strategy; port 0 picks an ephemeral port (tests).
    pub dns_listen: std::net::SocketAddr,
    /// Problems of the user config at load, each `{message, file, line, column}`;
    /// the keys they name use their defaults in `user`, every other key its
    /// value. `daemon status` shows them; `doctor` checks the file again.
    pub user_config_errors: Vec<Value>,
    /// Release checks (see `update`); never downloads or installs anything.
    pub update_check: crate::update::UpdateCheckOptions,
}

impl EngineOptions {
    /// Options rooted at `home` with the documented layout and defaults.
    pub fn new(home: impl Into<PathBuf>, socket: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            db_path: home.join("state.db"),
            logs: home.join("logs"),
            socket: socket.into(),
            home,
            user: UserConfig::default(),
            watch: true,
            start_timeout: Duration::from_secs(180),
            dns_listen: devlanes_resolver_dns::default_listen(),
            user_config_errors: Vec::new(),
            update_check: crate::update::UpdateCheckOptions::from_env(),
        }
    }
}

/// Shared engine state. Methods live in sibling modules as `impl Inner` blocks.
pub(crate) struct Inner {
    pub opts: EngineOptions,
    pub store: Store,
    pub integ: Integrations,
    pub bus: EventBus,
    pub secrets: Arc<SecretsCache>,
    pub projects: Mutex<BTreeMap<ProjectId, Arc<ProjectRt>>>,
    pub shutdown: CancellationToken,
    pub started: Instant,
    pub background: AtomicBool,
    pub attach: Mutex<crate::attach::AttachTable>,
    /// One PTY resize at a time, so a closing stream and a reopening one
    /// cannot apply their sizes out of order.
    pub attach_resize: tokio::sync::Mutex<()>,
    pub metrics: Mutex<crate::sampler::MetricsState>,
    pub next_stream: AtomicU64,
    /// Next `start_id` for `lane.phase` events and start results.
    pub next_start: AtomicU64,
    pub remote: crate::remote::Remotes,
    pub pacing: crate::pacing::Pacing,
    /// Embedded DNS server state for `doctor` (`null` without the resolver strategy).
    pub dns: Mutex<Value>,
    /// The latest state.db retention pass, for `doctor`.
    pub last_prune: Mutex<Option<crate::maintenance::LastPrune>>,
    /// The latest release the background check read.
    pub update: crate::update::UpdateState,
    /// Coalesces route snapshots for the proxy and `/etc/hosts`.
    pub route_publisher: crate::routes::RoutePublisher,
}

/// Cheap handle implementing [`Handler`].
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

impl Engine {
    /// Builds an engine. Background work (watchers, sampler, retention) starts in
    /// [`Handler::reconcile`], which the server calls before accepting clients.
    pub fn new(
        opts: EngineOptions,
        store: Store,
        integ: Integrations,
        shutdown: CancellationToken,
    ) -> Self {
        let secrets = Arc::new(SecretsCache::new(integ.secrets.clone()));
        Self {
            inner: Arc::new(Inner {
                opts,
                store,
                integ,
                bus: EventBus::default(),
                secrets,
                projects: Mutex::new(BTreeMap::new()),
                shutdown,
                started: Instant::now(),
                background: AtomicBool::new(false),
                attach: Mutex::new(crate::attach::AttachTable::default()),
                attach_resize: tokio::sync::Mutex::new(()),
                metrics: Mutex::new(crate::sampler::MetricsState::default()),
                next_stream: AtomicU64::new(1),
                next_start: AtomicU64::new(1),
                remote: crate::remote::Remotes::default(),
                pacing: crate::pacing::Pacing::default(),
                dns: Mutex::new(Value::Null),
                last_prune: Mutex::new(None),
                update: crate::update::UpdateState::default(),
                route_publisher: crate::routes::RoutePublisher::default(),
            }),
        }
    }
}

#[async_trait]
impl Handler for Engine {
    /// Counts live units in every lane.
    fn managed_processes(&self) -> usize {
        self.inner
            .all_lanes()
            .iter()
            .map(|(_, lane)| {
                lane.st()
                    .units
                    .values()
                    .filter(|unit| unit.host.is_alive())
                    .count()
            })
            .sum()
    }

    /// Reloads known projects, records a previous daemon's units whose identity
    /// still verifies as orphaned (left running, never killed here) and starts
    /// background work.
    async fn reconcile(&self) -> Result<(), RpcError> {
        self.inner.reconcile().await
    }

    /// Canonicalizes the cwd, then dispatches.
    async fn handle(&self, mut request: Request, session: Session) -> Result<Value, RpcError> {
        request.context.cwd = canonical(&request.context.cwd).await;
        self.inner.dispatch(request, session).await
    }

    /// Routes bytes to the unit only for a stream this session opened.
    async fn input(&self, stream_id: u64, data: Bytes, session: Session) -> Result<(), RpcError> {
        if self.inner.remote_input(stream_id, &data, &session).await {
            return Ok(());
        }
        self.inner.input(stream_id, data, &session).await
    }

    /// Stops every unit with its stop policy; containers keep running. An
    /// explicit shutdown first runs the `lane.stop` tasks of each lane with live
    /// units. Records the exits, then closes state.db so its WAL is checkpointed
    /// before exit. Returns `{reason, stopped: [{lane, unit}], tasks: [{lane,
    /// task, exit_code | error}]}`.
    async fn shutdown(&self, request: Shutdown) -> Result<Value, RpcError> {
        // Usually the server's token already fired; an embedder may call this
        // directly. Either way, new mutations and spawns are refused from here on.
        self.inner.shutdown.cancel();
        self.inner.remote_shutdown().await;
        let hooks = (request.reason == ShutdownReason::Explicit).then_some(&request.interrupt);
        let (stopped, tasks) = self.inner.stop_everything(hooks).await;
        tracing::info!(
            reason = request.reason.as_str(),
            stopped = stopped.len(),
            tasks = tasks.len(),
            "daemon shutdown stopped units"
        );
        self.inner.finish_stopped_runs().await;
        if let Err(error) = self.inner.store.close().await {
            tracing::warn!(%error, "state.db did not close cleanly");
        }
        Ok(json!({
            "reason": request.reason.as_str(),
            "stopped": stopped,
            "tasks": tasks,
        }))
    }
}

/// Resolves symlinks (macOS `/tmp`) so cwd matches the paths git reports.
async fn canonical(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::canonicalize(&owned).unwrap_or(owned))
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Decodes method parameters; `null` means defaults.
pub(crate) fn params<T: DeserializeOwned + Default>(value: Value) -> Result<T, RpcError> {
    if value.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(value)
        .map_err(|error| RpcError::new("bad_request", json!({"message": error.to_string()}), 2))
}

/// Decodes required parameters without a default.
pub(crate) fn required<T: DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value)
        .map_err(|error| RpcError::new("bad_request", json!({"message": error.to_string()}), 2))
}

/// Converts reference failures into `not_found`/`ambiguous`/`invalid_reference`.
pub(crate) fn reference_error(error: ReferenceError) -> RpcError {
    match error {
        ReferenceError::Invalid(reference) => {
            RpcError::new("invalid_reference", json!({"reference": reference}), 2)
        }
        ReferenceError::NotFound {
            reference,
            candidates,
        } => RpcError::new(
            "not_found",
            json!({"reference": reference, "candidates": candidates}),
            3,
        ),
        ReferenceError::Ambiguous {
            reference,
            candidates,
        } => RpcError::new(
            "ambiguous",
            json!({"reference": reference, "candidates": candidates}),
            3,
        ),
    }
}

/// Methods that change lanes, units, infra or state; refused once shutdown began.
fn mutating(method: &Method) -> bool {
    matches!(
        method,
        Method::DaemonResetState
            | Method::Start
            | Method::Stop
            | Method::Restart
            | Method::Resume
            | Method::LaneNew
            | Method::LaneClean
            | Method::LaneGc
            | Method::Exec
            | Method::Run
            | Method::InfraUp
            | Method::InfraDown
            | Method::Unlock
            | Method::Connect
            | Method::Disconnect
            | Method::HostsRetry
    )
}

/// The daemon is shutting down: it stops everything itself and takes no new work.
pub(crate) fn daemon_stopping() -> RpcError {
    RpcError::new(
        "daemon_stopping",
        json!({"message": "the daemon is shutting down; retry once it is gone"}),
        4,
    )
}

/// Error for operations that need `--yes`.
pub(crate) fn confirmation(data: Value) -> RpcError {
    RpcError::new("confirmation_required", data, 2)
}

/// A resolved lane with its project.
pub(crate) type LanePair = (Arc<ProjectRt>, Arc<LaneRt>);

impl Inner {
    /// Every lane of every project.
    pub(crate) fn all_lanes(&self) -> Vec<LanePair> {
        let projects: Vec<_> = lock(&self.projects).values().cloned().collect();
        projects
            .into_iter()
            .flat_map(|project| {
                project
                    .lanes()
                    .into_iter()
                    .map(move |lane| (project.clone(), lane))
            })
            .collect()
    }

    /// Every project.
    pub(crate) fn all_projects(&self) -> Vec<Arc<ProjectRt>> {
        lock(&self.projects).values().cloned().collect()
    }

    /// Finds a project by id.
    pub(crate) fn project(&self, id: ProjectId) -> Option<Arc<ProjectRt>> {
        lock(&self.projects).get(&id).cloned()
    }

    /// Routes one request to its method implementation.
    async fn dispatch(
        self: &Arc<Self>,
        request: Request,
        session: Session,
    ) -> Result<Value, RpcError> {
        let Request {
            method,
            params,
            context,
            ..
        } = request;
        if self.shutdown.is_cancelled() && mutating(&method) {
            return Err(daemon_stopping());
        }
        match method {
            Method::DaemonStatus => Ok(self.with_update(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "uptime_ms": self.started.elapsed().as_millis(),
                "release_check": self.release_check(),
                // As loaded at startup, so `daemon restart` shows what it applied.
                "user_config": {
                    "path": self.opts.home.join("config.yaml"),
                    "errors": self.opts.user_config_errors,
                    "warnings": self.opts.user.warnings(),
                },
            }))),
            Method::DaemonShutdown => Ok(json!({"stopping": true})),
            Method::DaemonResetState => self.reset_state(&context).await,
            Method::Doctor => self.doctor().await,
            Method::ConfigCheck => self.config_check(&context).await,
            Method::Start => self.start_method(params, &context, &session).await,
            Method::Stop => self.stop_method(params, &context).await,
            Method::Restart => self.restart_method(params, &context, &session).await,
            Method::Resume => self.resume_method(params, &context, &session).await,
            Method::Status => self
                .remote_status(params, &context, &session)
                .await
                .map(|result| self.with_update(result)),
            Method::LaneList => self.lane_list(&context).await,
            Method::LaneNew => self.lane_new(params, &context, &session).await,
            Method::LaneClean => self.lane_clean_method(params, &context).await,
            Method::LaneGc => self.lane_gc(&context).await,
            Method::Exec => self.exec_method(params, &context).await,
            Method::Run => self.run_method(params, &context, &session).await,
            Method::TaskList => self.task_list(&context).await,
            Method::ProfileList => self.profile_list(&context).await,
            Method::Logs => self.logs_method(params, &context, &session).await,
            Method::Wait => self.wait_method(params, &context, &session).await,
            Method::Attach => self.attach_method(params, &context, &session).await,
            Method::Input => Err(RpcError::new(
                "bad_request",
                json!({"message": "input travels as binary frames on an attach stream"}),
                2,
            )),
            Method::Resize => self.resize_method(params, &session).await,
            Method::InfraUp => self.infra_up_method(&context).await,
            Method::InfraDown => self.infra_down_method(&context).await,
            Method::InfraStatus => self.infra_status_method(&context).await,
            Method::Hosts => Ok(self.hosts()),
            Method::Env => self.env_method(params, &context).await,
            Method::Errors => self.errors_method(params, &context).await,
            Method::Why => self.why_method(params, &context).await,
            Method::Url => self.url_method(params, &context).await,
            Method::Metrics => self.metrics_method(params, &context).await,
            Method::Subscribe => self.remote_subscribe(params, &session).await,
            Method::SyncStatus => self.sync_status_method().await,
            Method::Connect => self.connect_method(params, &session).await,
            Method::Disconnect => self.disconnect_method(params).await,
            Method::HostList => self.host_list().await,
            Method::Unlock => self.unlock_method(params, &context).await,
            Method::HostsRetry => self.hosts_retry().await,
            // A newer client's method: answer instead of dropping the connection.
            Method::Unknown => Err(RpcError::new("unknown_method", json!({}), 2)),
        }
    }

    /// Lane candidates for reference resolution (assigned lanes only).
    pub(crate) fn candidates(&self) -> (Vec<LaneCandidate>, BTreeMap<LaneId, LanePair>) {
        let mut candidates = Vec::new();
        let mut map = BTreeMap::new();
        for (project, lane) in self.all_lanes() {
            let name = project.name();
            let st = lane.st();
            let Some(record) = &st.record else { continue };
            // An extra lane of a folder answers to its index and its own name;
            // the branch and the folder keep naming the folder's first lane.
            let candidate = match st.alias() {
                Some((_, alias)) => LaneCandidate {
                    id: record.id,
                    project_id: project.id,
                    project: name,
                    index: record.index,
                    branch: None,
                    name: alias.to_owned(),
                    path: PathBuf::new(),
                },
                None => LaneCandidate {
                    id: record.id,
                    project_id: project.id,
                    project: name,
                    index: record.index,
                    branch: st.branch.clone(),
                    name: record.name.clone(),
                    path: st.path.clone(),
                },
            };
            candidates.push(candidate);
            drop(st);
            if let Some(candidate) = candidates.last() {
                map.insert(candidate.id, (project.clone(), lane.clone()));
            }
        }
        (candidates, map)
    }

    /// Resolves `[project@]lane` relative to the request (cwd project first).
    pub(crate) async fn resolve_lane_ref(
        self: &Arc<Self>,
        reference: &str,
        context: &Context,
    ) -> Result<LanePair, RpcError> {
        let parsed: LaneRef = reference.parse().map_err(reference_error)?;
        let scope = match (&parsed.project, &context.project) {
            (None, Some(name)) => self.project_by_name(name)?.map(|p| p.id),
            (None, None) => self
                .project_for_cwd(&context.cwd)
                .await
                .ok()
                .flatten()
                .map(|p| p.id),
            _ => None,
        };
        let (candidates, map) = self.candidates();
        let resolve = ResolveContext {
            project: scope,
            lane: None,
            cwd: context.cwd.clone(),
            home: context
                .env
                .as_ref()
                .and_then(|env| env.get("HOME"))
                .map(PathBuf::from),
        };
        let found = match devlanes_core::refs::resolve_lane(&parsed, &candidates, &resolve) {
            Ok(found) => found,
            Err(error) => {
                // `project@<dir>` names a worktree without a lane (as older output
                // listed them); stop and clean still reach it.
                if let Some(pair) = self.unassigned_by_dir(&parsed, scope) {
                    return Ok(pair);
                }
                return Err(self.renamed_hint(reference_error(error), &parsed));
            }
        };
        map.get(&found.id).cloned().ok_or_else(|| {
            reference_error(ReferenceError::NotFound {
                reference: reference.into(),
                candidates: Vec::new(),
            })
        })
    }

    /// The one worktree without a lane whose directory name is the reference's
    /// selector, in its project (or the cwd's).
    fn unassigned_by_dir(
        &self,
        parsed: &LaneRef,
        scope: Option<devlanes_core::ProjectId>,
    ) -> Option<LanePair> {
        let mut found = self.all_lanes().into_iter().filter(|(project, lane)| {
            let st = lane.st();
            let in_scope = match &parsed.project {
                Some(name) => project.name() == *name,
                None => scope.is_none_or(|id| project.id == id),
            };
            in_scope
                && st.record.is_none()
                && st.alias().is_none()
                && crate::model::dir_name(&st.path) == parsed.selector
        });
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// A `not_found` for `<old>@<lane>` after `project:` was renamed gains
    /// `renamed_to` and `suggestion` (the same lane under the new name).
    fn renamed_hint(&self, mut error: RpcError, parsed: &LaneRef) -> RpcError {
        let Some(old) = &parsed.project else {
            return error;
        };
        if error.code != "not_found" {
            return error;
        }
        let renamed = self
            .all_projects()
            .into_iter()
            .find(|project| project.st().former_names.contains(old));
        if let Some(project) = renamed {
            let name = project.name();
            error.data["renamed_to"] = json!(name);
            error.data["suggestion"] = json!(format!("{name}@{}", parsed.selector));
        }
        error
    }

    /// Finds a project by its config name; several projects with one name are ambiguous.
    fn project_by_name(&self, name: &str) -> Result<Option<Arc<ProjectRt>>, RpcError> {
        let found: Vec<_> = self
            .all_projects()
            .into_iter()
            .filter(|project| project.name() == name)
            .collect();
        match found.len() {
            0 => {
                let mut error = RpcError::new(
                    "not_found",
                    json!({"reference": name, "candidates": self.all_projects().iter().map(|p| p.name()).collect::<Vec<_>>()}),
                    3,
                );
                let renamed = self
                    .all_projects()
                    .into_iter()
                    .find(|project| project.st().former_names.iter().any(|n| n == name));
                if let Some(project) = renamed {
                    error.data["renamed_to"] = json!(project.name());
                    error.data["suggestion"] = json!(project.name());
                }
                Err(error)
            }
            1 => Ok(found.into_iter().next()),
            _ => Ok(None),
        }
    }

    /// The request's lane: `context.lane`, else the lane whose worktree holds the cwd.
    pub(crate) async fn target_lane(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<LanePair, RpcError> {
        if let Some(reference) = &context.lane {
            return self.resolve_lane_ref(reference, context).await;
        }
        self.cwd_lane(context).await
    }

    /// The lane containing the cwd, including unassigned and no-config lanes.
    pub(crate) async fn cwd_lane(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<LanePair, RpcError> {
        if let Some(project) = self.project_for_cwd(&context.cwd).await? {
            if let Some(lane) = Self::lane_at(&project, &context.cwd) {
                // Running devlanes in a worktree is what makes it a lane.
                self.claim_lane(&project, &lane).await;
                return Ok((project, lane));
            }
        } else if !context.cwd.as_os_str().is_empty() {
            return Err(no_config(&context.cwd).await);
        }
        let (candidates, _) = self.candidates();
        let mut names: Vec<String> = candidates
            .iter()
            .map(|c| format!("{}@{}", c.project, c.index))
            .collect();
        names.sort();
        Err(RpcError::new(
            "not_found",
            json!({"reference": context.cwd, "candidates": names}),
            3,
        ))
    }

    /// Splits `[lane:]unit` selectors and resolves their lane. All selectors that
    /// name a lane must name the same one; unqualified ones use the request's lane.
    pub(crate) async fn lane_for_units(
        self: &Arc<Self>,
        units: &[String],
        context: &Context,
    ) -> Result<(LanePair, Vec<String>), RpcError> {
        let mut lane_ref: Option<String> = None;
        let mut selectors = Vec::new();
        for unit in units {
            let parsed: UnitRef = unit.parse().map_err(reference_error)?;
            if let Some(lane) = &parsed.lane {
                let text = match &lane.project {
                    Some(project) => format!("{project}@{}", lane.selector),
                    None => lane.selector.clone(),
                };
                if lane_ref.as_ref().is_some_and(|known| *known != text) {
                    return Err(RpcError::new("mixed_lanes", json!({"units": units}), 2));
                }
                lane_ref = Some(text);
            }
            selectors.push(parsed.selector);
        }
        let by = if lane_ref.is_some() {
            "unit reference"
        } else if context.lane.is_some() {
            "--lane"
        } else {
            "working directory"
        };
        let pair = match lane_ref {
            Some(reference) => self.resolve_lane_ref(&reference, context).await?,
            None => self.target_lane(context).await?,
        };
        tracing::debug!(
            lane = %pair.1.st().reference(&pair.0.name()),
            by,
            cwd = %context.cwd.display(),
            selectors = %selectors.join(","),
            "lane resolved"
        );
        Ok((pair, selectors))
    }
}

/// `no_config` (exit 3) for a cwd with no `devlanes.yaml` up to its repository
/// root: `path` is the cwd, `searched_to` the last directory looked in.
pub(crate) async fn no_config(cwd: &Path) -> RpcError {
    let searched_to = match devlanes_git::search_config(cwd).await {
        Ok(devlanes_git::ConfigSearch::Missing { searched_to }) => Some(searched_to),
        _ => None,
    };
    RpcError::new(
        "no_config",
        json!({"path": cwd, "searched_to": searched_to}),
        3,
    )
}

/// Matches name, glob or `+tag` selectors against `(name, tags)` candidates.
/// Every selector must match something; results keep candidate order.
pub(crate) fn match_units(
    selectors: &[String],
    candidates: &[(String, Vec<String>)],
    lane: &str,
) -> Result<Vec<String>, RpcError> {
    let mut chosen = vec![false; candidates.len()];
    for selector in selectors {
        let mut any = false;
        let glob = if selector.starts_with('+') {
            None
        } else {
            Some(
                globset::Glob::new(selector)
                    .map_err(|_| {
                        RpcError::new("invalid_reference", json!({"reference": selector}), 2)
                    })?
                    .compile_matcher(),
            )
        };
        for (index, (name, tags)) in candidates.iter().enumerate() {
            let hit = match (&glob, selector.strip_prefix('+')) {
                (_, Some(tag)) => tags.iter().any(|t| t == tag),
                (Some(glob), None) => glob.is_match(name),
                (None, None) => false,
            };
            if hit {
                chosen[index] = true;
                any = true;
            }
        }
        if !any {
            let mut names: Vec<_> = candidates
                .iter()
                .map(|(name, _)| format!("{lane}:{name}"))
                .collect();
            names.sort();
            return Err(RpcError::new(
                "not_found",
                json!({"reference": selector, "candidates": names}),
                3,
            ));
        }
    }
    Ok(candidates
        .iter()
        .zip(chosen)
        .filter(|(_, chosen)| *chosen)
        .map(|((name, _), _)| name.clone())
        .collect())
}

/// Adds `lane` and `started` to an error so `devlanes with` can tear down.
pub(crate) fn with_ownership(mut error: RpcError, lane: &str, started: &[String]) -> RpcError {
    if !error.data.is_object() {
        error.data = json!({"detail": error.data});
    }
    if let Some(map) = error.data.as_object_mut() {
        map.entry("lane").or_insert_with(|| json!(lane));
        map.insert("started".into(), json!(started));
    }
    error
}

/// The `Arc<Inner>` behind an [`Engine`], for in-crate tests.
impl Engine {
    /// Subscribes to engine events (tests and in-process clients).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<devlanes_proto::Event> {
        self.inner.bus.subscribe()
    }

    /// Runs state.db retention now, as of `now_ms` (Unix milliseconds), with the
    /// policy from the user config. The maintenance loop does this hourly.
    pub async fn prune_state_at(
        &self,
        now_ms: i64,
    ) -> Result<devlanes_state::PruneReport, devlanes_state::Error> {
        self.inner.prune_state(now_ms).await
    }
}

/// Requests counted as attempts to use the client environment (decision 7).
pub(crate) fn carries_env(context: &Context) -> Option<Arc<crate::model::EnvMap>> {
    context.env.as_ref().map(|env| Arc::new(env.clone()))
}

/// Sets the daemon-wide flag once; true for the first caller.
pub(crate) fn once(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}
