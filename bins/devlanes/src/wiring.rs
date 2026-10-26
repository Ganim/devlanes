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
use devlanes_daemon::integrations::ExitDetails;
use devlanes_daemon::integrations::{
    ConnectOptions, RemoteConnector, RemotePrompt, RemotePrompter,
};
use devlanes_daemon::{
    Engine, EngineOptions, Handler,
    integrations::{
        ContainerLogChunk, ContainerLogOptions, InfraContainer, InfraError, InfraEvent,
        InfraPortNeed, InfraProvider, InfraSpec, InfraUp, Integrations, LaunchCheck, ManagedUnit,
        PlannedUnit, PortInspector, PortScope, PresetEntry, RouteEntry, RouteSink, RouteStatus,
        SysProcesses, UnitRunner, unavailable,
    },
};
use devlanes_proto::RpcError;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// The TUI's progress bar, colors, glyphs and quit choice from the user config,
/// and the service definition the daemon page's doctor checks compare stop
/// timeouts with.
pub async fn tui_options(paths: &Paths) -> devlanes_tui::Options {
    use std::io::IsTerminal;
    // An invalid key leaves only that setting on its default, with a warning.
    let user = crate::user_config::display(paths);
    let tui = &user.tui;
    // The TUI is always human output; each feature still needs a terminal
    // that supports it.
    let features = devlanes_term::Features::detect(
        &crate::terminal::settings(paths),
        std::io::stdout().is_terminal(),
        |key| std::env::var(key).ok(),
    );
    devlanes_tui::Options {
        progress: features.progress,
        title: features.title,
        links: features.links,
        notifications: features.notifications,
        sync_output: true,
        theme: Some(tui.theme.as_str().to_owned()),
        glyphs: Some(tui.glyphs.as_str().to_owned()),
        on_quit: Some(tui.on_quit.as_str().to_owned()),
        start_default: !user.start.restore,
        service: Some(crate::daemon::service_report().await).filter(|report| !report.is_null()),
        // The process's environment, zone and directory.
        env: None,
        time_zone: None,
        cwd: None,
    }
}

/// A TUI failure as a CLI error.
pub fn tui_error(error: devlanes_tui::Error) -> RpcError {
    match error {
        devlanes_tui::Error::NotATerminal => {
            RpcError::new("not_a_terminal", json!({"hint": "devlanes status"}), 2)
        }
        other => RpcError::new("tui_failed", json!({"message": other.to_string()}), 1),
    }
}

/// Opens the TUI; closing it never stops anything.
pub async fn ui(paths: &Paths) -> Result<u8, RpcError> {
    devlanes_tui::run_with(paths.clone(), tui_options(paths).await)
        .await
        .map(|()| 0)
        .map_err(tui_error)
}

/// How long a daemon a client spawned stays up after the last TUI closed with
/// nothing running: enough for quitting and reopening the TUI, or for a
/// command typed right after, not to pay for a new daemon.
const IDLE_EXIT_GRACE: Duration = Duration::from_secs(30);

/// Builds the daemon engine with every concrete integration. Integrations that
/// cannot start (no Docker, no CA, a busy proxy port) degrade to reported errors
/// instead of failing the daemon; an invalid user config fails it. `spawned`:
/// a client started this daemon, so it may exit when idle (`daemon.exit_when_idle`).
pub async fn engine(
    paths: &Paths,
    shutdown: CancellationToken,
    spawned: bool,
) -> anyhow::Result<Arc<dyn devlanes_daemon::Handler>> {
    match tokio::task::spawn_blocking(devlanes_sys::raise_nofile_limit).await? {
        Ok(limit) => tracing::info!(?limit, "raised the open-file limit"),
        Err(error) => tracing::warn!(%error, "cannot raise the open-file limit"),
    }
    let user = load_user_config(&paths.home).await?;
    let (hosts_policy, hosts_tld) = devlanes_daemon::hosts::user_policy(&user);
    // The probe can wait on upstream DNS, so it runs while the store, CA and proxy start.
    let hosts_probe = (hosts_policy.user_mode() != devlanes_resolver_hosts::HostsFileMode::Never)
        .then(|| {
            tokio::spawn(devlanes_resolver_hosts::probe_localhost(
                HOSTS_PROBE_TIMEOUT,
            ))
        });
    let store = open_store(&paths.db).await?;
    let ca_tld = crate::local::ca_tld(&user);
    let (ca_env, ca_status, ca) =
        certificate_authority(&paths.home, user.tls.ca, ca_tld.as_deref()).await;
    let ca_permitted = ca.as_ref().and_then(|ca| ca.permitted_dns_names());
    let idle = Arc::new(devlanes_daemon::idle::IdleBoard::default());
    let routes = Arc::new(proxy(&user, ca, idle.clone(), shutdown.clone()).await);
    let infra = Arc::new(ContainerInfra::new().await);
    #[cfg(unix)]
    let mut integ = Integrations::with_runner(Arc::new(PtyUnits(
        devlanes_runner_pty::PtyRunner::default(),
    )));
    #[cfg(windows)]
    let shell = windows_shell().await;
    #[cfg(windows)]
    let mut integ = Integrations::with_runner(Arc::new(WindowsUnits(
        devlanes_runner_windows::WindowsRunner::default().with_shell(shell.clone()),
    )));
    #[cfg(windows)]
    {
        integ.shell = shell;
    }
    integ.contributors = contributors();
    integ.script_runners = script_runners();
    integ.metrics = metrics_sources(infra.docker.as_ref());
    integ.infra = infra;
    integ.routes = routes;
    integ.ports = Arc::new(SysPorts);
    #[cfg(unix)]
    {
        integ.processes = Arc::new(SysProcesses);
    }
    #[cfg(windows)]
    {
        integ.processes = Arc::new(WindowsProcesses {
            helper: devlanes_runner_windows::ConsoleHelper::current_exe().ok(),
        });
    }
    integ.ca_env = ca_env;
    integ.ca_status = ca_status;
    integ.ca_permitted = ca_permitted;
    integ.presets = presets();
    integ.launch_checks = vec![Arc::new(DistinctTauriIdentifiers)];
    integ.remote = Arc::new(SshConnector {
        home: paths.home.clone(),
        exe: std::env::current_exe().ok(),
    });
    #[cfg(unix)]
    {
        integ.hosts = hosts_file(hosts_policy, hosts_tld, hosts_probe)
            .await
            .map(Arc::new);
    }
    // Names under the resolver TLD go through the NRPT rule instead.
    #[cfg(windows)]
    {
        let _ = hosts_tld;
        integ.native_hosts = Some(native_hosts(hosts_policy, hosts_probe).await);
    }
    integ.pressure = pressure_source();
    integ.system_pressure = pressure_source();
    integ.start_gate = Some(start_gate(&user.startup, integ.pressure.clone()));
    integ.resolver_files = Some(Arc::new(OsResolverFiles));
    let mut options = EngineOptions::new(&paths.home, &paths.socket);
    options.db_path = paths.db.clone();
    options.logs = paths.logs.clone();
    // A service manager would not start the daemon again, so only a spawned one exits.
    options.idle_exit = (spawned && user.daemon.exit_when_idle).then_some(IDLE_EXIT_GRACE);
    options.idle = idle;
    options.user = user;
    let engine: Arc<dyn Handler> = Arc::new(Engine::new(options, store, integ, shutdown));
    Ok(engine)
}

/// Bound on the startup `*.localhost` probe; past it the resolver counts as failing.
const HOSTS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Windows answers every `*.localhost` name itself, so nothing writes the hosts
/// file; `doctor` shows the startup probe that confirms it.
#[cfg(windows)]
async fn native_hosts(
    policy: devlanes_resolver_hosts::HostsFilePolicy,
    probe: Option<tokio::task::JoinHandle<devlanes_resolver_hosts::LocalhostResolution>>,
) -> devlanes_daemon::hosts::NativeHosts {
    let resolution = match probe {
        // A panicked probe proves nothing about the resolver; treat it like a failure.
        Some(probe) => Some(
            probe
                .await
                .unwrap_or(devlanes_resolver_hosts::LocalhostResolution::Fails),
        ),
        None => None,
    };
    tracing::info!(mode = %policy.user_mode(), ?resolution, "hosts file not used on Windows");
    devlanes_daemon::hosts::NativeHosts::new(policy, resolution)
}

/// The `/etc/hosts` fallback over `sudo -n` and the installed helper. Only names
/// under `.localhost` are written when the configured TLD is one the helper refuses.
#[cfg(unix)]
async fn hosts_file(
    policy: devlanes_resolver_hosts::HostsFilePolicy,
    tld: Option<String>,
    probe: Option<tokio::task::JoinHandle<devlanes_resolver_hosts::LocalhostResolution>>,
) -> Option<devlanes_daemon::hosts::HostsFile> {
    use devlanes_resolver_hosts::{
        Executor, FileKind, FsProbe, HostsClient, INSTALL_PATH, LocalhostResolution,
        SystemExecutor, SystemFs,
    };
    let resolution = match probe {
        // A panicked probe proves nothing about the resolver; treat it like a failure.
        Some(probe) => Some(probe.await.unwrap_or(LocalhostResolution::Fails)),
        None => None,
    };
    tracing::info!(mode = %policy.user_mode(), ?resolution, "hosts file policy");
    let executor: Arc<dyn Executor> = Arc::new(SystemExecutor);
    let fs: Arc<dyn FsProbe> = Arc::new(SystemFs);
    let (client, tld_error) = match HostsClient::new(executor.clone(), fs.clone(), tld.as_deref()) {
        Ok(client) => (client, None),
        Err(error) => match HostsClient::new(executor, fs, None) {
            Ok(client) => (client, Some(error)),
            Err(error) => {
                tracing::warn!(%error, "hosts file fallback unavailable");
                return None;
            }
        },
    };
    let installed = matches!(
        SystemFs.lstat(Path::new(INSTALL_PATH)),
        Ok(Some(info)) if info.kind == FileKind::File
    );
    Some(devlanes_daemon::hosts::HostsFile::spawn(
        client,
        devlanes_daemon::hosts::HostsOptions {
            policy,
            resolution,
            installed,
            tld_error,
            sync: devlanes_resolver_hosts::SyncConfig::default(),
            // Shipped together: the helper next to this binary has this version.
            expected_version: env!("CARGO_PKG_VERSION").into(),
        },
    ))
}

