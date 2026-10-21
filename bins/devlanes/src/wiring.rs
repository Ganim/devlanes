//! Registers concrete integrations. This is the only module that names them.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use devlanes_client::Paths;
use devlanes_core::{
    ExitOutcome, LaunchSpec, OutputChunk, ProcessIdentity, StopPolicy, TerminalSize, Unit,
    contrib::{CA_BUNDLE_ENV, CA_CERT_ENV, Contributor, ScriptRunner},
};
use devlanes_daemon::{
    Engine, EngineOptions, Handler,
    integrations::{
        InfraError, InfraEvent, InfraPortNeed, InfraProvider, InfraSpec, InfraUp, Integrations,
        LaunchCheck, ManagedUnit, PlannedUnit, PortInspector, PortScope, PresetEntry, RouteEntry,
        RouteSink, RouteStatus, SysProcesses, UnitRunner, unavailable,
    },
};
use devlanes_proto::RpcError;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Terminal launchers for "open in terminal"; the TUI picks one by the user config name.
fn launchers() -> Vec<Arc<dyn devlanes_term::TerminalLauncher>> {
    vec![
        Arc::new(devlanes_term_iterm2::Iterm2),
        Arc::new(devlanes_term_ghostty::Ghostty),
        Arc::new(devlanes_term_wezterm::Wezterm),
        Arc::new(devlanes_term_kitty::Kitty),
        Arc::new(devlanes_term_terminal_app::TerminalApp),
    ]
}

/// The user's preferred terminal from `~/.devlanes/config.yaml`, if it parses.
fn preferred_terminal(paths: &Paths) -> Option<String> {
    let config = devlanes_config::load_user(paths.home.join("config.yaml")).ok()?;
    let value = serde_json::to_value(config.terminal?).ok()?;
    value.as_str().map(str::to_owned)
}

/// Opens the TUI; closing it never stops anything.
pub async fn ui(paths: &Paths) -> Result<u8, RpcError> {
    let options = devlanes_tui::Options {
        launchers: launchers(),
        terminal: preferred_terminal(paths),
    };
    devlanes_tui::run_with(paths.clone(), options)
        .await
        .map(|()| 0)
        .map_err(|error| match error {
            devlanes_tui::Error::NotATerminal => {
                RpcError::new("not_a_terminal", json!({"hint": "devlanes status"}), 2)
            }
            other => RpcError::new("tui_failed", json!({"message": other.to_string()}), 1),
        })
}

/// Builds the daemon engine with every concrete integration. Integrations that
/// cannot start (no Docker, no CA, a busy proxy port) degrade to reported errors
/// instead of failing the daemon.
pub async fn engine(
    paths: &Paths,
    shutdown: CancellationToken,
) -> anyhow::Result<Arc<dyn devlanes_daemon::Handler>> {
    match tokio::task::spawn_blocking(devlanes_sys::raise_nofile_limit).await? {
        Ok(limit) => tracing::info!(?limit, "raised the open-file limit"),
        Err(error) => tracing::warn!(%error, "cannot raise the open-file limit"),
    }
    let user = load_user_config(&paths.home).await;
    let store = devlanes_state::Store::open(&paths.db).await?;
    let (ca_env, ca_status, ca) = certificate_authority(&paths.home).await;
    let routes = Arc::new(proxy(&user, ca, shutdown.clone()).await);
    let infra = Arc::new(ContainerInfra::new().await);
    let mut integ = Integrations::with_runner(Arc::new(PtyUnits(
        devlanes_runner_pty::PtyRunner::default(),
    )));
    integ.contributors = contributors();
    integ.script_runners = script_runners();
    integ.secrets = Some(Arc::new(devlanes_secrets_fnox::FnoxProvider::default()));
    integ.infra = infra;
    integ.routes = routes;
    integ.metrics = metrics_sources();
    integ.ports = Arc::new(SysPorts);
    integ.processes = Arc::new(SysProcesses);
    integ.ca_env = ca_env;
    integ.ca_status = ca_status;
    integ.presets = presets();
    integ.launch_checks = vec![Arc::new(DistinctTauriIdentifiers)];
    let mut options = EngineOptions::new(&paths.home, &paths.socket);
    options.db_path = paths.db.clone();
    options.logs = paths.logs.clone();
    options.user = user;
    let engine: Arc<dyn Handler> = Arc::new(Engine::new(options, store, integ, shutdown));
    Ok(engine)
}

