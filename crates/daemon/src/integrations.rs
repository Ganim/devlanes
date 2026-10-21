//! Daemon-side contracts for injected integrations. The composition root implements
//! them over concrete crates (PTY runner, Compose/Docker, the proxy, metrics
//! collectors); tests implement them with in-memory fakes.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    ExitOutcome, LaunchSpec, OutputChunk, ProcessIdentity, StopPolicy, TerminalSize, Unit,
    contrib::{Contributor, ScriptRunner, SecretsProvider},
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// A spawned process that also reports its identity and listening ports.
#[async_trait]
pub trait ManagedUnit: Unit {
    /// Leader identity recorded in `unit_runs`; `None` when the runner cannot tell.
    fn identity(&self) -> Option<ProcessIdentity>;
    /// TCP ports the unit's process tree listens on (diagnostics only).
    async fn listening_ports(&self) -> Vec<u16>;
}

/// Spawns daemon-owned units.
#[async_trait]
pub trait UnitRunner: Send + Sync {
    /// Starts `spec` in its own session. The returned unit's first `output()`
    /// subscriber must receive everything since launch.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error>;
}

/// Adapts any core [`devlanes_core::Runner`]; identity comes from the first subject.
pub struct CoreRunner<R>(pub R);

#[async_trait]
impl<R: devlanes_core::Runner> UnitRunner for CoreRunner<R> {
    /// Wraps the spawned unit and captures its leader identity once.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        let unit = self.0.spawn(spec).await?;
        let identity = unit
            .subjects()
            .await
            .ok()
            .and_then(|s| s.into_iter().next());
        Ok(Arc::new(CoreUnit { unit, identity }))
    }
}

/// A core unit plus the identity captured right after spawn.
struct CoreUnit {
    unit: Arc<dyn Unit>,
    identity: Option<ProcessIdentity>,
}

#[async_trait]
impl Unit for CoreUnit {
    /// Delegates to the wrapped unit.
    async fn input(&self, bytes: Bytes) -> Result<(), devlanes_core::Error> {
        self.unit.input(bytes).await
    }
    /// Delegates to the wrapped unit.
    async fn resize(&self, size: TerminalSize) -> Result<(), devlanes_core::Error> {
        self.unit.resize(size).await
    }
    /// Delegates to the wrapped unit.
    async fn stop(&self, policy: StopPolicy) -> Result<(), devlanes_core::Error> {
        self.unit.stop(policy).await
    }
    /// Delegates to the wrapped unit.
    async fn wait(&self) -> Result<ExitOutcome, devlanes_core::Error> {
        self.unit.wait().await
    }
    /// Delegates to the wrapped unit.
    fn output(&self) -> broadcast::Receiver<OutputChunk> {
        self.unit.output()
    }
    /// Delegates to the wrapped unit.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, devlanes_core::Error> {
        self.unit.subjects().await
    }
}

#[async_trait]
impl ManagedUnit for CoreUnit {
    /// Identity captured at spawn.
    fn identity(&self) -> Option<ProcessIdentity> {
        self.identity
    }
    /// Core units expose no socket inspection.
    async fn listening_ports(&self) -> Vec<u16> {
        Vec::new()
    }
}

/// Infra failures carry a machine code (`docker_unavailable`, `compose_failed`, ...).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct InfraError {
    pub code: String,
    pub message: String,
}

impl InfraError {
    /// Builds an error with a stable code and a diagnostic message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// A plain container from `infra.containers`, after expression evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerDecl {
    pub name: String,
    pub image: String,
    pub command: Option<Vec<String>>,
    pub volumes: Vec<String>,
    /// Port name to container port, in declaration order.
    pub ports: Vec<(String, u16)>,
    pub per_lane: bool,
    pub env: BTreeMap<String, String>,
}