/// `~/.devlanes/config.yaml`, or defaults when the file is absent. An invalid file stops
/// the daemon from starting: running with silently defaulted settings (another CA, other
/// ports) surprises more than an error naming the line to fix. The error is one
/// `USER_CONFIG_INVALID:` line, which the spawning CLI turns back into a `config_error`.
async fn load_user_config(home: &Path) -> anyhow::Result<devlanes_config::UserConfig> {
    let path = home.join("config.yaml");
    tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return Ok(devlanes_config::UserConfig::default());
        }
        devlanes_config::load_user(&path)
            .map_err(|error| anyhow::anyhow!(crate::user_config::startup_reason(&error)))
    })
    .await?
}

/// Platform, env-manager and preset contributors (priority orders them).
fn contributors() -> Vec<Arc<dyn Contributor>> {
    vec![
        Arc::new(devlanes_env_mise::MiseContributor::default()),
        Arc::new(devlanes_env_direnv::DirenvContributor::default()),
        Arc::new(devlanes_platform_node::NodeContributor),
        Arc::new(devlanes_platform_node::CloudflareContributor),
        Arc::new(devlanes_platform_python::PythonContributor),
        Arc::new(devlanes_platform_bun::BunContributor),
        Arc::new(devlanes_platform_deno::DenoContributor),
        Arc::new(devlanes_platform_rust::RustContributor),
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
        Arc::new(devlanes_run_script_deno::DenoTasks),
        Arc::new(devlanes_run_script_cargo::CargoBins),
        Arc::new(devlanes_run_script_python::PythonScripts),
    ]
}

