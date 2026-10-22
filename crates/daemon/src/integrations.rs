//! Daemon-side contracts for injected integrations. The composition root implements
//! them over concrete crates (PTY runner, Compose/Docker, the proxy, metrics
//! collectors); tests implement them with in-memory fakes.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    ExitOutcome, LaunchSpec, OutputChunk, ProcessIdentity, StopPolicy, TerminalSize, Unit,
    contrib::{Contributor, ScriptRunner, SecretsProvider},
    shell::Shell,
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
    /// How the unit ended, once it has; `None` while it runs or when the runner
    /// keeps no more than the [`ExitOutcome`] that [`Unit::wait`] returns.
    fn exit_details(&self) -> Option<ExitDetails> {
        None
    }
}

/// A finished unit's status plus the processes it left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitDetails {
    pub outcome: ExitOutcome,
    /// Daemons a task started outside its session, kept running on purpose
    /// ([`UnitRunner::spawn_task`]).
    pub left_running: Vec<devlanes_proto::LeftProcess>,
    /// Processes that survived the final sweep or still hold the unit's terminal
    /// or output pipe.
    pub escaped: Vec<devlanes_proto::LeftProcess>,
}

/// Spawns daemon-owned units.
#[async_trait]
pub trait UnitRunner: Send + Sync {
    /// Starts `spec` in its own session. The returned unit's first `output()`
    /// subscriber must receive everything since launch.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error>;

    /// Starts a task. Unlike services, tasks may legitimately start daemons
    /// (`pg_ctl start`, `redis-server --daemonize yes`), so runners that sweep
    /// leftovers after a natural exit should spare descendants that left the task's
    /// session. Defaults to [`UnitRunner::spawn`].
    async fn spawn_task(
        &self,
        spec: LaunchSpec,
    ) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        self.spawn(spec).await
    }
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

/// Lines of a failed command's stderr kept in errors; the end explains most failures.
pub const STDERR_TAIL_LINES: usize = 40;

/// Infra failures carry a machine code (`docker_unavailable`, `compose_failed`, ...)
/// and, when a command or API call failed, what ran, how it ended and its stderr.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct InfraError {
    pub code: String,
    pub message: String,
    /// Full command line (`docker compose -p acme up ...`) or API call description.
    pub command: Option<String>,
    /// Exit status or signal (`exit status: 1`), HTTP status, or health state.
    pub status: Option<String>,
    /// Last [`STDERR_TAIL_LINES`] lines of stderr (or the engine's message), ANSI stripped.
    pub stderr: Option<String>,
}

impl InfraError {
    /// Builds an error with a stable code and a diagnostic message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            command: None,
            status: None,
            stderr: None,
        }
    }

    /// Adds what failed. `stderr` is ANSI-stripped and cut to its last lines here, so
    /// every adapter reports the same shape.
    pub fn with_command(
        mut self,
        command: impl Into<String>,
        status: Option<String>,
        stderr: Option<&str>,
    ) -> Self {
        self.command = Some(command.into());
        self.status = status;
        self.stderr = stderr.map(stderr_tail);
        self
    }

    /// Error data for clients: `{message, command, status, stderr}`; unknown fields are null.
    pub fn data(&self) -> Value {
        serde_json::json!({
            "message": self.message,
            "command": self.command,
            "status": self.status,
            "stderr": self.stderr,
        })
    }
}

impl From<InfraError> for devlanes_proto::RpcError {
    /// Keeps the infra code; exit code 1.
    fn from(error: InfraError) -> Self {
        let data = error.data();
        devlanes_proto::RpcError::new(error.code, data, 1)
    }
}

/// ANSI-stripped last [`STDERR_TAIL_LINES`] lines of `text`.
fn stderr_tail(text: &str) -> String {
    let plain = strip_ansi_escapes::strip_str(text);
    let lines: Vec<&str> = plain.trim_end().lines().collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines[start..].join("\n")
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
    /// Checked by the daemon after `up`; the infra phase waits for it.
    #[serde(skip)]
    pub ready: Option<devlanes_resolve::ContainerReady>,
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
    /// Running containers behind the namespace entries, sampled for Docker stats.
    pub containers: Vec<InfraContainer>,
}