/// Everything an infra provider needs for one lane, with expressions evaluated.
/// `infra.env` values may come from `.env` files; never log them.
#[derive(Debug, Clone)]
pub struct InfraSpec {
    pub project: String,
    pub lane: u32,
    /// Directory of `devlanes.yaml` in the lane's worktree.
    pub lane_dir: PathBuf,
    /// Directory of `devlanes.yaml` in the primary worktree.
    pub primary_dir: PathBuf,
    pub compose_files: Vec<PathBuf>,
    pub project_name: Option<String>,
    pub exclude: Vec<String>,
    pub per_lane: Vec<String>,
    pub profiles: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub containers: Vec<ContainerDecl>,
    /// The lane's port block, `(first, size)`.
    pub block: (u16, u16),
    /// The project's shared block for shared plain containers, if allocated.
    pub shared_block: Option<(u16, u16)>,
    /// Host ports allocated for [`InfraPortNeed`]s, keyed by `<unit>/<name>`.
    pub ports: BTreeMap<String, u16>,
    /// devlanes-owned directory for generated files such as compose overrides.
    pub state_dir: PathBuf,
}

impl InfraSpec {
    /// True when the config declares no infra at all.
    pub fn is_empty(&self) -> bool {
        self.compose_files.is_empty() && self.containers.is_empty()
    }
}

/// Which block a requested infra port comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortScope {
    Lane,
    Shared,
}

/// A host port an infra provider needs; `unit`/`name` become a stable offset key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfraPortNeed {
    pub scope: PortScope,
    pub unit: String,
    pub name: String,
}

impl InfraPortNeed {
    /// Key used in [`InfraSpec::ports`].
    pub fn key(&self) -> String {
        format!("{}/{}", self.unit, self.name)
    }
}

/// Result of bringing a lane's infra up.
#[derive(Debug, Clone, Default)]
pub struct InfraUp {
    /// The `infra` expression namespace: `{name: {port, ports: {...}}}`.
    pub namespace: Value,
    /// `COMPOSE_PROJECT_NAME`, `COMPOSE_FILE`, `DEVLANES_COMPOSE_LANE_PROJECT`.
    pub setup_env: BTreeMap<String, String>,
    /// Structured report for clients (services, health, adopted containers).
    pub report: Value,
}

/// A container lifecycle change for the event bus.
#[derive(Debug, Clone, Serialize)]
pub struct InfraEvent {
    pub project: String,
    pub lane: Option<u32>,
    pub container: String,
    pub state: String,
}