/// Preset option parsers for `config check`.
fn presets() -> Vec<PresetEntry> {
    vec![
        PresetEntry {
            name: devlanes_preset_vite::NAME,
            parse: Arc::new(|value| {
                devlanes_preset_vite::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
        PresetEntry {
            name: devlanes_preset_next::NAME,
            parse: Arc::new(|value| {
                devlanes_preset_next::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
        PresetEntry {
            name: devlanes_preset_wrangler::NAME,
            parse: Arc::new(|value| {
                devlanes_preset_wrangler::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
        PresetEntry {
            name: devlanes_preset_tauri::NAME,
            parse: Arc::new(|value| {
                devlanes_preset_tauri::parse_options(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }),
        },
    ]
}

/// System CPU and memory pressure for the start gate and the memory warning.
fn pressure_source() -> Option<Arc<dyn devlanes_core::gate::PressureSource>> {
    #[cfg(target_os = "macos")]
    let source: Option<Arc<dyn devlanes_core::gate::PressureSource>> =
        Some(Arc::new(devlanes_metrics_macos::MacosPressure::new()));
    #[cfg(target_os = "linux")]
    let source: Option<Arc<dyn devlanes_core::gate::PressureSource>> =
        Some(Arc::new(devlanes_metrics_linux::LinuxPressure::new()));
    #[cfg(windows)]
    let source: Option<Arc<dyn devlanes_core::gate::PressureSource>> =
        Some(Arc::new(devlanes_metrics_windows::WindowsPressure::new()));
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let source = None;
    source
}

/// The machine-wide start gate for `startup.mode`. The default mode is
/// `devlanes_config::DEFAULT_STARTUP_MODE`.
fn start_gate(
    startup: &devlanes_config::StartupConfig,
    pressure: Option<Arc<dyn devlanes_core::gate::PressureSource>>,
) -> Arc<dyn devlanes_core::gate::StartGate> {
    use devlanes_start_gate::{AdaptiveConfig, AdaptiveGate, AllAtOnce, FixedWindow, GateOptions};
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let (initial, max) = startup.windows(cores);
    let options = GateOptions {
        slot_timeout: startup.slot_timeout.0,
        settle_cpu: startup.settle_cpu.0,
        reserve_memory: startup.reserve_memory.map(|bytes| bytes.0),
        ..GateOptions::default()
    };
    let clock = Arc::new(devlanes_core::clock::TokioClock::new());
    tracing::info!(mode = ?startup.mode, initial, max, "start gate");
    match (startup.mode, pressure) {
        (devlanes_config::StartupMode::All, _) => Arc::new(AllAtOnce::new(options, clock)),
        (devlanes_config::StartupMode::Adaptive, Some(pressure)) => Arc::new(AdaptiveGate::new(
            AdaptiveConfig::new(initial, max, startup.target_cpu.0),
            options,
            clock,
            pressure,
        )),
        (devlanes_config::StartupMode::Fixed, Some(pressure)) => {
            Arc::new(FixedWindow::with_pressure(max, options, clock, pressure))
        }
        // Without pressure readings an adaptive window could never grow.
        (mode, None) => {
            if mode == devlanes_config::StartupMode::Adaptive {
                tracing::warn!(
                    max,
                    "startup.mode adaptive needs CPU and memory readings, which this platform lacks; using a fixed window of `max`"
                );
            }
            Arc::new(FixedWindow::new(max, options, clock))
        }
    }
}

/// Process collectors for this OS plus Docker container stats. Container stats
/// reuse the infra client so they read the engine the Docker context selects.
fn metrics_sources(
    docker: Option<&devlanes_infra_docker::DockerInfra>,
) -> Vec<Arc<dyn devlanes_metrics::MetricsSource>> {
    let mut sources: Vec<Arc<dyn devlanes_metrics::MetricsSource>> = Vec::new();
    #[cfg(target_os = "macos")]
    match devlanes_metrics_macos::MacosSource::new() {
        Ok(source) => sources.push(Arc::new(source)),
        Err(error) => tracing::warn!(%error, "process metrics unavailable"),
    }
    #[cfg(target_os = "linux")]
    sources.push(Arc::new(devlanes_metrics_linux::LinuxSource::new()));
    #[cfg(windows)]
    sources.push(Arc::new(devlanes_metrics_windows::WindowsSource::new()));
    match docker {
        Some(docker) => sources.push(Arc::new(devlanes_metrics_docker::DockerSource::new(
            docker.client().clone(),
        ))),
        None => tracing::debug!("container metrics unavailable without a Docker client"),
    }
    sources
}

/// Opens state.db. A database from before the Diesel store cannot be migrated;
/// the error names the reset command (the first line reaches the spawning CLI).
async fn open_store(db: &Path) -> anyhow::Result<devlanes_state::Store> {
    match devlanes_state::Store::open(db).await {
        Ok(store) => Ok(store),
        Err(devlanes_state::Error::LegacySchema { version }) => Err(anyhow::anyhow!(
            "STATE_LEGACY_SCHEMA: state.db has a pre-release schema ({version}); run `devlanes daemon reset-state --yes`\n\
             database: {}",
            db.display()
        )),
        Err(error) => Err(error.into()),
    }
}

/// Checks the file `devlanes certs trust --resolver` writes (the NRPT rule on
/// Windows).
struct OsResolverFiles;

impl devlanes_daemon::integrations::ResolverFiles for OsResolverFiles {
    /// `{path, installed, matches}`; `matches` is false when the file routes the TLD
    /// elsewhere (another port, a stale install), `{error}` when the OS has no form.
    #[cfg(unix)]
    fn status(&self, tld: &str, server: std::net::SocketAddr) -> Value {
        match crate::local::resolver_file(tld, server) {
            Ok(file) => {
                let current = std::fs::read_to_string(&file.path).ok();
                json!({
                    "path": file.path,
                    "installed": current.is_some(),
                    "matches": current.as_deref() == Some(file.contents.as_str()),
                })
            }
            Err(error) => json!({"error": error}),
        }
    }

    /// The NRPT rule in the same shape; see [`crate::local::nrpt::doctor_status`].
    #[cfg(windows)]
    fn status(&self, tld: &str, server: std::net::SocketAddr) -> Value {
        crate::local::nrpt::doctor_status(tld, server)
    }
}

/// Trust env for contributors, `doctor` facts and the CA itself.
type LoadedCa = (
    BTreeMap<String, String>,
    Value,
    Option<Arc<devlanes_pki::CertificateAuthority>>,
);

/// Loads the CA selected by `tls.ca`: devlanes' own under `$DEVLANES_HOME/ca`
/// (created on first use), or mkcert's from `mkcert -CAROOT`. The combined bundle
/// is written under `$DEVLANES_HOME/ca` either way, so contributors and the proxy
/// use the same root.
/// A new devlanes CA is name-constrained to the special-use names plus `tld`.
async fn certificate_authority(
    home: &Path,
    kind: devlanes_config::CaKind,
    tld: Option<&str>,
) -> LoadedCa {
    certificate_authority_with(home, kind, Path::new("mkcert"), tld).await
}

/// [`certificate_authority`] with an explicit `mkcert` program (tests).
async fn certificate_authority_with(
    home: &Path,
    kind: devlanes_config::CaKind,
    mkcert: &Path,
    tld: Option<&str>,
) -> LoadedCa {
    let tld = tld.map(str::to_owned);
    let failed = |kind: &str, code: &str, message: String| -> LoadedCa {
        tracing::warn!(code, %message, "CA unavailable; HTTPS is off");
        (
            BTreeMap::new(),
            json!({"present": false, "kind": kind, "error": code, "message": message}),
            None,
        )
    };
    let caroot = match kind {
        devlanes_config::CaKind::Devlanes => None,
        devlanes_config::CaKind::Mkcert => {
            let output = tokio::process::Command::new(mkcert)
                .arg("-CAROOT")
                .stdin(std::process::Stdio::null())
                .output()
                .await;
            match output {
                // Exit status, emptiness and absoluteness are all checked there.
                Ok(output) => match crate::local::mkcert_caroot(&output) {
                    Ok(root) => Some(root),
                    Err(message) => return failed("mkcert", "mkcert_failed", message),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return failed(
                        "mkcert",
                        "mkcert_missing",
                        "tls.ca is mkcert but `mkcert` is not on PATH; install mkcert or set tls.ca: devlanes".into(),
                    );
                }
                Err(error) => return failed("mkcert", "mkcert_failed", error.to_string()),
            }
        }
    };
    let directory = home.join("ca");
    let kind_word = if caroot.is_some() {
        "mkcert"
    } else {
        "devlanes"
    };
    let root = caroot.clone();
    let result = tokio::task::spawn_blocking(move || {
        let (ca, cert) = match &root {
            Some(root) => (
                devlanes_pki::CertificateAuthority::from_mkcert(root)?,
                root.join("rootCA.pem"),
            ),
            None => (
                devlanes_pki::CertificateAuthority::load_or_create_with_tld(
                    &directory,
                    tld.as_deref(),
                )?,
                directory.join("ca.pem"),
            ),
        };
        #[cfg(unix)]
        if !directory.exists() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&directory)?;
        }
        // With mkcert only the bundle lives here, so the CA never created it.
        #[cfg(windows)]
        if !directory.exists() {
            if let Some(parent) = directory.parent() {
                std::fs::create_dir_all(parent)?;
            }
            devlanes_pki::create_private_dir(&directory)?;
        }
        #[cfg(unix)]
        let system = [
            "/etc/ssl/cert.pem",
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
        ]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
        #[cfg(windows)]
        let system = {
            let roots = devlanes_trust_windows::bundle_roots(ca.certificate_der().as_ref());
            for problem in &roots.problems {
                tracing::warn!(%problem, "bundle.pem roots");
            }
            roots.pem
        };
        let bundle = directory.join("bundle.pem");
        let temp = directory.join(".bundle.pem.tmp");
        std::fs::write(&temp, ca.combined_bundle(&system))?;
        #[cfg(unix)]
        std::fs::rename(&temp, &bundle)?;
        // A program reading the old bundle briefly blocks the replace on Windows.
        #[cfg(windows)]
        devlanes_sys_windows::file::rename_retrying(&temp, &bundle)?;
        Ok::<_, devlanes_pki::Error>((ca, cert, bundle))
    })
    .await;
    match result {
        Ok(Ok((ca, cert, bundle))) => {
            let env = BTreeMap::from([
                (CA_CERT_ENV.to_owned(), cert.display().to_string()),
                (CA_BUNDLE_ENV.to_owned(), bundle.display().to_string()),
            ]);
            let constrained = devlanes_trust::CaCertificate::from_pem(ca.certificate_pem(), "ca")
                .ok()
                .map(|c| c.is_name_constrained());
            // Only devlanes' own CA can be recreated with constraints; mkcert's never has them.
            let recreate = caroot.is_none() && constrained == Some(false);
            let status = json!({
                "present": true,
                "kind": kind_word,
                "path": cert,
                "caroot": caroot,
                "trusted": "unknown",
                "name_constrained": constrained,
                "hint": recreate.then_some(crate::local::UNCONSTRAINED_CA),
                "fix": recreate.then(|| crate::local::recreate_ca_command(home)),
            });
            (env, status, Some(Arc::new(ca)))
        }
        Ok(Err(error)) => failed(kind_word, "ca_invalid", error.to_string()),
        Err(error) => failed(kind_word, "ca_invalid", error.to_string()),
    }
}

/// Binds the proxy (HTTP with fallback 8080, HTTPS with fallback 8443 when a CA
/// exists) and returns the routing sink. Bind failures are reported, not fatal.
async fn proxy(
    user: &devlanes_config::UserConfig,
    ca: Option<Arc<devlanes_pki::CertificateAuthority>>,
    idle: Arc<devlanes_daemon::idle::IdleBoard>,
    shutdown: CancellationToken,
) -> ProxyRoutes {
    let lan = user.proxy.lan;
    let mut status = serde_json::Map::new();
    let https = match &ca {
        Some(_) => match devlanes_proxy::bind_listeners(
            user.proxy.https_port,
            devlanes_config::HTTPS_FALLBACK_PORT,
            lan,
        ) {
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
        via: user.proxy.via,
        activity: Some(Arc::new(LaneActivity(idle))),
        ..devlanes_proxy::ProxyConfig::default()
    };
    let proxy = devlanes_proxy::Proxy::new(config, devlanes_proxy::RouteTable::default());
    if let (Some(listeners), Some(ca)) = (https, ca) {
        match proxy.tls_config(ca) {
            Ok(tls) => {
                status.insert(
                    "https".into(),
                    json!({"bound": true, "port": listeners.port, "requested_port": user.proxy.https_port, "fallback_reason": listeners.fallback_reason}),
                );
                for listener in listeners.listeners() {
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
    match devlanes_proxy::bind_listeners(
        user.proxy.http_port,
        devlanes_config::HTTP_FALLBACK_PORT,
        lan,
    ) {
        Ok(listeners) => {
            status.insert(
                "http".into(),
                json!({"bound": true, "port": listeners.port, "requested_port": user.proxy.http_port, "fallback_reason": listeners.fallback_reason}),
            );
            for listener in listeners.listeners() {
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
        rejected: Mutex::new(Vec::new()),
    }
}

/// Reports proxy traffic to the idle lanes board, which thaws frozen lanes.
struct LaneActivity(Arc<devlanes_daemon::idle::IdleBoard>);

impl devlanes_proxy::LaneActivity for LaneActivity {
    /// Records the request; for a frozen lane, waits until it runs again.
    fn request(
        &self,
        owner: &str,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        self.0.request(owner)
    }

    /// Records client bytes on an upgraded stream.
    fn stream(&self, owner: &str) {
        self.0.stream(owner);
    }
}

/// Publishes engine routes to the proxy's atomic table.
struct ProxyRoutes {
    proxy: devlanes_proxy::Proxy,
    status: Value,
    /// Entries the last snapshot dropped: `{host, lane, unit, reason}`.
    rejected: Mutex<Vec<Value>>,
}

impl RouteSink for ProxyRoutes {
    /// Converts and swaps the table. Invalid and duplicate entries are dropped
    /// one by one (and listed in `status().rejected`) so they never hold back
    /// the routes of other lanes.
    fn publish(&self, routes: Vec<RouteEntry>) {
        let owners: Vec<(String, String)> = routes
            .iter()
            .map(|entry| (entry.lane.clone(), entry.unit.clone()))
            .collect();
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
                    RouteStatus::Reconnecting => devlanes_proxy::RouteState::Reconnecting,
                };
                route.start_hint = entry.start_hint;
                route.certificate_names = entry.certificate_names;
                route.owner = Some(entry.lane).filter(|lane| !lane.is_empty());
                route.unit = Some(entry.unit).filter(|unit| !unit.is_empty());
                route.remote = entry.remote;
                route
            })
            .collect();
        let (table, rejected) = devlanes_proxy::RouteTable::accept(routes);
        self.proxy.replace_routes(table);
        let rejected: Vec<Value> = rejected
            .into_iter()
            .map(|rejected| {
                tracing::warn!(host = %rejected.host, error = %rejected.error, "route dropped");
                let (lane, unit) = owners.get(rejected.index).cloned().unwrap_or_default();
                json!({
                    "host": rejected.host,
                    "lane": lane,
                    "unit": unit,
                    "reason": rejected.error.to_string(),
                })
            })
            .collect();
        *self.rejected.lock().unwrap_or_else(|e| e.into_inner()) = rejected;
    }

    /// Bind results plus the accept loops' live health, and the entries the last
    /// snapshot dropped.
    fn status(&self) -> Value {
        let mut status = listener_status(&self.status, &self.proxy.listener_health());
        let rejected = self
            .rejected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(map) = status.as_object_mut()
            && !rejected.is_empty()
        {
            map.insert("rejected".into(), Value::Array(rejected));
        }
        status
    }
}

/// `{http, https}` from the bind results, each bound one extended with
/// `serving` (every listener of that kind running without a pending accept
/// error) and its `listeners`: `[{address, running, accept_errors, last_error}]`.
fn listener_status(bound: &Value, health: &[devlanes_proxy::ListenerHealth]) -> Value {
    let mut status = bound.clone();
    for (kind, tls) in [("http", false), ("https", true)] {
        let Some(entry) = status.get_mut(kind).and_then(Value::as_object_mut) else {
            continue;
        };
        if entry.get("bound").and_then(Value::as_bool) != Some(true) {
            entry.insert("serving".into(), Value::Bool(false));
            continue;
        }
        let listeners: Vec<&devlanes_proxy::ListenerHealth> = health
            .iter()
            .filter(|listener| listener.tls == tls)
            .collect();
        let serving = !listeners.is_empty()
            && listeners
                .iter()
                .all(|listener| listener.running && listener.last_error.is_none());
        entry.insert("serving".into(), Value::Bool(serving));
        entry.insert(
            "listeners".into(),
            listeners
                .iter()
                .map(|listener| {
                    json!({
                        "address": listener.address,
                        "running": listener.running,
                        "accept_errors": listener.accept_errors,
                        "last_error": listener.last_error,
                    })
                })
                .collect(),
        );
    }
    status
}

/// `connect` over system OpenSSH, with ssh's prompts relayed to the attached CLI.
struct SshConnector {
    /// `$DEVLANES_HOME`; control sockets live in its `run/ssh`.
    home: PathBuf,
    /// This binary, which ssh runs as `askpass`; without it ssh runs in batch mode.
    exe: Option<PathBuf>,
}

impl RemoteConnector for SshConnector {
    /// One SSH transport (and ControlMaster) per box; Windows has no
    /// ControlMaster and runs one `ssh.exe` per session.
    fn transport(
        &self,
        host: &str,
        prompts: Arc<dyn RemotePrompter>,
        connect: ConnectOptions,
    ) -> Result<Arc<dyn devlanes_transport::Transport>, RpcError> {
        let mut options = devlanes_transport_ssh::SshOptions::new(&self.home);
        // A box without devlanes gets this client's own release, and only with
        // `remote connect --install`; otherwise the plan is reported.
        options.manifest_url = Some(devlanes_version::release_manifest_url(
            devlanes_version::VERSION,
        ));
        options.bootstrap = if connect.install {
            devlanes_transport_ssh::BootstrapPolicy::Install
        } else {
            devlanes_transport_ssh::BootstrapPolicy::Report
        };
        options.askpass = self
            .exe
            .clone()
            .map(|exe| devlanes_transport_ssh::AskpassConfig {
                exe,
                relay: Arc::new(PromptBridge(prompts)),
            });
        Ok(Arc::new(devlanes_transport_ssh::SshTransport::new(
            host, options,
        )))
    }
}

/// Adapts the daemon's prompt relay to the SSH transport's.
struct PromptBridge(Arc<dyn RemotePrompter>);

#[async_trait]
impl devlanes_transport_ssh::PromptRelay for PromptBridge {
    /// Forwards with the CLI's prompt kind words.
    async fn prompt(&self, prompt: devlanes_transport_ssh::AskpassPrompt) -> Option<String> {
        let kind = match prompt.kind {
            devlanes_transport_ssh::PromptKind::Secret => "secret",
            devlanes_transport_ssh::PromptKind::Confirm => "confirm",
            devlanes_transport_ssh::PromptKind::Notify => "notify",
        };
        self.0
            .ask(RemotePrompt {
                host: prompt.destination,
                prompt: prompt.prompt,
                kind,
            })
            .await
    }
}

/// Git for Windows' `sh.exe` for `run:` strings and `ready.cmd`, looked up
/// once at daemon start; units and checks that need it fail with
/// `shell_missing` without one.
#[cfg(windows)]
async fn windows_shell() -> Option<PathBuf> {
    let shell = devlanes_shell_windows::windows_shell().await;
    match &shell {
        Some(path) => {
            tracing::info!(shell = %path.display(), "shell for run strings and ready.cmd")
        }
        None => {
            tracing::warn!("no Git for Windows sh.exe found; run strings and ready.cmd cannot run")
        }
    }
    shell
}

/// [`SysProcesses`] whose orphan stops ask first, as a running unit's stop
/// does: `WM_CLOSE` and one Ctrl-Break per unit through `helper`, then
/// termination of what is left after the grace period.
#[cfg(windows)]
struct WindowsProcesses {
    helper: Option<devlanes_runner_windows::ConsoleHelper>,
}

#[cfg(windows)]
#[async_trait]
impl devlanes_daemon::integrations::ProcessTable for WindowsProcesses {
    /// Delegates.
    async fn alive(&self, identity: ProcessIdentity) -> bool {
        devlanes_daemon::integrations::ProcessTable::alive(&SysProcesses, identity).await
    }

    /// Delegates: a verified terminate.
    async fn terminate(&self, identity: ProcessIdentity, grace: Duration) {
        devlanes_daemon::integrations::ProcessTable::terminate(&SysProcesses, identity, grace)
            .await;
    }

    /// Delegates: a verified resume. A unit's recorded members are every
    /// process of its job but the leader, so resuming each covers the unit.
    async fn resume(&self, identity: ProcessIdentity) {
        devlanes_daemon::integrations::ProcessTable::resume(&SysProcesses, identity).await;
    }

    /// See [`devlanes_runner_windows::stop_orphan`].
    async fn terminate_unit(
        &self,
        leader: ProcessIdentity,
        members: &[ProcessIdentity],
        grace: Duration,
    ) {
        let (helper, members) = (self.helper.clone(), members.to_vec());
        let stopped = tokio::task::spawn_blocking(move || {
            devlanes_runner_windows::stop_orphan(helper.as_ref(), leader, &members, grace)
        })
        .await;
        match stopped {
            Ok(stop) => tracing::info!(
                pid = leader.pid,
                asked_through = ?stop.asked_through,
                windows = stop.windows,
                running = stop.running,
                terminated = stop.terminated,
                "stopped an orphaned unit"
            ),
            Err(error) => {
                tracing::warn!(%error, pid = leader.pid, "stopping an orphaned unit failed")
            }
        }
    }
}

/// The job-object runner with identity and socket inspection. Tasks use
/// [`UnitRunner::spawn_task`]'s default: on Windows they sweep like services.
#[cfg(windows)]
struct WindowsUnits(devlanes_runner_windows::WindowsRunner);

#[cfg(windows)]
#[async_trait]
impl UnitRunner for WindowsUnits {
    /// Spawns in a job, with piped output or, for `pty: true`, a pseudo console.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        Ok(Arc::new(WindowsManaged(self.0.spawn_unit(spec).await?)))
    }
}

/// [`devlanes_runner_windows::WindowsUnit`] as a daemon unit.
#[cfg(windows)]
struct WindowsManaged(Arc<devlanes_runner_windows::WindowsUnit>);

#[cfg(windows)]
#[async_trait]
impl Unit for WindowsManaged {
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
    /// Delegates: true for `pty: true` units, which run in a pseudo console.
    fn renders_screen(&self) -> bool {
        self.0.renders_screen()
    }
    /// Delegates.
    async fn set_power(&self, power: devlanes_core::Power) -> Result<(), devlanes_core::Error> {
        self.0.set_power(power).await
    }
}

#[cfg(windows)]
#[async_trait]
impl ManagedUnit for WindowsManaged {
    /// Leader identity recorded at spawn.
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.0.identity())
    }
    /// Listening TCP ports of the unit's job.
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
    /// The runner's exit report with leftover processes by pid and name.
    fn exit_details(&self) -> Option<ExitDetails> {
        let report = self.0.exit_report()?;
        Some(ExitDetails {
            outcome: report.outcome,
            left_running: left_processes(report.left_running),
            escaped: left_processes(report.escaped),
        })
    }
}

/// Leftover processes by pid and name, for exit details.
#[cfg(windows)]
fn left_processes(list: Vec<devlanes_sys::ProcessInfo>) -> Vec<devlanes_proto::LeftProcess> {
    list.into_iter()
        .map(|process| devlanes_proto::LeftProcess {
            pid: process.pid,
            command: process.name,
        })
        .collect()
}

/// The PTY runner with identity and socket inspection.
#[cfg(unix)]
struct PtyUnits(devlanes_runner_pty::PtyRunner);

#[cfg(unix)]
#[async_trait]
impl UnitRunner for PtyUnits {
    /// Spawns in a PTY or pipes per the spec.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        Ok(Arc::new(PtyManaged(self.0.spawn_unit(spec).await?)))
    }

    /// Tasks keep daemons they started in their own session running after they exit.
    async fn spawn_task(
        &self,
        spec: LaunchSpec,
    ) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        let runner = self
            .0
            .clone()
            .with_exit_sweep(devlanes_runner_pty::ExitSweep::KeepDaemonized);
        Ok(Arc::new(PtyManaged(runner.spawn_unit(spec).await?)))
    }
}

/// [`devlanes_runner_pty::PtyUnit`] as a daemon unit.
#[cfg(unix)]
struct PtyManaged(Arc<devlanes_runner_pty::PtyUnit>);

#[cfg(unix)]
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
    /// Delegates.
    async fn set_power(&self, power: devlanes_core::Power) -> Result<(), devlanes_core::Error> {
        self.0.set_power(power).await
    }
}

#[cfg(unix)]
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
    /// The runner's exit report with leftover processes by pid and name.
    fn exit_details(&self) -> Option<ExitDetails> {
        let report = self.0.exit_report()?;
        let processes = |list: Vec<devlanes_sys::ProcessInfo>| {
            list.into_iter()
                .map(|process| devlanes_proto::LeftProcess {
                    pid: process.pid,
                    command: process.name,
                })
                .collect()
        };
        Some(ExitDetails {
            outcome: report.outcome,
            left_running: processes(report.left_running),
            escaped: processes(report.escaped),
        })
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

/// The daemon variables the `docker` CLI needs: where to find binaries and its
/// config, which engine to talk to, locale and proxies. A daemon started by hand
/// may carry a full shell environment; none of the rest reaches Compose.
/// Windows names match in any case (the variable is `Path` there).
fn compose_base_env(vars: impl Iterator<Item = (String, String)>) -> BTreeMap<String, String> {
    /// The docker CLI finds Docker Desktop's plugins, Compose among them, under
    /// `ProgramFiles`, and its config and contexts under `USERPROFILE`; Winsock
    /// needs `SystemRoot`.
    #[cfg(windows)]
    const PLATFORM: &[&str] = &[
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "TEMP",
        "TMP",
        "USERNAME",
    ];
    #[cfg(not(windows))]
    const PLATFORM: &[&str] = &[];
    const NAMES: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "LANG",
        "TZ",
        "SSH_AUTH_SOCK",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ];
    const PREFIXES: &[&str] = &["LC_", "XDG_", "DOCKER_"];
    let same = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    vars.filter(|(name, _)| {
        NAMES.iter().chain(PLATFORM).any(|known| same(name, known))
            || PREFIXES.iter().any(|prefix| {
                name.get(..prefix.len())
                    .is_some_and(|start| same(start, prefix))
            })
    })
    .collect()
}

/// Containers a pause or unpause changed through Compose and the Engine API
/// together. Without Docker nothing changed. An error is returned only when
/// nothing changed, so one failing side does not hide what the other did.
fn changed_containers(
    compose: Result<usize, InfraError>,
    docker: Result<usize, InfraError>,
) -> Result<usize, InfraError> {
    let mut changed = 0;
    let mut failure = None;
    for result in [compose, docker] {
        match result {
            Ok(count) => changed += count,
            Err(error) if error.code == "docker_unavailable" => {}
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    match failure {
        Some(error) if changed == 0 => Err(error),
        _ => Ok(changed),
    }
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
        devlanes_infra_compose::Error::Command {
            command,
            status,
            stderr,
        } => InfraError::new("compose_failed", error.to_string()).with_command(
            command.clone(),
            Some(status.clone()),
            Some(stderr),
        ),
        devlanes_infra_compose::Error::Timeout { command, timeout } => {
            InfraError::new("compose_failed", error.to_string()).with_command(
                command.clone(),
                Some(format!("timed out after {}s", timeout.as_secs())),
                None,
            )
        }
        devlanes_infra_compose::Error::Json { command, source } => {
            InfraError::new("compose_failed", error.to_string()).with_command(
                command.clone(),
                Some(format!("unexpected output: {source}")),
                None,
            )
        }
        _ => InfraError::new("compose_failed", error.to_string()),
    }
}

/// Docker API failures for `operation` (such as `ensure container acme-db`);
/// connection problems mean Docker is unavailable.
fn docker_error(operation: &str) -> impl FnOnce(devlanes_infra_docker::Error) -> InfraError {
    let command = format!("docker API: {operation}");
    move |error| {
        let text = error.to_string();
        if matches!(error, devlanes_infra_docker::Error::Io(_))
            || text.contains("connect")
            || text.contains("No such file")
        {
            return InfraError::new("docker_unavailable", text);
        }
        let (status, stderr) = match &error {
            devlanes_infra_docker::Error::Docker(
                bollard::errors::Error::DockerResponseServerError {
                    status_code,
                    message,
                },
            ) => (Some(format!("HTTP {status_code}")), Some(message.clone())),
            devlanes_infra_docker::Error::Unhealthy { health, .. } => {
                (Some(format!("{health:?}")), None)
            }
            devlanes_infra_docker::Error::HealthTimeout { timeout, last, .. } => (
                Some(format!(
                    "not healthy after {}s ({last:?})",
                    timeout.as_secs()
                )),
                None,
            ),
            _ => (None, None),
        };
        InfraError::new("docker_failed", text).with_command(command, status, stderr.as_deref())
    }
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
        // Compose interpolates `${VAR}` from its environment, so it gets only what
        // the docker CLI needs from the daemon's; each project adds `infra.env`
        // and its `.env` file (plan 7.1).
        let cli = devlanes_infra_compose::ComposeCli::new().with_base_env(compose_base_env(
            std::env::vars_os().filter_map(|(name, value)| {
                Some((name.into_string().ok()?, value.into_string().ok()?))
            }),
        ));
        Self {
            compose: devlanes_infra_compose::Compose::new(cli),
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
            let block = devlanes_infra_docker::PortBlock::new(first, size)
                .map_err(|error| InfraError::new("docker_failed", error.to_string()))?;
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

    /// Stats targets for compose services, one per running replica, with the
    /// state `docker compose ps` reported for the service.
    fn compose_targets(
        targets: &mut Vec<InfraContainer>,
        containers: &BTreeMap<String, Vec<String>>,
        health: &BTreeMap<String, devlanes_infra_compose::Health>,
        shared: bool,
    ) {
        for (service, names) in containers {
            let state = health.get(service).map(compose_state);
            targets.extend(names.iter().map(|docker| InfraContainer {
                name: service.clone(),
                docker: docker.clone(),
                shared,
                state: state.clone(),
            }));
        }
    }

    /// Stats targets for plain containers. Their names are deterministic, so no
    /// engine call is needed; a container that is not running yields no sample.
    /// `state` is what the caller knows, such as `running` right after `ensure`.
    fn plain_targets(
        targets: &mut Vec<InfraContainer>,
        containers: &[devlanes_infra_docker::ContainerSpec],
        state: Option<&str>,
    ) {
        targets.extend(containers.iter().map(|container| InfraContainer {
            name: container.name.clone(),
            docker: devlanes_infra_docker::container_name(
                &container.project,
                &container.name,
                container.scope,
            ),
            shared: container.scope == devlanes_infra_docker::Scope::Shared,
            state: state.map(str::to_owned),
        }));
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
                let exit_code = match &event.kind {
                    devlanes_infra_docker::EventKind::Died { exit_code } => *exit_code,
                    _ => None,
                };
                let _ = sender.send(InfraEvent {
                    project: project.clone(),
                    lane,
                    container: event.container_name.clone(),
                    state,
                    exit_code,
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
        let mut targets = Vec::new();
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
            Self::compose_targets(&mut targets, &shared.containers, &shared.health, true);
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
            Self::compose_targets(
                &mut targets,
                &lane_report.containers,
                &lane_report.health,
                false,
            );
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
                        .map_err(docker_error(&format!(
                            "ensure container {} (image {})",
                            container.name, container.image
                        )))?,
                );
            }
            Self::container_namespace(&mut namespace, &containers);
            // `ensure` returned, so each one runs; events refine it from here.
            Self::plain_targets(&mut targets, &containers, Some("running"));
            report.insert("containers".into(), json!(ensured));
            self.watch_events(&spec.project);
        }
        Ok(InfraUp {
            namespace: Value::Object(namespace),
            setup_env: self.setup_env(spec).await,
            report: Value::Object(report),
            containers: targets,
        })
    }

    /// Endpoints of what already runs, without starting anything.
    async fn namespace(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        let mut namespace = serde_json::Map::new();
        let mut targets = Vec::new();
        if !spec.compose_files.is_empty()
            && let Ok(shared) = self.compose.shared_status(&Self::shared(spec)).await
        {
            Self::compose_namespace(&mut namespace, &shared.endpoints);
            Self::compose_targets(&mut targets, &shared.containers, &shared.health, true);
        }
        if Self::has_lane_services(spec)
            && let Ok(lane) = self.compose.lane_status(&Self::lane(spec)).await
        {
            Self::compose_namespace(&mut namespace, &lane.endpoints);
            Self::compose_targets(&mut targets, &lane.containers, &lane.health, false);
        }
        if let Ok(containers) = Self::containers(spec) {
            Self::container_namespace(&mut namespace, &containers);
            Self::plain_targets(&mut targets, &containers, None);
        }
        Ok(InfraUp {
            namespace: Value::Object(namespace),
            setup_env: self.setup_env(spec).await,
            report: Value::Null,
            containers: targets,
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
            let managed =
                self.docker()?
                    .list_managed(&spec.project)
                    .await
                    .map_err(docker_error(&format!(
                        "list containers of {}",
                        spec.project
                    )))?;
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
                .map_err(docker_error(&format!(
                    "stop shared containers of {}",
                    spec.project
                )))?;
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
                    report.insert("compose_error".into(), error.data());
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

    /// Pauses the lane's own compose services and plain containers.
    async fn pause_lane(&self, project: &str, lane: u32) -> Result<usize, InfraError> {
        let compose = self
            .compose
            .lane_pause(project, lane)
            .await
            .map_err(compose_error);
        let docker = match &self.docker {
            Some(docker) => docker
                .pause_lane(project, lane)
                .await
                .map_err(docker_error("pause")),
            None => Ok(0),
        };
        changed_containers(compose, docker)
    }

    /// Unpauses the lane's own compose services and plain containers.
    async fn unpause_lane(&self, project: &str, lane: u32) -> Result<usize, InfraError> {
        let compose = self
            .compose
            .lane_unpause(project, lane)
            .await
            .map_err(compose_error);
        let docker = match &self.docker {
            Some(docker) => docker
                .unpause_lane(project, lane)
                .await
                .map_err(docker_error("unpause")),
            None => Ok(0),
        };
        changed_containers(compose, docker)
    }

    /// Stdout and stderr of any container through the Engine API.
    async fn container_logs(
        &self,
        docker: &str,
        options: ContainerLogOptions,
    ) -> Result<BoxStream<'static, Result<ContainerLogChunk, InfraError>>, InfraError> {
        use futures_util::StreamExt;
        let operation = format!("logs of container {docker}");
        let missing = format!("no container named {docker}");
        let stream = self
            .docker()?
            .logs(
                docker,
                devlanes_infra_docker::LogOptions {
                    tail: options.tail,
                    follow: options.follow,
                    since: options.since,
                    timestamps: options.timestamps,
                },
            )
            .map(move |chunk| match chunk {
                Ok(chunk) => Ok(ContainerLogChunk {
                    stderr: chunk.stream == devlanes_infra_docker::LogStream::Stderr,
                    bytes: chunk.bytes,
                }),
                Err(error) if error.is_not_found() => {
                    Err(InfraError::new("not_found", missing.clone()))
                }
                Err(error) => Err(docker_error(&operation)(error)),
            });
        Ok(stream.boxed())
    }

    /// `StartedAt` of plain containers and shared/per-lane compose services. The
    /// shared project is resolved and each compose project listed once per call,
    /// since every `config` and `ps` is a Compose process. A failed lookup leaves
    /// out only the containers it covers, as if they did not run.
    async fn containers_started_at(
        &self,
        spec: &InfraSpec,
        containers: &[String],
    ) -> Result<BTreeMap<String, String>, InfraError> {
        let Some(docker) = &self.docker else {
            return Ok(BTreeMap::new());
        };
        // The outer `None` means not looked up yet; an inner `None` a failed lookup,
        // which is not retried within the call.
        let mut shared_name: Option<Option<String>> = None;
        let mut listed: BTreeMap<String, Option<Vec<devlanes_infra_compose::ContainerStatus>>> =
            BTreeMap::new();
        let mut names = Vec::new();
        for container in containers {
            if let Some(declared) = spec.containers.iter().find(|c| &c.name == container) {
                let scope = if declared.per_lane {
                    devlanes_infra_docker::Scope::Lane(spec.lane)
                } else {
                    devlanes_infra_docker::Scope::Shared
                };
                names.push((
                    container,
                    devlanes_infra_docker::container_name(&spec.project, container, scope),
                ));
                continue;
            }
            let project = if spec.per_lane.iter().any(|s| s == container) {
                devlanes_infra_compose::lane_project_name(&spec.project, spec.lane).ok()
            } else {
                if shared_name.is_none() {
                    let resolved = self.compose.shared_project(&Self::shared(spec)).await;
                    shared_name = Some(
                        resolved
                            .ok()
                            .map(|(project, _)| project.name.unwrap_or_default()),
                    );
                }
                shared_name.clone().flatten()
            };
            let Some(project) = project else {
                continue;
            };
            if !listed.contains_key(&project) {
                let statuses = self.compose.cli().ps(&project).await.ok();
                listed.insert(project.clone(), statuses);
            }
            let found = listed
                .get(&project)
                .and_then(Option::as_ref)
                .and_then(|statuses| statuses.iter().find(|s| &s.service == container));
            if let Some(status) = found {
                names.push((container, status.id.clone()));
            }
        }
        let inspected =
            futures_util::future::join_all(names.into_iter().map(|(container, name)| async move {
                let started = match docker.client().inspect_container(&name, None).await {
                    Ok(inspect) => inspect.state.and_then(|state| state.started_at),
                    Err(_) => None,
                };
                started.map(|at| (container.clone(), at))
            }))
            .await;
        Ok(inspected.into_iter().flatten().collect())
    }

    /// Lists labelled containers so status can show them; never stops anything.
    async fn adopt(&self, projects: &[String]) -> Result<Value, InfraError> {
        let docker = self.docker()?;
        let mut adopted = serde_json::Map::new();
        for project in projects {
            let managed = docker
                .list_managed(project)
                .await
                .map_err(docker_error(&format!("list containers of {project}")))?;
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

    /// Shared and per-lane compose services from `docker compose ps`, then plain
    /// containers from the engine's list. A source that cannot answer adds nothing.
    async fn progress(&self, spec: &InfraSpec) -> Vec<devlanes_proto::PhaseContainer> {
        let mut progress = Vec::new();
        if !spec.compose_files.is_empty()
            && let Ok(shared) = self.compose.shared_status(&Self::shared(spec)).await
        {
            progress.extend(shared.health.iter().map(compose_progress));
        }
        if Self::has_lane_services(spec)
            && let Ok(lane) = self.compose.lane_status(&Self::lane(spec)).await
        {
            progress.extend(lane.health.iter().map(compose_progress));
        }
        if !spec.containers.is_empty()
            && let Some(docker) = &self.docker
            && let Ok(managed) = docker.list_managed(&spec.project).await
        {
            for container in &spec.containers {
                let scope = if container.per_lane {
                    devlanes_infra_docker::Scope::Lane(spec.lane)
                } else {
                    devlanes_infra_docker::Scope::Shared
                };
                let health = managed
                    .iter()
                    .find(|m| m.key.as_deref() == Some(&container.name) && m.scope == Some(scope))
                    .map_or(devlanes_infra_docker::Health::Missing, |m| m.health.clone());
                progress.push(docker_progress(&container.name, &health));
            }
        }
        progress
    }
}

/// A compose service's health as `ContainerSummary.state`.
fn compose_state(health: &devlanes_infra_compose::Health) -> String {
    use devlanes_infra_compose::Health;
    match health {
        Health::Running => "running".into(),
        Health::Starting => "starting".into(),
        Health::Healthy => "healthy".into(),
        Health::Unhealthy => "unhealthy".into(),
        Health::Exited { code } => format!("exited {code}"),
        Health::Other { state } => state.clone(),
        Health::Missing => "missing".into(),
    }
}

/// A compose service's health as start progress.
fn compose_progress(
    (name, health): (&String, &devlanes_infra_compose::Health),
) -> devlanes_proto::PhaseContainer {
    use devlanes_infra_compose::Health;
    use devlanes_proto::ContainerPhase;
    let (state, health) = match health {
        // `up --wait` takes a running service without a healthcheck as up.
        Health::Running => (ContainerPhase::Healthy, None),
        Health::Starting => (ContainerPhase::WaitingHealthy, Some("starting".to_owned())),
        Health::Healthy => (ContainerPhase::Healthy, Some("healthy".to_owned())),
        Health::Unhealthy => (ContainerPhase::Failed, Some("unhealthy".to_owned())),
        Health::Exited { code } => (ContainerPhase::Failed, Some(format!("exited ({code})"))),
        Health::Other { state } if state == "created" => (ContainerPhase::Creating, None),
        Health::Other { state } => (ContainerPhase::Starting, Some(state.clone())),
        Health::Missing => (ContainerPhase::Creating, None),
    };
    devlanes_proto::PhaseContainer {
        name: name.clone(),
        state,
        health,
    }
}

/// A plain container's health as start progress, in the same words as compose's.
fn docker_progress(
    name: &str,
    health: &devlanes_infra_docker::Health,
) -> devlanes_proto::PhaseContainer {
    use devlanes_infra_docker::Health;
    use devlanes_proto::ContainerPhase;
    let (state, health) = match health {
        Health::Running => (ContainerPhase::Healthy, None),
        Health::Starting => (ContainerPhase::WaitingHealthy, Some("starting".to_owned())),
        Health::Healthy => (ContainerPhase::Healthy, Some("healthy".to_owned())),
        Health::Unhealthy => (ContainerPhase::Failed, Some("unhealthy".to_owned())),
        // The engine's container list carries no exit code.
        Health::Exited { .. } => (ContainerPhase::Failed, Some("exited".to_owned())),
        Health::Other { state } if state == "created" => (ContainerPhase::Creating, None),
        Health::Other { state } => (ContainerPhase::Starting, Some(state.clone())),
        Health::Missing => (ContainerPhase::Creating, None),
    };
    devlanes_proto::PhaseContainer {
        name: name.to_owned(),
        state,
        health,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// A failed compose command keeps its command line, status and stderr tail.
    fn compose_command_failure_is_structured() {
        let error = compose_error(devlanes_infra_compose::Error::Command {
            command: "docker compose -p acme up --wait".into(),
            status: "exit status: 1".into(),
            stderr: "\x1b[31mpull access denied\x1b[0m".into(),
        });
        assert_eq!(error.code, "compose_failed");
        let data = error.data();
        assert_eq!(data["command"], "docker compose -p acme up --wait");
        assert_eq!(data["status"], "exit status: 1");
        assert_eq!(data["stderr"], "pull access denied");
        assert!(
            data["message"]
                .as_str()
                .is_some_and(|m| m.contains("pull access denied"))
        );
    }

    #[test]
    /// An engine error names the API call and Docker's HTTP status and message.
    fn docker_server_failure_is_structured() {
        let error = docker_error("ensure container db (image postgres:16)")(
            devlanes_infra_docker::Error::Docker(
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 404,
                    message: "No such image: postgres:16".into(),
                },
            ),
        );
        assert_eq!(error.code, "docker_failed");
        let data = error.data();
        assert_eq!(
            data["command"],
            "docker API: ensure container db (image postgres:16)"
        );
        assert_eq!(data["status"], "HTTP 404");
        assert_eq!(data["stderr"], "No such image: postgres:16");
    }

    #[test]
    /// Compose sees the docker CLI's variables and none of the shell's others.
    fn compose_base_env_keeps_docker_variables_only() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/acme"),
            ("DOCKER_HOST", "unix:///var/run/docker.sock"),
            ("DOCKER_CONFIG", "/home/acme/.docker"),
            ("LC_ALL", "C"),
            ("POSTGRES_PASSWORD", "secret"),
            ("COMPOSE_PROJECT_NAME", "other"),
            ("GITHUB_TOKEN", "secret"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let kept: Vec<String> = compose_base_env(vars.into_iter()).into_keys().collect();
        assert_eq!(
            kept,
            ["DOCKER_CONFIG", "DOCKER_HOST", "HOME", "LC_ALL", "PATH"]
        );
    }

    #[cfg(windows)]
    #[test]
    /// Windows spells the names in any case, and the docker CLI needs
    /// `ProgramFiles` to find Docker Desktop's Compose plugin.
    fn compose_base_env_keeps_windows_variables() {
        let vars = [
            ("Path", r"C:\Windows\system32"),
            ("ProgramFiles", r"C:\Program Files"),
            ("USERPROFILE", r"C:\Users\acme"),
            ("SystemRoot", r"C:\Windows"),
            ("Docker_Host", "npipe:////./pipe/docker_engine"),
            ("GITHUB_TOKEN", "secret"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let kept: Vec<String> = compose_base_env(vars.into_iter()).into_keys().collect();
        assert_eq!(
            kept,
            [
                "Docker_Host",
                "Path",
                "ProgramFiles",
                "SystemRoot",
                "USERPROFILE"
            ]
        );
    }

    /// A route entry of `lane:unit` for `host`.
    fn entry(host: &str, lane: &str, unit: &str, port: u16) -> RouteEntry {
        RouteEntry {
            host: host.into(),
            port,
            fallback: false,
            host_header_upstream: false,
            status: RouteStatus::Ready,
            start_hint: String::new(),
            project: "acme".into(),
            lane: lane.into(),
            unit: unit.into(),
            certificate_names: Vec::new(),
            remote: false,
        }
    }

    #[test]
    /// An invalid host and a trailing-dot duplicate are dropped and reported by
    /// owner; every other lane's routes still reach the proxy.
    fn proxy_routes_drop_bad_entries_one_by_one() {
        let sink = ProxyRoutes {
            proxy: devlanes_proxy::Proxy::new(
                devlanes_proxy::ProxyConfig::default(),
                devlanes_proxy::RouteTable::default(),
            ),
            status: json!({}),
            rejected: Mutex::new(Vec::new()),
        };
        sink.publish(vec![
            entry("web.acme.localhost", "acme@0", "web", 3000),
            entry("api_v2.acme.localhost", "acme@0", "api", 3001),
            entry("web.acme.localhost.", "acme@1", "web", 3010),
            entry("docs.acme-1.localhost", "acme@1", "docs", 3011),
        ]);
        let routes = sink.proxy.routes();
        let hosts: Vec<&str> = routes.routes().iter().map(|r| r.host.as_str()).collect();
        assert_eq!(hosts, ["docs.acme-1.localhost", "web.acme.localhost"]);
        let rejected = &sink.status()["rejected"];
        assert_eq!(rejected[0]["host"], "api_v2.acme.localhost");
        assert_eq!(rejected[0]["lane"], "acme@0");
        assert_eq!(rejected[0]["unit"], "api");
        assert_eq!(rejected[1]["host"], "web.acme.localhost.");
        assert_eq!(rejected[1]["lane"], "acme@1");
        assert!(
            rejected[1]["reason"]
                .as_str()
                .is_some_and(|r| r.contains("duplicate")),
            "{rejected}"
        );
        sink.publish(vec![entry("web.acme.localhost", "acme@0", "web", 3000)]);
        assert!(
            sink.status().get("rejected").is_none(),
            "a clean snapshot clears them"
        );
    }

    /// A fake `mkcert` that prints `caroot` for `-CAROOT`.
    #[cfg(unix)]
    fn fake_mkcert(dir: &Path, caroot: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("mkcert");
        std::fs::write(
            &program,
            format!("#!/bin/sh\necho '{}'\n", caroot.display()),
        )
        .expect("script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        program
    }

    /// A fake `mkcert` that prints `caroot` for `-CAROOT`: the prebuilt
    /// `fake-tool`, since Windows cannot run scripts as programs.
    #[cfg(windows)]
    fn fake_mkcert(dir: &Path, caroot: &Path) -> PathBuf {
        use devlanes_testing::fake_tool::{Behaviour, install};
        install(
            dir,
            "mkcert",
            &Behaviour::new().stdout(format!("{}\n", caroot.display())),
        )
        .expect("install fake-tool")
    }

    #[tokio::test]
    /// With `tls.ca: mkcert` the daemon loads mkcert's root, points the cert env at
    /// it and still writes the bundle under `$DEVLANES_HOME/ca`.
    async fn mkcert_ca_comes_from_caroot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let generated = tmp.path().join("generated");
        devlanes_pki::CertificateAuthority::load_or_create(&generated).expect("ca");
        let caroot = tmp.path().join("mkcert-root");
        std::fs::create_dir(&caroot).expect("caroot");
        // Renames keep the key's private mode, which from_mkcert requires.
        std::fs::rename(generated.join("ca.pem"), caroot.join("rootCA.pem")).expect("cert");
        std::fs::rename(generated.join("ca-key.pem"), caroot.join("rootCA-key.pem")).expect("key");
        let program = fake_mkcert(tmp.path(), &caroot);
        let home = tmp.path().join("home");

        let (env, status, ca) =
            certificate_authority_with(&home, devlanes_config::CaKind::Mkcert, &program, None)
                .await;
        assert!(ca.is_some(), "{status}");
        assert_eq!(status["kind"], "mkcert");
        assert_eq!(
            env[CA_CERT_ENV],
            caroot.join("rootCA.pem").display().to_string()
        );
        let bundle = home.join("ca").join("bundle.pem");
        assert_eq!(env[CA_BUNDLE_ENV], bundle.display().to_string());
        let root = std::fs::read_to_string(caroot.join("rootCA.pem")).expect("root");
        let bundled = std::fs::read_to_string(bundle).expect("bundle");
        assert!(
            bundled.contains(root.trim()),
            "bundle carries the mkcert root"
        );
        assert!(
            !home.join("ca/ca.pem").exists(),
            "no devlanes CA is created"
        );
    }

    #[tokio::test]
    /// A new devlanes CA is created with name constraints, so `doctor` has nothing to flag.
    async fn new_devlanes_ca_is_name_constrained() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir(&home).expect("home");
        let (_, status, ca) = certificate_authority_with(
            &home,
            devlanes_config::CaKind::Devlanes,
            Path::new("mkcert"),
            Some("dev.internal"),
        )
        .await;
        assert!(ca.is_some(), "{status}");
        assert_eq!(status["name_constrained"], true, "{status}");
        assert!(status["hint"].is_null() && status["fix"].is_null());
    }

    #[cfg(windows)]
    #[tokio::test]
    /// On Windows bundle.pem holds Mozilla's roots and the Windows store's,
    /// every block a certificate, and the devlanes CA exactly once, last; the
    /// CA directory is private.
    async fn windows_bundle_has_public_roots_and_the_devlanes_ca() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir(&home).expect("home");
        let (env, status, ca) = certificate_authority_with(
            &home,
            devlanes_config::CaKind::Devlanes,
            Path::new("mkcert"),
            None,
        )
        .await;
        let ca = ca.expect("devlanes CA");
        assert_eq!(status["present"], true, "{status}");
        let bundle = std::fs::read_to_string(&env[CA_BUNDLE_ENV]).expect("bundle");
        let ders: Vec<Vec<u8>> = bundle
            .split_inclusive("-----END CERTIFICATE-----\n")
            .filter(|block| !block.trim().is_empty())
            .map(|block| {
                devlanes_trust::CaCertificate::from_pem(block, "root")
                    .expect("PEM certificate")
                    .der()
                    .to_vec()
            })
            .collect();
        assert!(ders.len() > 100, "only {} certificates", ders.len());
        let own = ca.certificate_der().to_vec();
        assert_eq!(ders.iter().filter(|der| **der == own).count(), 1);
        assert_eq!(ders.last(), Some(&own));
        assert_eq!(
            devlanes_sys_windows::security::check_private_dir(&home.join("ca")).expect("check"),
            Ok(())
        );
    }

    #[tokio::test]
    /// A missing `mkcert` leaves HTTPS off with a `doctor` error naming the fix.
    async fn missing_mkcert_is_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (env, status, ca) = certificate_authority_with(
            tmp.path(),
            devlanes_config::CaKind::Mkcert,
            &tmp.path().join("no-such-mkcert"),
            None,
        )
        .await;
        assert!(ca.is_none() && env.is_empty());
        assert_eq!(status["present"], false);
        assert_eq!(status["error"], "mkcert_missing");
        assert!(
            status["message"]
                .as_str()
                .is_some_and(|m| m.contains("tls.ca"))
        );

        // An empty CAROOT is an invalid CA, not a missing program.
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).expect("empty");
        let program = fake_mkcert(tmp.path(), &empty);
        let (_, status, ca) =
            certificate_authority_with(tmp.path(), devlanes_config::CaKind::Mkcert, &program, None)
                .await;
        assert!(ca.is_none());
        assert_eq!(status["error"], "ca_invalid");

        // A relative CAROOT would resolve against the daemon's working directory.
        let relative = tmp.path().join("relative");
        std::fs::create_dir(&relative).expect("dir");
        let program = fake_mkcert(&relative, Path::new("mkcert-root"));
        let (_, status, ca) =
            certificate_authority_with(tmp.path(), devlanes_config::CaKind::Mkcert, &program, None)
                .await;
        assert!(ca.is_none());
        assert_eq!(status["error"], "mkcert_failed");
        assert!(
            status["message"]
                .as_str()
                .is_some_and(|m| m.contains("relative")),
            "{status}"
        );
    }

    /// An empty one-page SQLite database whose header carries `user_version`.
    fn legacy_database(user_version: u32) -> Vec<u8> {
        let mut page = vec![0u8; 4096];
        page[..16].copy_from_slice(b"SQLite format 3\0");
        page[16..18].copy_from_slice(&4096u16.to_be_bytes());
        page[18..24].copy_from_slice(&[1, 1, 0, 64, 32, 32]);
        page[24..28].copy_from_slice(&1u32.to_be_bytes()); // change counter
        page[28..32].copy_from_slice(&1u32.to_be_bytes()); // pages
        page[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format
        page[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8
        page[60..64].copy_from_slice(&user_version.to_be_bytes());
        page[92..96].copy_from_slice(&1u32.to_be_bytes()); // valid for counter 1
        page[96..100].copy_from_slice(&3_045_000u32.to_be_bytes());
        // Empty sqlite_schema: a table leaf with no cells, content at the page end.
        page[100] = 0x0d;
        page[105..107].copy_from_slice(&4096u16.to_be_bytes());
        page
    }

    #[tokio::test]
    /// A pre-Diesel state.db fails startup with the stable code and the reset
    /// command on the first line, which the spawning CLI shows.
    async fn legacy_state_names_the_reset_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("state.db");
        std::fs::write(&db, legacy_database(4)).expect("db");
        let error = open_store(&db).await.err().expect("legacy schema refused");
        let message = error.to_string();
        let first = message.lines().next().unwrap_or_default();
        assert!(first.starts_with("STATE_LEGACY_SCHEMA:"), "{message}");
        assert!(
            first.contains("devlanes daemon reset-state --yes"),
            "{message}"
        );
        assert!(
            (first.len() as u64) < devlanes_client::READY_LINE_LIMIT,
            "fits the readiness line"
        );
        assert!(message.contains(&db.display().to_string()));
    }

    #[tokio::test]
    /// A missing user config means defaults; an invalid one stops the daemon with
    /// one coded line that the spawning CLI turns into the same `config_error` a
    /// strict load gives (file, line, column, whole message), instead of running
    /// on defaults.
    async fn invalid_user_config_refuses_to_start() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Temp folders are short on Linux (`/tmp/.tmpXXXX`); a long home makes the
        // line longer than the old 126-byte cut on every platform.
        let home = tmp
            .path()
            .join("a-devlanes-home-long-enough-to-pass-the-old-readiness-cut");
        std::fs::create_dir(&home).expect("home");
        let user = load_user_config(&home).await.expect("no file, defaults");
        assert_eq!(
            user.proxy.https_port,
            devlanes_config::UserConfig::default().proxy.https_port
        );
        let path = home.join("config.yaml");
        std::fs::write(
            &path,
            "proxy:\n  http_port: [not, a, port]\n  https_port: 9443\n",
        )
        .expect("config");
        let message = load_user_config(&home)
            .await
            .expect_err("invalid config refused")
            .to_string();
        assert!(message.starts_with("USER_CONFIG_INVALID: "), "{message}");
        assert!(!message.contains('\n'), "one readiness line: {message}");
        // Longer than the old 126-byte cut, still whole within the client's bound.
        assert!(message.len() > 126, "{message}");
        assert!(
            (message.len() as u64) < devlanes_client::READY_LINE_LIMIT,
            "{message}"
        );
        // What the spawning client turns the line into.
        let error = devlanes_client::user_config_startup_error(&message).expect("parsed line");
        let paths = Paths::new(&home).expect("paths");
        let strict = crate::user_config::load(&paths)
            .await
            .expect_err("strict load refuses it too");
        assert_eq!(error, strict, "{message}");
        assert_eq!((error.code.as_str(), error.exit_code), ("config_error", 1));
        assert_eq!(error.data["file"], path.display().to_string());
        assert_eq!(error.data["line"], 2);
        assert!(
            error.data["column"].as_u64().is_some_and(|c| c > 0),
            "{error:?}"
        );
    }

    #[test]
    /// Doctor's proxy facts follow the accept loops: serving only while every
    /// listener of a kind runs without a pending accept error; never when unbound.
    fn proxy_status_reflects_listener_health() {
        let bound = json!({
            "http": {"bound": true, "port": 8080},
            "https": {"bound": false, "reason": "no_ca"},
        });
        let listener =
            |port: u16, running: bool, error: Option<&str>| devlanes_proxy::ListenerHealth {
                address: ([127, 0, 0, 1], port).into(),
                tls: false,
                running,
                accept_errors: u64::from(error.is_some()),
                last_error: error.map(str::to_owned),
            };
        let healthy = listener_status(&bound, &[listener(8080, true, None)]);
        assert_eq!(healthy["http"]["serving"], true);
        assert_eq!(healthy["http"]["listeners"][0]["address"], "127.0.0.1:8080");
        assert_eq!(healthy["https"]["serving"], false);
        assert_eq!(healthy["https"]["reason"], "no_ca");

        let failing = listener_status(
            &bound,
            &[
                listener(8080, true, None),
                listener(8080, true, Some("Too many open files")),
            ],
        );
        assert_eq!(failing["http"]["serving"], false);
        assert_eq!(failing["http"]["listeners"][1]["accept_errors"], 1);
        let stopped = listener_status(&bound, &[listener(8080, false, None)]);
        assert_eq!(stopped["http"]["serving"], false);
        assert_eq!(listener_status(&bound, &[])["http"]["serving"], false);
    }

    #[tokio::test]
    /// The offline reset refuses while a daemon holds the lock, then moves the
    /// legacy database and its sidecars aside (never deleting) so a fresh store
    /// opens, and the project data folders with them, leaving whatever else the
    /// data folder holds.
    async fn offline_reset_moves_a_legacy_database_aside() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            // The daemon lock only accepts a private home, like $DEVLANES_HOME.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&home)
                .expect("home");
        }
        #[cfg(windows)]
        std::fs::create_dir(&home).expect("home");
        let db = home.join("state.db");
        let legacy = legacy_database(4);
        std::fs::write(&db, &legacy).expect("db");
        assert!(open_store(&db).await.is_err());
        // Written after the failed open, which discards an invalid WAL.
        std::fs::write(home.join("state.db-wal"), b"wal").expect("wal");
        assert_eq!(crate::daemon::state_files(&db).len(), 2);
        // `lanes.data_dir` may name a folder that also holds other things.
        let data = tmp.path().join("lanes");
        for folder in ["acme/1", "acme/3", "photos/beach", "empty"] {
            std::fs::create_dir_all(data.join(folder)).expect("folder");
        }
        std::fs::write(data.join("acme/1/dump.sql"), "rows").expect("lane file");
        std::fs::write(data.join("notes.txt"), "notes").expect("file");

        let daemon = devlanes_daemon::lock_singleton(&home).expect("daemon lock");
        let refused = crate::daemon::move_state_aside(&home, &db, &data, "20261021T150405Z")
            .expect_err("a running daemon keeps its state");
        assert_eq!(refused.code, "daemon_running");
        assert!(db.exists(), "nothing moved");
        assert!(data.join("acme/1").exists(), "nothing moved");
        drop(daemon);

        let moved =
            crate::daemon::move_state_aside(&home, &db, &data, "20261021T150405Z").expect("moved");
        let aside = home.join("state.db.legacy-20261021T150405Z");
        let data_aside = data.join(".legacy-20261021T150405Z");
        assert_eq!(
            moved,
            [
                (db.clone(), aside.clone()),
                (
                    home.join("state.db-wal"),
                    home.join("state.db.legacy-20261021T150405Z-wal")
                ),
                (data.join("acme"), data_aside.join("acme")),
            ]
        );
        assert_eq!(std::fs::read(&aside).expect("kept"), legacy);
        assert!(crate::daemon::state_files(&db).is_empty());
        assert_eq!(
            std::fs::read_to_string(data_aside.join("acme/1/dump.sql")).expect("kept"),
            "rows"
        );
        assert!(data_aside.join("acme/3").is_dir());
        for kept in ["photos/beach", "empty", "notes.txt"] {
            assert!(data.join(kept).exists(), "{kept} stays");
        }

        // A second reset in the same second must not overwrite the first.
        std::fs::write(&db, &legacy).expect("db again");
        let clash = crate::daemon::move_state_aside(&home, &db, &data, "20261021T150405Z")
            .expect_err("target exists");
        assert!(db.exists() && aside.exists());
        assert!(
            clash.data["message"]
                .as_str()
                .is_some_and(|m| m.contains("already exists"))
        );
        std::fs::remove_file(&db).expect("cleanup");

        let fresh = open_store(&db).await.expect("fresh store after the reset");
        drop(fresh);
    }
}