/// `~/.devlanes/config.yaml`, or defaults when absent or invalid (logged).
async fn load_user_config(home: &Path) -> devlanes_config::UserConfig {
    let path = home.join("config.yaml");
    tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return devlanes_config::UserConfig::default();
        }
        devlanes_config::load_user(&path).unwrap_or_else(|error| {
            tracing::warn!(error = %error.message, "invalid user config; using defaults");
            devlanes_config::UserConfig::default()
        })
    })
    .await
    .unwrap_or_default()
}

/// Platform, env-manager and preset contributors (priority orders them).
fn contributors() -> Vec<Arc<dyn Contributor>> {
    vec![
        Arc::new(devlanes_env_mise::MiseContributor::default()),
        Arc::new(devlanes_env_direnv::DirenvContributor::default()),
        Arc::new(devlanes_platform_node::NodeContributor),
        Arc::new(devlanes_platform_node::CloudflareContributor),
        Arc::new(devlanes_platform_python::PythonContributor),
        Arc::new(devlanes_preset_vite::VitePreset),
        Arc::new(devlanes_preset_next::NextPreset),
        Arc::new(devlanes_preset_wrangler::WranglerPreset),
        Arc::new(devlanes_preset_tauri::TauriPreset),
    ]
}

/// `run_script` sources: package.json, Makefile, justfile.
fn script_runners() -> Vec<Arc<dyn ScriptRunner>> {
    vec![
        Arc::new(devlanes_run_script_npm::NpmScripts),
        Arc::new(devlanes_run_script_make::MakeTargets),
        Arc::new(devlanes_run_script_just::JustRecipes),
    ]
}