/// One running container behind an `infra` namespace entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfraContainer {
    /// The namespace key: compose service or `infra.containers` name.
    pub name: String,
    /// Container name or id the Docker API accepts.
    pub docker: String,
    /// Shared project infra rather than the lane's own container.
    pub shared: bool,
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
    /// The last `tail` log lines of the plain container `container`, for `ready: {log}`.
    async fn container_logs(
        &self,
        _spec: &InfraSpec,
        _container: &str,
        _tail: usize,
    ) -> Result<Vec<String>, InfraError> {
        Err(InfraError::new(
            "not_supported_yet",
            "this infra provider cannot read container logs",
        ))
    }
    /// Lists labelled containers after a daemon restart. Never stops anything.
    async fn adopt(&self, projects: &[String]) -> Result<Value, InfraError>;
    /// Docker reachability and Compose version for `doctor`.
    async fn doctor(&self) -> Value;
    /// Container events; `None` when the provider has no event source.
    fn events(&self) -> Option<broadcast::Receiver<InfraEvent>> {
        None
    }
    /// Each container `up` brings up for `spec`, as it is now. Polled while `up`
    /// runs, alongside it, for start progress; empty when the provider cannot tell.
    async fn progress(&self, _spec: &InfraSpec) -> Vec<devlanes_proto::PhaseContainer> {
        Vec::new()
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
    Failed {
        log_tail: String,
    },
    /// A mirrored route whose box connection is down and retrying.
    Reconnecting,
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
    /// Port the HTTPS listener bound (443, its fallback or `proxy.https_port`),
    /// read from [`Self::status`]'s `https: {bound, port}`; public URLs carry it.
    fn https_port(&self) -> Option<u16> {
        let status = self.status();
        let https = status.get("https")?;
        if https.get("bound").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        https
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
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
    /// SIGKILL after `grace`. Only processes whose identity matches are signalled,
    /// and the session is swept only while the leader itself still verifies.
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
    #[cfg(unix)]
    async fn terminate(&self, identity: ProcessIdentity, grace: Duration) {
        let signal_all = move |signal: devlanes_sys::Signal| {
            let snapshot = devlanes_sys::processes().unwrap_or_default();
            // Empty unless the leader itself still verifies: a dead leader's session
            // ID may name someone else's session, so then only `identity` is tried.
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

    /// Terminates the recorded process if its identity still verifies, and the
    /// descendants it has now: a crashed daemon's job objects cannot be reopened,
    /// and children started after the last recording (or after the crash) were
    /// never recorded, so they are found by parent in a fresh snapshot, the way
    /// Unix sweeps a session. Each recorded member arrives here on its own (see
    /// `track_escaped`). This is the hard part only: the composition root wraps
    /// it with a phase that asks first (Ctrl-Break, `WM_CLOSE`) and honours
    /// `grace`, since asking needs the runner's console-control helper.
    #[cfg(windows)]
    async fn terminate(&self, identity: ProcessIdentity, _grace: Duration) {
        let ended = tokio::task::spawn_blocking(move || {
            let snapshot = devlanes_sys::processes()?;
            // Descendants are only adopted under a root that still verifies.
            let mut tree = devlanes_sys::DescendantTracker::new(identity);
            tree.update(&snapshot);
            for process in tree.live() {
                let member = process.identity();
                if let Err(error) = devlanes_sys::windows::proc::terminate_verified(&member, 1) {
                    tracing::warn!(%error, pid = member.pid, "terminating an orphan failed");
                }
            }
            Ok::<_, devlanes_sys::Error>(())
        })
        .await;
        match ended {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, pid = identity.pid, "terminating an orphan failed")
            }
            Err(error) => {
                tracing::warn!(%error, pid = identity.pid, "terminating an orphan failed")
            }
        }
    }
}

/// Validates a preset's opaque option block; `None` when the service has none.
pub type PresetParser = dyn Fn(Option<&Value>) -> Result<(), String> + Send + Sync;

/// A registered preset: its config key and option parser. The option schemas are
/// generated into the config crate's schema files at build time.
#[derive(Clone)]
pub struct PresetEntry {
    pub name: &'static str,
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
    /// DNS names the CA's name constraints permit (subtrees such as `localhost`);
    /// `None` when it has none (mkcert) or there is no CA. Routed hosts outside
    /// them get a warning, since browsers reject their certificates.
    pub ca_permitted: Option<Vec<String>>,
    /// Preset option parsers that `config check` uses.
    pub presets: Vec<PresetEntry>,
    pub launch_checks: Vec<Arc<dyn LaunchCheck>>,
    /// Transports to boxes for `connect`.
    pub remote: Arc<dyn RemoteConnector>,
    /// The `/etc/hosts` fallback; `None` leaves the hosts file alone.
    pub hosts: Option<Arc<crate::hosts::HostsFile>>,
    /// The machine-wide start gate; `None` spawns every service at once.
    pub start_gate: Option<Arc<dyn devlanes_core::gate::StartGate>>,
    /// System pressure, for the upfront memory warning.
    pub pressure: Option<Arc<dyn devlanes_core::gate::PressureSource>>,
    /// Checks the OS resolver file for the `resolver` hostname strategy.
    pub resolver_files: Option<Arc<dyn ResolverFiles>>,
    /// Runs `run:` strings and `ready.cmd` probes. Defaults to
    /// [`Shell::default`]; the composition root sets [`detect_shell`]'s result.
    pub shell: Shell,
}

/// Finds the shell for `run:` strings once, at daemon startup. Unix always uses
/// `/bin/sh` and looks nothing up.
#[cfg(unix)]
pub async fn detect_shell() -> Shell {
    Shell::posix()
}

/// Finds the shell for `run:` strings once, at daemon startup: Git for Windows'
/// `sh.exe`, else `cmd.exe`, which `config check` and launch notes warn about.
/// It runs before the daemon answers, so the usual case starts no process: a
/// `git.exe` on the daemon's `PATH` in a Git for Windows layout gives the
/// install root. Only when that finds nothing does it ask `git --exec-path`,
/// for at most [`EXEC_PATH_TIMEOUT`].
#[cfg(windows)]
pub async fn detect_shell() -> Shell {
    let path = std::env::var_os("PATH");
    let on_path = tokio::task::spawn_blocking(move || git_sh_on_path(path.as_deref()))
        .await
        .ok()
        .flatten();
    let found = match on_path {
        Some(sh) => Some(sh),
        None => git_sh_from_exec_path().await,
    };
    if let Some(sh) = found {
        tracing::info!(shell = %sh.display(), "run strings use Git for Windows' sh");
        return Shell::git_sh(&sh);
    }
    let fallback = Shell::default();
    tracing::warn!(
        shell = %fallback.program,
        "Git for Windows' sh.exe was not found; run strings fall back to cmd.exe"
    );
    fallback
}

/// Upper bound for `git --exec-path`, well below the client's wait for a
/// starting daemon, which also covers this detection.
#[cfg(windows)]
const EXEC_PATH_TIMEOUT: Duration = Duration::from_secs(2);

/// The first `sh.exe` of a Git for Windows install whose `git.exe` is in a
/// directory of `path`, in `PATH` order. Checks files only; blocking.
#[cfg(windows)]
fn git_sh_on_path(path: Option<&std::ffi::OsStr>) -> Option<std::path::PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git.exe"))
        .filter(|git| git.is_file())
        .filter_map(|git| devlanes_core::shell::git_root_from_exe(&git))
        .find_map(|root| first_file(&root))
}