/// Containers: shared and per-lane Compose services plus plain Docker containers.
/// Implementations never stop containers implicitly; only the explicit calls do.
#[async_trait]
pub trait InfraProvider: Send + Sync {
    /// Host ports the lane's infra needs from the lane or project block.
    async fn port_needs(&self, spec: &InfraSpec) -> Result<Vec<InfraPortNeed>, InfraError>;
    /// Starts shared and per-lane infra and waits until healthy.
    async fn up(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError>;
    /// The `infra` namespace and setup env without starting anything.
    async fn namespace(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError>;
    /// Health and endpoints of a project's infra.
    async fn status(&self, spec: &InfraSpec) -> Result<Value, InfraError>;
    /// Stops the project's shared containers, keeping their data (`infra down`).
    async fn down(&self, spec: &InfraSpec) -> Result<Value, InfraError>;
    /// Removes the lane's own containers and volumes (`lane clean`).
    async fn remove_lane(&self, project: &str, lane: u32) -> Result<Value, InfraError>;
    /// `StartedAt` of the container backing `container` (a compose service or plain
    /// container name) as seen from this lane; `None` when it does not run.
    async fn container_started_at(
        &self,
        spec: &InfraSpec,
        container: &str,
    ) -> Result<Option<String>, InfraError>;
    /// Lists labelled containers after a daemon restart. Never stops anything.
    async fn adopt(&self, projects: &[String]) -> Result<Value, InfraError>;
    /// Docker reachability and Compose version for `doctor`.
    async fn doctor(&self) -> Value;
    /// Container events; `None` when the provider has no event source.
    fn events(&self) -> Option<broadcast::Receiver<InfraEvent>> {
        None
    }
}

/// Provider used when no Docker integration is registered: every call reports
/// `docker_unavailable` and lanes without infra are unaffected.
pub struct NoInfra;

#[async_trait]
impl InfraProvider for NoInfra {
    /// No ports without an engine.
    async fn port_needs(&self, _: &InfraSpec) -> Result<Vec<InfraPortNeed>, InfraError> {
        Ok(Vec::new())
    }
    /// Fails for projects that declare infra.
    async fn up(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        if spec.is_empty() {
            return Ok(InfraUp::default());
        }
        Err(unavailable())
    }
    /// Empty namespace.
    async fn namespace(&self, _: &InfraSpec) -> Result<InfraUp, InfraError> {
        Ok(InfraUp::default())
    }
    /// Reports the missing engine.
    async fn status(&self, _: &InfraSpec) -> Result<Value, InfraError> {
        Err(unavailable())
    }
    /// Reports the missing engine.
    async fn down(&self, _: &InfraSpec) -> Result<Value, InfraError> {
        Err(unavailable())
    }
    /// Nothing to remove without an engine.
    async fn remove_lane(&self, _: &str, _: u32) -> Result<Value, InfraError> {
        Ok(Value::Null)
    }
    /// No containers.
    async fn container_started_at(
        &self,
        _: &InfraSpec,
        _: &str,
    ) -> Result<Option<String>, InfraError> {
        Ok(None)
    }
    /// No containers.
    async fn adopt(&self, _: &[String]) -> Result<Value, InfraError> {
        Ok(Value::Null)
    }
    /// Reports the missing engine.
    async fn doctor(&self) -> Value {
        serde_json::json!({"docker": {"reachable": false, "reason": "docker_unavailable"}})
    }
}

/// Error used by [`NoInfra`] and by adapters whose engine is unreachable.
pub fn unavailable() -> InfraError {
    InfraError::new("docker_unavailable", "no Docker engine is reachable")
}

/// What the proxy should show for a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RouteStatus {
    Ready,
    Idle,
    Starting,
    Failed { log_tail: String },
}

/// One hostname route, independent of the proxy crate's types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteEntry {
    pub host: String,
    pub port: u16,
    pub fallback: bool,
    pub host_header_upstream: bool,
    pub status: RouteStatus,
    /// Command shown on idle and failed pages, e.g. `devlanes start --lane acme@2`.
    pub start_hint: String,
    pub project: String,
    pub lane: String,
    pub unit: String,
    /// Extra certificate names, normally the lane apex domain.
    pub certificate_names: Vec<String>,
}

/// Receives complete routing snapshots.
pub trait RouteSink: Send + Sync {
    /// Replaces every route at once.
    fn publish(&self, routes: Vec<RouteEntry>);
    /// Listener ports, fallbacks and TLS state for `doctor` and `hosts`.
    fn status(&self) -> Value {
        Value::Null
    }
}

/// Sink that drops routes, for tests and proxy-less setups.
pub struct NoRoutes;

impl RouteSink for NoRoutes {
    /// Discards the snapshot.
    fn publish(&self, _: Vec<RouteEntry>) {}
}

/// Finds which process holds a TCP port (diagnostics only).
#[async_trait]
pub trait PortInspector: Send + Sync {
    /// PID listening on `port`, if any can be identified.
    async fn holder(&self, port: u16) -> Option<u32>;
}

/// Inspector that never identifies a holder.
pub struct NoPortInspector;

#[async_trait]
impl PortInspector for NoPortInspector {
    /// Always unknown.
    async fn holder(&self, _: u16) -> Option<u32> {
        None
    }
}

/// Identity-checked process control for reconciliation after a daemon crash.
#[async_trait]
pub trait ProcessTable: Send + Sync {
    /// True when the PID still belongs to the recorded process (same start time).
    async fn alive(&self, identity: ProcessIdentity) -> bool;
    /// Terminates the recorded leader and its session members, escalating to
    /// SIGKILL after `grace`. Only processes whose identity matches are signalled.
    async fn terminate(&self, identity: ProcessIdentity, grace: Duration);
}

/// [`ProcessTable`] over `devlanes-sys`; blocking reads run off the executor.
pub struct SysProcesses;

#[async_trait]
impl ProcessTable for SysProcesses {
    /// Compares PID and start time.
    async fn alive(&self, identity: ProcessIdentity) -> bool {
        tokio::task::spawn_blocking(move || devlanes_sys::matches(&identity).unwrap_or(false))
            .await
            .unwrap_or(false)
    }