/// Preset option schemas and parsers for `schema` and `config check`.
fn presets() -> Vec<PresetEntry> {
    vec![
        PresetEntry {
            name: devlanes_preset_vite::NAME,
            schema: devlanes_preset_vite::options_schema(),
            parse: Arc::new(|value| {
                devlanes_preset_vite::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
        PresetEntry {
            name: devlanes_preset_next::NAME,
            schema: devlanes_preset_next::options_schema(),
            parse: Arc::new(|value| {
                devlanes_preset_next::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
        PresetEntry {
            name: devlanes_preset_wrangler::NAME,
            schema: devlanes_preset_wrangler::options_schema(),
            parse: Arc::new(|value| {
                devlanes_preset_wrangler::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
        PresetEntry {
            name: devlanes_preset_tauri::NAME,
            schema: devlanes_preset_tauri::options_schema(),
            parse: Arc::new(|value| {
                devlanes_preset_tauri::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
    ]
}

/// Process collectors for this OS plus Docker container stats.
fn metrics_sources() -> Vec<Arc<dyn devlanes_metrics::MetricsSource>> {
    let mut sources: Vec<Arc<dyn devlanes_metrics::MetricsSource>> = Vec::new();
    #[cfg(target_os = "macos")]
    match devlanes_metrics_macos::MacosSource::new() {
        Ok(source) => sources.push(Arc::new(source)),
        Err(error) => tracing::warn!(%error, "process metrics unavailable"),
    }
    #[cfg(target_os = "linux")]
    sources.push(Arc::new(devlanes_metrics_linux::LinuxSource::new()));
    match devlanes_metrics_docker::DockerSource::connect() {
        Ok(source) => sources.push(Arc::new(source)),
        Err(error) => tracing::debug!(%error, "container metrics unavailable"),
    }
    sources
}

/// Loads or creates the CA under `$DEVLANES_HOME/ca` and writes the combined bundle.
/// Returns the trust env for contributors, `doctor` facts and the CA itself.
async fn certificate_authority(
    home: &Path,
) -> (
    BTreeMap<String, String>,
    Value,
    Option<Arc<devlanes_pki::CertificateAuthority>>,
) {
    let directory = home.join("ca");
    let result = tokio::task::spawn_blocking(move || {
        let ca = devlanes_pki::CertificateAuthority::load_or_create(&directory)?;
        let system = [
            "/etc/ssl/cert.pem",
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
        ]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
        let bundle = directory.join("bundle.pem");
        let temp = directory.join(".bundle.pem.tmp");
        std::fs::write(&temp, ca.combined_bundle(&system))?;
        std::fs::rename(&temp, &bundle)?;
        Ok::<_, devlanes_pki::Error>((ca, directory.join("ca.pem"), bundle))
    })
    .await;
    match result {
        Ok(Ok((ca, cert, bundle))) => {
            let env = BTreeMap::from([
                (CA_CERT_ENV.to_owned(), cert.display().to_string()),
                (CA_BUNDLE_ENV.to_owned(), bundle.display().to_string()),
            ]);
            let status = json!({"present": true, "path": cert, "trusted": "unknown"});
            (env, status, Some(Arc::new(ca)))
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "CA unavailable; HTTPS is off");
            (
                BTreeMap::new(),
                json!({"present": false, "error": error.to_string()}),
                None,
            )
        }
        Err(error) => (
            BTreeMap::new(),
            json!({"present": false, "error": error.to_string()}),
            None,
        ),
    }
}

/// Binds the proxy (HTTP with fallback 8080, HTTPS with fallback 8443 when a CA
/// exists) and returns the routing sink. Bind failures are reported, not fatal.
async fn proxy(
    user: &devlanes_config::UserConfig,
    ca: Option<Arc<devlanes_pki::CertificateAuthority>>,
    shutdown: CancellationToken,
) -> ProxyRoutes {
    let lan = user.proxy.lan;
    let mut status = serde_json::Map::new();
    let https = match &ca {
        Some(_) => match devlanes_proxy::bind_listeners(user.proxy.https_port, 8443, lan) {
            Ok(listeners) => Some(listeners),
            Err(error) => {
                status.insert(
                    "https".into(),
                    json!({"bound": false, "error": error.to_string()}),
                );
                None
            }
        },
        None => {
            status.insert("https".into(), json!({"bound": false, "reason": "no_ca"}));
            None
        }
    };
    let config = devlanes_proxy::ProxyConfig {
        lan,
        https_redirect_port: https.as_ref().map(|l| l.port),
        ..devlanes_proxy::ProxyConfig::default()
    };
    let proxy = devlanes_proxy::Proxy::new(config, devlanes_proxy::RouteTable::default());
    if let (Some(listeners), Some(ca)) = (https, ca) {
        match proxy.tls_config(ca) {
            Ok(tls) => {
                status.insert(
                    "https".into(),
                    json!({"bound": true, "port": listeners.port, "fallback_reason": listeners.fallback_reason}),
                );
                for listener in [listeners.ipv4, listeners.ipv6] {
                    let (proxy, tls, shutdown) = (proxy.clone(), tls.clone(), shutdown.clone());
                    tokio::spawn(async move {
                        if let Err(error) = proxy.serve_tls(listener, tls, shutdown).await {
                            tracing::warn!(%error, "https listener stopped");
                        }
                    });
                }
            }
            Err(error) => {
                status.insert(
                    "https".into(),
                    json!({"bound": false, "error": error.to_string()}),
                );
            }
        }
    }
    match devlanes_proxy::bind_listeners(user.proxy.http_port, 8080, lan) {
        Ok(listeners) => {
            status.insert(
                "http".into(),
                json!({"bound": true, "port": listeners.port, "fallback_reason": listeners.fallback_reason}),
            );
            for listener in [listeners.ipv4, listeners.ipv6] {
                let (proxy, shutdown) = (proxy.clone(), shutdown.clone());
                tokio::spawn(async move {
                    if let Err(error) = proxy.serve(listener, shutdown).await {
                        tracing::warn!(%error, "http listener stopped");
                    }
                });
            }
        }
        Err(error) => {
            status.insert(
                "http".into(),
                json!({"bound": false, "error": error.to_string()}),
            );
        }
    }
    ProxyRoutes {
        proxy,
        status: Value::Object(status),
    }
}

/// Publishes engine routes to the proxy's atomic table.
struct ProxyRoutes {
    proxy: devlanes_proxy::Proxy,
    status: Value,
}

impl RouteSink for ProxyRoutes {
    /// Converts and swaps the table; an invalid snapshot keeps the previous one.
    fn publish(&self, routes: Vec<RouteEntry>) {
        let routes = routes
            .into_iter()
            .map(|entry| {
                let mut route = devlanes_proxy::Route::new(
                    entry.host,
                    devlanes_proxy::Upstream::Local(entry.port),
                );
                route.fallback = entry.fallback;
                route.host_header = if entry.host_header_upstream {
                    devlanes_proxy::HostHeader::Upstream
                } else {
                    devlanes_proxy::HostHeader::Preserve
                };
                route.state = match entry.status {
                    RouteStatus::Ready => devlanes_proxy::RouteState::Ready,
                    RouteStatus::Idle => devlanes_proxy::RouteState::Idle,
                    RouteStatus::Starting => devlanes_proxy::RouteState::Starting,
                    RouteStatus::Failed { log_tail } => {
                        devlanes_proxy::RouteState::Failed { log_tail }
                    }
                };
                route.start_hint = entry.start_hint;
                route.certificate_names = entry.certificate_names;
                route
            })
            .collect();
        match devlanes_proxy::RouteTable::new(routes) {
            Ok(table) => self.proxy.replace_routes(table),
            Err(error) => tracing::warn!(%error, "route snapshot rejected"),
        }
    }

    /// Listener facts.
    fn status(&self) -> Value {
        self.status.clone()
    }
}

/// The PTY runner with identity and socket inspection.
struct PtyUnits(devlanes_runner_pty::PtyRunner);

#[async_trait]
impl UnitRunner for PtyUnits {
    /// Spawns in a PTY or pipes per the spec.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        Ok(Arc::new(PtyManaged(self.0.spawn_unit(spec).await?)))
    }
}

/// [`devlanes_runner_pty::PtyUnit`] as a daemon unit.
struct PtyManaged(Arc<devlanes_runner_pty::PtyUnit>);

#[async_trait]
impl Unit for PtyManaged {
    /// Delegates.
    async fn input(&self, bytes: bytes::Bytes) -> Result<(), devlanes_core::Error> {
        self.0.input(bytes).await
    }
    /// Delegates.
    async fn resize(&self, size: TerminalSize) -> Result<(), devlanes_core::Error> {
        self.0.resize(size).await
    }
    /// Delegates.
    async fn stop(&self, policy: StopPolicy) -> Result<(), devlanes_core::Error> {
        self.0.stop(policy).await
    }
    /// Delegates.
    async fn wait(&self) -> Result<ExitOutcome, devlanes_core::Error> {
        self.0.wait().await
    }
    /// Delegates; the first subscriber sees everything since launch.
    fn output(&self) -> broadcast::Receiver<OutputChunk> {
        self.0.output()
    }
    /// Delegates.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, devlanes_core::Error> {
        self.0.subjects().await
    }
}

#[async_trait]
impl ManagedUnit for PtyManaged {
    /// Leader identity recorded at spawn.
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.0.identity())
    }
    /// Listening TCP ports of the tracked tree.
    async fn listening_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self
            .0
            .listening_ports()
            .await
            .unwrap_or_default()
            .iter()
            .map(|socket| socket.addr.port())
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }
}

/// Finds a port's holder from the process table (diagnostics only).
struct SysPorts;

#[async_trait]
impl PortInspector for SysPorts {
    /// Scans listening sockets of all visible processes.
    async fn holder(&self, port: u16) -> Option<u32> {
        tokio::task::spawn_blocking(move || {
            let pids: Vec<u32> = devlanes_sys::processes()
                .ok()?
                .iter()
                .map(|p| p.pid)
                .collect();
            devlanes_sys::listening_tcp(&pids)
                .ok()?
                .into_iter()
                .find(|socket| socket.addr.port() == port)
                .map(|socket| socket.pid)
        })
        .await
        .ok()
        .flatten()
    }
}

/// Refuses two tauri instances (any lane or replica) sharing a bundle identifier.
struct DistinctTauriIdentifiers;

impl LaunchCheck for DistinctTauriIdentifiers {
    /// Reads `identifier` from each tauri overlay file.
    fn check(&self, starting: &[PlannedUnit], running: &[PlannedUnit]) -> Result<(), String> {
        let identifiers: Vec<(String, String)> = starting
            .iter()
            .chain(running)
            .filter(|unit| unit.preset.as_deref() == Some(devlanes_preset_tauri::NAME))
            .filter_map(|unit| {
                let file = unit.files.first()?;
                let overlay: Value = serde_json::from_slice(&file.contents).ok()?;
                Some((
                    unit.label.clone(),
                    overlay.get("identifier")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        devlanes_preset_tauri::ensure_distinct(
            identifiers.iter().map(|(l, i)| (l.as_str(), i.as_str())),
        )
        .map_err(|error| error.to_string())
    }
}

/// Compose (shared and per-lane services) plus plain Docker containers.
struct ContainerInfra {
    compose: devlanes_infra_compose::Compose,
    docker: Option<devlanes_infra_docker::DockerInfra>,
    events: broadcast::Sender<InfraEvent>,
    /// Projects whose container events are already forwarded.
    watched: Mutex<BTreeSet<String>>,
}

/// Compose failures; a missing `docker` binary means Docker is unavailable.
fn compose_error(error: devlanes_infra_compose::Error) -> InfraError {
    match &error {
        devlanes_infra_compose::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            unavailable()
        }
        devlanes_infra_compose::Error::UnsupportedVersion { .. } => {
            InfraError::new("compose_unsupported", error.to_string())
        }
        devlanes_infra_compose::Error::ContainerNameConflict(_) => {
            InfraError::new("container_name_conflict", error.to_string())
        }
        _ => InfraError::new("compose_failed", error.to_string()),
    }
}

/// Docker API failures; connection problems mean Docker is unavailable.
fn docker_error(error: devlanes_infra_docker::Error) -> InfraError {
    let text = error.to_string();
    if matches!(error, devlanes_infra_docker::Error::Io(_))
        || text.contains("connect")
        || text.contains("No such file")
    {
        return InfraError::new("docker_unavailable", text);
    }
    InfraError::new("docker_failed", text)
}

impl ContainerInfra {
    /// Connects lazily; a missing engine only affects projects that declare infra.
    async fn new() -> Self {
        let docker = match devlanes_infra_docker::DockerInfra::connect().await {
            Ok(docker) => Some(docker),
            Err(error) => {
                tracing::info!(%error, "docker unavailable");
                None
            }
        };
        Self {
            compose: devlanes_infra_compose::Compose::default(),
            docker,
            events: broadcast::channel(256).0,
            watched: Mutex::new(BTreeSet::new()),
        }
    }

    /// The Docker client or `docker_unavailable`.
    fn docker(&self) -> Result<&devlanes_infra_docker::DockerInfra, InfraError> {
        self.docker.as_ref().ok_or_else(unavailable)
    }

    /// Shared compose project description.
    fn shared(spec: &InfraSpec) -> devlanes_infra_compose::SharedInfra {
        devlanes_infra_compose::SharedInfra {
            project_name: spec.project_name.clone(),
            primary_dir: spec.primary_dir.clone(),
            files: spec.compose_files.clone(),
            env_file: Some(PathBuf::from(".env")),
            profiles: spec.profiles.clone(),
            exclude: spec.exclude.iter().cloned().collect(),
            per_lane: spec.per_lane.iter().cloned().collect(),
            env: spec.env.clone(),
        }
    }

    /// Per-lane compose project description.
    fn lane(spec: &InfraSpec) -> devlanes_infra_compose::LaneInfra {
        devlanes_infra_compose::LaneInfra {
            project: spec.project.clone(),
            lane: spec.lane,
            lane_dir: spec.lane_dir.clone(),
            files: spec.compose_files.clone(),
            env_file: Some(PathBuf::from(".env")),
            profiles: spec.profiles.clone(),
            exclude: spec.exclude.iter().cloned().collect(),
            per_lane: spec.per_lane.iter().cloned().collect(),
            env: spec.env.clone(),
            state_dir: spec.state_dir.clone(),
        }
    }

    /// Whether the spec has per-lane compose services.
    fn has_lane_services(spec: &InfraSpec) -> bool {
        !spec.compose_files.is_empty() && spec.per_lane.iter().any(|s| !spec.exclude.contains(s))
    }

    /// Plain container specs with host ports from the spec.
    fn containers(
        spec: &InfraSpec,
    ) -> Result<Vec<devlanes_infra_docker::ContainerSpec>, InfraError> {
        let mut specs = Vec::new();
        for container in &spec.containers {
            let scope = if container.per_lane {
                devlanes_infra_docker::Scope::Lane(spec.lane)
            } else {
                devlanes_infra_docker::Scope::Shared
            };
            let (first, size) = if container.per_lane {
                spec.block
            } else {
                spec.shared_block.unwrap_or(spec.block)
            };
            let block = devlanes_infra_docker::PortBlock::new(first, size).map_err(docker_error)?;
            let mut ports = Vec::new();
            for (name, target) in &container.ports {
                let key = format!("infra.{}/{name}", container.name);
                let host = spec.ports.get(&key).copied().ok_or_else(|| {
                    InfraError::new("port_missing", format!("no host port allocated for {key}"))
                })?;
                ports.push(devlanes_infra_docker::NamedPort {
                    name: name.clone(),
                    container: *target,
                    host,
                    protocol: devlanes_infra_docker::Protocol::Tcp,
                });
            }
            specs.push(devlanes_infra_docker::ContainerSpec {
                project: spec.project.clone(),
                name: container.name.clone(),
                image: container.image.clone(),
                command: container.command.clone(),
                env: container.env.clone(),
                volumes: container.volumes.clone(),
                ports,
                scope,
                base_dir: if container.per_lane {
                    spec.lane_dir.clone()
                } else {
                    spec.primary_dir.clone()
                },
                block,
            });
        }
        Ok(specs)
    }

    /// Namespace entries for compose endpoints.
    fn compose_namespace(
        namespace: &mut serde_json::Map<String, Value>,
        endpoints: &devlanes_infra_compose::Endpoints,
    ) {
        for (service, endpoint) in endpoints {
            let ports: serde_json::Map<String, Value> = endpoint
                .ports
                .iter()
                .map(|(target, host)| (target.to_string(), json!(host)))
                .collect();
            namespace.insert(
                service.clone(),
                json!({"port": endpoint.port, "ports": ports}),
            );
        }
    }

    /// Namespace entries for plain containers from their allocated ports.
    fn container_namespace(
        namespace: &mut serde_json::Map<String, Value>,
        containers: &[devlanes_infra_docker::ContainerSpec],
    ) {
        for container in containers {
            let ports: serde_json::Map<String, Value> = container
                .ports
                .iter()
                .map(|port| (port.name.clone(), json!(port.host)))
                .collect();
            let first = container.ports.first().map(|port| port.host);
            namespace.insert(
                container.name.clone(),
                json!({"port": first, "ports": ports}),
            );
        }
    }

    /// Setup env for `infra.setup` entries.
    async fn setup_env(&self, spec: &InfraSpec) -> BTreeMap<String, String> {
        if spec.compose_files.is_empty() {
            return BTreeMap::new();
        }
        let lane_project = Self::has_lane_services(spec)
            .then(|| devlanes_infra_compose::lane_project_name(&spec.project, spec.lane).ok())
            .flatten();
        match self.compose.shared_project(&Self::shared(spec)).await {
            Ok((project, _)) => {
                devlanes_infra_compose::setup_env(&project, lane_project.as_deref())
                    .unwrap_or_default()
            }
            Err(_) => BTreeMap::new(),
        }
    }

    /// Forwards a project's container events once.
    fn watch_events(&self, project: &str) {
        let Some(docker) = &self.docker else { return };
        let fresh = self
            .watched
            .lock()
            .is_ok_and(|mut watched| watched.insert(project.to_owned()));
        if !fresh {
            return;
        }
        let mut stream = docker.project_events(project);
        let sender = self.events.clone();
        let project = project.to_owned();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(event) = stream.next().await {
                let Ok(event) = event else { break };
                let lane = match event.scope() {
                    Some(devlanes_infra_docker::Scope::Lane(lane)) => Some(lane),
                    _ => None,
                };
                let state = serde_json::to_value(&event.kind)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                let _ = sender.send(InfraEvent {
                    project: project.clone(),
                    lane,
                    container: event.container_name.clone(),
                    state,
                });
            }
        });
    }
}

#[async_trait]
impl InfraProvider for ContainerInfra {
    /// Per-lane compose container ports and plain container ports.
    async fn port_needs(&self, spec: &InfraSpec) -> Result<Vec<InfraPortNeed>, InfraError> {
        let mut needs = Vec::new();
        if Self::has_lane_services(spec) {
            let requests = self
                .compose
                .lane_port_requests(&Self::lane(spec))
                .await
                .map_err(compose_error)?;
            for request in requests {
                let name = if request.protocol == "tcp" {
                    request.target.to_string()
                } else {
                    format!("{}/{}", request.target, request.protocol)
                };
                needs.push(InfraPortNeed {
                    scope: PortScope::Lane,
                    unit: format!("infra.{}", request.service),
                    name,
                });
            }
        }
        for container in &spec.containers {
            for (name, _) in &container.ports {
                needs.push(InfraPortNeed {
                    scope: if container.per_lane {
                        PortScope::Lane
                    } else {
                        PortScope::Shared
                    },
                    unit: format!("infra.{}", container.name),
                    name: name.clone(),
                });
            }
        }
        Ok(needs)
    }

    /// Shared compose up, per-lane compose up, then plain containers.
    async fn up(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        let mut namespace = serde_json::Map::new();
        let mut report = serde_json::Map::new();
        if !spec.compose_files.is_empty() {
            let shared = self
                .compose
                .shared_up(
                    &Self::shared(spec),
                    devlanes_infra_compose::UpOptions::default(),
                )
                .await
                .map_err(compose_error)?;
            Self::compose_namespace(&mut namespace, &shared.endpoints);
            report.insert("shared".into(), json!(shared));
            self.watch_events(&shared.project);
        }
        if Self::has_lane_services(spec) {
            let lane = Self::lane(spec);
            let mut ports: devlanes_infra_compose::LanePorts = BTreeMap::new();
            for request in self
                .compose
                .lane_port_requests(&lane)
                .await
                .map_err(compose_error)?
            {
                let key = format!("infra.{}/{}", request.service, request.target);
                if let Some(host) = spec.ports.get(&key) {
                    ports
                        .entry(request.service.clone())
                        .or_default()
                        .insert(request.target, *host);
                }
            }
            let block = devlanes_infra_compose::PortBlock::new(spec.block.0, spec.block.1)
                .map_err(compose_error)?;
            let lane_report = self
                .compose
                .lane_up(
                    &lane,
                    &ports,
                    block,
                    devlanes_infra_compose::UpOptions::default(),
                )
                .await
                .map_err(compose_error)?;
            Self::compose_namespace(&mut namespace, &lane_report.endpoints);
            report.insert("lane".into(), json!(lane_report));
        }
        let containers = Self::containers(spec)?;
        if !containers.is_empty() {
            let docker = self.docker()?;
            let mut ensured = Vec::new();
            for container in &containers {
                ensured.push(
                    docker
                        .ensure(container, devlanes_infra_docker::EnsureOptions::default())
                        .await
                        .map_err(docker_error)?,
                );
            }
            Self::container_namespace(&mut namespace, &containers);
            report.insert("containers".into(), json!(ensured));
            self.watch_events(&spec.project);
        }
        Ok(InfraUp {
            namespace: Value::Object(namespace),
            setup_env: self.setup_env(spec).await,
            report: Value::Object(report),
        })
    }

    /// Endpoints of what already runs, without starting anything.
    async fn namespace(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        let mut namespace = serde_json::Map::new();
        if !spec.compose_files.is_empty()
            && let Ok(shared) = self.compose.shared_status(&Self::shared(spec)).await
        {
            Self::compose_namespace(&mut namespace, &shared.endpoints);
        }
        if Self::has_lane_services(spec)
            && let Ok(lane) = self.compose.lane_status(&Self::lane(spec)).await
        {
            Self::compose_namespace(&mut namespace, &lane.endpoints);
        }
        if let Ok(containers) = Self::containers(spec) {
            Self::container_namespace(&mut namespace, &containers);
        }
        Ok(InfraUp {
            namespace: Value::Object(namespace),
            setup_env: self.setup_env(spec).await,
            report: Value::Null,
        })
    }

    /// Health and endpoints of shared, per-lane and plain containers.
    async fn status(&self, spec: &InfraSpec) -> Result<Value, InfraError> {
        let mut report = serde_json::Map::new();
        if !spec.compose_files.is_empty() {
            let shared = self
                .compose
                .shared_status(&Self::shared(spec))
                .await
                .map_err(compose_error)?;
            report.insert("shared".into(), json!(shared));
        }
        if Self::has_lane_services(spec) {
            let lane = self
                .compose
                .lane_status(&Self::lane(spec))
                .await
                .map_err(compose_error)?;
            report.insert("lane".into(), json!(lane));
        }
        if !spec.containers.is_empty() {
            let managed = self
                .docker()?
                .list_managed(&spec.project)
                .await
                .map_err(docker_error)?;
            report.insert("containers".into(), json!(managed));
        }
        Ok(Value::Object(report))
    }

    /// Stops shared compose services and shared plain containers; data stays.
    async fn down(&self, spec: &InfraSpec) -> Result<Value, InfraError> {
        let mut report = serde_json::Map::new();
        if !spec.compose_files.is_empty() {
            let (project, _) = self
                .compose
                .shared_project(&Self::shared(spec))
                .await
                .map_err(compose_error)?;
            let name = project.name.unwrap_or_default();
            self.compose
                .shared_stop(&name, Duration::from_secs(30))
                .await
                .map_err(compose_error)?;
            report.insert("compose".into(), json!(name));
        }
        if let Some(docker) = &self.docker {
            let stopped = docker
                .stop(
                    &spec.project,
                    Some(devlanes_infra_docker::Scope::Shared),
                    Duration::from_secs(30),
                )
                .await
                .map_err(docker_error)?;
            report.insert("containers".into(), json!(stopped));
        }
        Ok(Value::Object(report))
    }

    /// Removes the lane's compose project (with volumes) and plain containers.
    async fn remove_lane(&self, project: &str, lane: u32) -> Result<Value, InfraError> {
        let mut report = serde_json::Map::new();
        match self
            .compose
            .lane_down(project, lane, Duration::from_secs(30))
            .await
        {
            Ok(()) => {
                report.insert(
                    "compose".into(),
                    json!(devlanes_infra_compose::lane_project_name(project, lane).ok()),
                );
            }
            Err(error) => {
                let error = compose_error(error);
                if error.code != "docker_unavailable" {
                    report.insert("compose_error".into(), json!(error.message));
                }
            }
        }
        if let Some(docker) = &self.docker {
            match docker.remove_lane(project, lane).await {
                Ok(removed) => {
                    report.insert("containers".into(), json!(removed));
                }
                Err(error) => {
                    report.insert("docker_error".into(), json!(error.to_string()));
                }
            }
        }
        Ok(Value::Object(report))
    }

    /// `StartedAt` of a plain container or a shared/per-lane compose service.
    async fn container_started_at(
        &self,
        spec: &InfraSpec,
        container: &str,
    ) -> Result<Option<String>, InfraError> {
        let Some(docker) = &self.docker else {
            return Ok(None);
        };
        let name = if let Some(declared) = spec.containers.iter().find(|c| c.name == container) {
            let scope = if declared.per_lane {
                devlanes_infra_docker::Scope::Lane(spec.lane)
            } else {
                devlanes_infra_docker::Scope::Shared
            };
            devlanes_infra_docker::container_name(&spec.project, container, scope)
        } else {
            let project = if spec.per_lane.iter().any(|s| s == container) {
                devlanes_infra_compose::lane_project_name(&spec.project, spec.lane)
                    .map_err(compose_error)?
            } else {
                let (project, _) = self
                    .compose
                    .shared_project(&Self::shared(spec))
                    .await
                    .map_err(compose_error)?;
                project.name.unwrap_or_default()
            };
            let statuses = self
                .compose
                .cli()
                .ps(&project)
                .await
                .map_err(compose_error)?;
            match statuses
                .into_iter()
                .find(|status| status.service == container)
            {
                Some(status) => status.id,
                None => return Ok(None),
            }
        };
        match docker.client().inspect_container(&name, None).await {
            Ok(inspect) => Ok(inspect.state.and_then(|state| state.started_at)),
            Err(_) => Ok(None),
        }
    }

    /// Lists labelled containers so status can show them; never stops anything.
    async fn adopt(&self, projects: &[String]) -> Result<Value, InfraError> {
        let docker = self.docker()?;
        let mut adopted = serde_json::Map::new();
        for project in projects {
            let managed = docker.list_managed(project).await.map_err(docker_error)?;
            if !managed.is_empty() {
                self.watch_events(project);
            }
            adopted.insert(
                project.clone(),
                json!(managed.iter().map(|m| m.name.clone()).collect::<Vec<_>>()),
            );
        }
        Ok(Value::Object(adopted))
    }

    /// Engine reachability and Compose version.
    async fn doctor(&self) -> Value {
        let docker = match &self.docker {
            Some(docker) => match docker.server_version().await {
                Ok(version) => json!({"reachable": true, "version": version}),
                Err(error) => json!({"reachable": false, "error": error.to_string()}),
            },
            None => json!({"reachable": false, "reason": "docker_unavailable"}),
        };
        let compose = match self.compose.version().await {
            Ok(version) => json!({
                "version": version.to_string(),
                "supports_lane_overrides": version.require_override_tags().is_ok(),
            }),
            Err(error) => json!({"error": compose_error(error).code}),
        };
        json!({"docker": docker, "compose": compose})
    }

    /// Container lifecycle events of watched projects.
    fn events(&self) -> Option<broadcast::Receiver<InfraEvent>> {
        Some(self.events.subscribe())
    }
}