/// The first `sh.exe` candidate under a Git for Windows `root` that exists.
#[cfg(windows)]
fn first_file(root: &std::path::Path) -> Option<std::path::PathBuf> {
    devlanes_core::shell::git_sh_candidates(root)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// `sh.exe` from the install root `git --exec-path` reports, for a `git` the
/// `PATH` scan did not recognise (a shim, a custom layout).
#[cfg(windows)]
async fn git_sh_from_exec_path() -> Option<std::path::PathBuf> {
    use devlanes_sys::NoWindow;
    use std::process::Stdio;
    let output = tokio::process::Command::new("git")
        .no_window()
        .arg("--exec-path")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let exec_path = match tokio::time::timeout(EXEC_PATH_TIMEOUT, output).await {
        // Git for Windows prints `C:/Program Files/Git/mingw64/libexec/git-core`.
        Ok(Ok(output)) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim()
            .replace('/', "\\"),
        Ok(Ok(output)) => {
            tracing::debug!(status = %output.status, "git --exec-path failed");
            return None;
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "cannot run git --exec-path");
            return None;
        }
        Err(_) => {
            tracing::debug!("git --exec-path timed out");
            return None;
        }
    };
    let Some(root) =
        devlanes_core::shell::git_root_from_exec_path(std::path::Path::new(&exec_path))
    else {
        tracing::debug!(exec_path, "git --exec-path is not a Windows path");
        return None;
    };
    tokio::task::spawn_blocking(move || first_file(&root))
        .await
        .ok()
        .flatten()
}