    /// SIGTERM to the verified session, then SIGKILL to verified survivors.
    async fn terminate(&self, identity: ProcessIdentity, grace: Duration) {
        let signal_all = move |signal: devlanes_sys::Signal| {
            let snapshot = devlanes_sys::processes().unwrap_or_default();
            let mut targets = devlanes_sys::session_members(&snapshot, &identity);
            targets.retain(|p| p.pid != identity.pid);
            let mut alive = 0;
            if matches!(
                devlanes_sys::signal_verified(&identity, signal),
                Ok(devlanes_sys::SignalOutcome::Sent)
            ) {
                alive += 1;
            }
            for member in targets {
                if matches!(
                    devlanes_sys::signal_verified(&member.identity(), signal),
                    Ok(devlanes_sys::SignalOutcome::Sent)
                ) {
                    alive += 1;
                }
            }
            alive
        };
        let sent = tokio::task::spawn_blocking(move || signal_all(devlanes_sys::Signal::TERM))
            .await
            .unwrap_or(0);
        if sent == 0 {
            return;
        }
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline {
            if !self.alive(identity).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = tokio::task::spawn_blocking(move || signal_all(devlanes_sys::Signal::KILL)).await;
    }
}

/// Validates a preset's opaque option block; `None` when the service has none.
pub type PresetParser = dyn Fn(Option<&Value>) -> Result<(), String> + Send + Sync;

/// A registered preset: its config key, option schema and option parser.
#[derive(Clone)]
pub struct PresetEntry {
    pub name: &'static str,
    pub schema: schemars::Schema,
    pub parse: Arc<PresetParser>,
}

/// A unit about to start or already running, for cross-lane preflight checks.
#[derive(Clone)]
pub struct PlannedUnit {
    /// Display label, `project@lane:unit`.
    pub label: String,
    pub preset: Option<String>,
    /// Files the unit's contributors prepared (paths and contents, no secrets).
    pub files: Vec<devlanes_core::contrib::PreparedFile>,
}

/// Checks a project's starting units against its running ones before any spawn
/// (for example distinct per-lane app identifiers).
pub trait LaunchCheck: Send + Sync {
    /// Returns a message when `starting` conflicts with `running` or itself.
    fn check(&self, starting: &[PlannedUnit], running: &[PlannedUnit]) -> Result<(), String>;
}

/// Every injected integration. Constructed by the composition root.
#[derive(Clone)]
pub struct Integrations {
    pub runner: Arc<dyn UnitRunner>,
    pub contributors: Vec<Arc<dyn Contributor>>,
    pub script_runners: Vec<Arc<dyn ScriptRunner>>,
    /// Wrapped by the daemon's per-lane cache, which `unlock` can fill.
    pub secrets: Option<Arc<dyn SecretsProvider>>,
    pub infra: Arc<dyn InfraProvider>,
    pub routes: Arc<dyn RouteSink>,
    pub metrics: Vec<Arc<dyn devlanes_metrics::MetricsSource>>,
    pub ports: Arc<dyn PortInspector>,
    pub processes: Arc<dyn ProcessTable>,
    /// CA trust variables keyed by `devlanes_core::contrib::CA_CERT_ENV` and
    /// `CA_BUNDLE_ENV`; empty when no CA is available.
    pub ca_env: BTreeMap<String, String>,
    /// CA facts for `doctor` (`present`, `path`).
    pub ca_status: Value,
    /// Preset option schemas and parsers for `schema` and `config check`.
    pub presets: Vec<PresetEntry>,
    pub launch_checks: Vec<Arc<dyn LaunchCheck>>,
}

impl Integrations {
    /// Integrations with only a runner; everything else is a no-op.
    pub fn with_runner(runner: Arc<dyn UnitRunner>) -> Self {
        Self {
            runner,
            contributors: Vec::new(),
            script_runners: Vec::new(),
            secrets: None,
            infra: Arc::new(NoInfra),
            routes: Arc::new(NoRoutes),
            metrics: Vec::new(),
            ports: Arc::new(NoPortInspector),
            processes: Arc::new(SysProcesses),
            ca_env: BTreeMap::new(),
            ca_status: Value::Null,
            presets: Vec::new(),
            launch_checks: Vec::new(),
        }
    }
}