/// Reports whether the OS forwards the devlanes TLD to the embedded DNS server.
/// OS-specific file locations live in the implementation, not the engine.
pub trait ResolverFiles: Send + Sync {
    /// `{path, installed, matches}` for the file that routes `tld` to `server`.
    /// Called on a blocking thread.
    fn status(&self, tld: &str, server: std::net::SocketAddr) -> Value;
}

/// What ssh (or another transport) asks the user while connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePrompt {
    pub host: String,
    pub prompt: String,
    /// `secret`, `confirm` or `notify`, as the CLI renders them.
    pub kind: &'static str,
}

/// Relays connection prompts to the CLI attached to `connect`.
#[async_trait]
pub trait RemotePrompter: Send + Sync {
    /// The answer, or `None` when nobody is attached or the user cancelled.
    async fn ask(&self, prompt: RemotePrompt) -> Option<String>;
}

/// What `remote connect` asked for beyond the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Install the client's own release on a box that lacks devlanes
    /// (`--install`); without it a missing binary is only reported.
    pub install: bool,
}

/// Builds transports to boxes; the composition root wires SSH, tests an in-memory pipe.
pub trait RemoteConnector: Send + Sync {
    /// A transport to `host`; prompts during authentication go to `prompts`.
    fn transport(
        &self,
        host: &str,
        prompts: Arc<dyn RemotePrompter>,
        options: ConnectOptions,
    ) -> Result<Arc<dyn devlanes_transport::Transport>, devlanes_proto::RpcError>;

    /// Transport name recorded in `remote_hosts`.
    fn kind(&self) -> &'static str {
        "ssh"
    }

    /// Loopback addresses that mirrored ports bind; each port is bound on all of them.
    fn mirror_addresses(&self) -> Vec<std::net::IpAddr> {
        vec![
            std::net::Ipv4Addr::LOCALHOST.into(),
            std::net::Ipv6Addr::LOCALHOST.into(),
        ]
    }

    /// Reconnect delays.
    fn backoff(&self) -> devlanes_transport::Backoff {
        devlanes_transport::Backoff::default()
    }
}

/// Connector used when no transport is registered: `connect` is unsupported.
pub struct NoRemote;

impl RemoteConnector for NoRemote {
    /// Refuses with `not_supported_yet`.
    fn transport(
        &self,
        _: &str,
        _: Arc<dyn RemotePrompter>,
        _: ConnectOptions,
    ) -> Result<Arc<dyn devlanes_transport::Transport>, devlanes_proto::RpcError> {
        Err(devlanes_proto::RpcError::new(
            "not_supported_yet",
            serde_json::json!({"feature": "remote transport"}),
            1,
        ))
    }
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
            ca_permitted: None,
            presets: Vec::new(),
            launch_checks: Vec::new(),
            remote: Arc::new(NoRemote),
            hosts: None,
            start_gate: None,
            pressure: None,
            resolver_files: None,
            shell: Shell::default(),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Creates an empty file and its directories.
    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("directories");
        }
        std::fs::write(path, b"").expect("file");
    }

    #[test]
    /// The `PATH` scan finds `sh.exe` from a `git.exe` in a Git for Windows
    /// layout, preferring the `bin` launcher, and skips other `git.exe` files.
    fn path_scan_finds_git_sh_from_the_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("Git");
        let shim = dir.path().join("shims");
        touch(&shim.join("git.exe"));
        touch(&root.join("cmd").join("git.exe"));
        touch(&root.join("usr").join("bin").join("sh.exe"));
        let path = std::env::join_paths([shim.clone(), root.join("cmd")]).expect("PATH");
        assert_eq!(
            git_sh_on_path(Some(path.as_os_str())),
            Some(root.join("usr").join("bin").join("sh.exe"))
        );
        touch(&root.join("bin").join("sh.exe"));
        assert_eq!(
            git_sh_on_path(Some(path.as_os_str())),
            Some(root.join("bin").join("sh.exe"))
        );
        let only_shim = std::env::join_paths([shim]).expect("PATH");
        assert_eq!(git_sh_on_path(Some(only_shim.as_os_str())), None);
        assert_eq!(git_sh_on_path(None), None);
    }
}
