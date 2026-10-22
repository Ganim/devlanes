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
#[cfg(unix)]
use devlanes_daemon::integrations::{
    ConnectOptions, RemoteConnector, RemotePrompt, RemotePrompter,
};
use devlanes_daemon::{
    Engine, EngineOptions, Handler,
    integrations::{
        InfraContainer, InfraError, InfraEvent, InfraPortNeed, InfraProvider, InfraSpec, InfraUp,
        Integrations, LaunchCheck, ManagedUnit, PlannedUnit, PortInspector, PortScope, PresetEntry,
        RouteEntry, RouteSink, RouteStatus, UnitRunner, unavailable,
    },
};
use devlanes_proto::RpcError;
use devlanes_sys::NoWindow;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Terminal launchers for "open in terminal"; the TUI picks one by the user config name.
#[cfg(unix)]
fn launchers() -> Vec<Arc<dyn devlanes_term::TerminalLauncher>> {
    vec![
        Arc::new(devlanes_term_iterm2::Iterm2),
        Arc::new(devlanes_term_ghostty::Ghostty),
        Arc::new(devlanes_term_wezterm::Wezterm),
        Arc::new(devlanes_term_kitty::Kitty),
        Arc::new(devlanes_term_terminal_app::TerminalApp),
    ]
}

/// Terminal launchers for "open in terminal"; the TUI picks one by the user
/// config name, else the first installed, so Windows Terminal comes first.
/// iTerm2, Terminal.app, kitty and Ghostty have no Windows version.
#[cfg(windows)]
fn launchers() -> Vec<Arc<dyn devlanes_term::TerminalLauncher>> {
    vec![
        Arc::new(devlanes_term_windows_terminal::WindowsTerminal),
        Arc::new(devlanes_term_wezterm::Wezterm),
    ]
}

/// The user's preferred terminal from `~/.devlanes/config.yaml`, if it parses.
fn preferred_terminal(paths: &Paths) -> Option<String> {
    let config = devlanes_config::load_user(paths.home.join("config.yaml")).ok()?;
    let value = serde_json::to_value(config.terminal?.app()?).ok()?;
    value.as_str().map(str::to_owned)
}

/// The TUI's launchers, terminal app, progress bar, colors and glyphs from the
/// user config.
pub fn tui_options(paths: &Paths) -> devlanes_tui::Options {
    use std::io::IsTerminal;
    // A config that does not load leaves the TUI on its detected defaults.
    let tui = devlanes_config::load_user(paths.home.join("config.yaml"))
        .map(|config| config.tui)
        .unwrap_or_default();
    devlanes_tui::Options {
        launchers: launchers(),
        terminal: preferred_terminal(paths),
        // The TUI is always human output; it still needs a terminal that draws the bar.
        progress: devlanes_term::progress_support_from_env(
            crate::progress::configured_mode(paths),
            std::io::stdout().is_terminal(),
        ),
        theme: Some(tui.theme.as_str().to_owned()),
        glyphs: Some(tui.glyphs.as_str().to_owned()),
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
    devlanes_tui::run_with(paths.clone(), tui_options(paths))
        .await
        .map(|()| 0)
        .map_err(tui_error)
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
    let (user, user_config_errors) = load_user_config(&paths.home).await;
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
    let routes = Arc::new(proxy(&user, ca, shutdown.clone()).await);
    let infra = Arc::new(ContainerInfra::new().await);
    let mut integ = Integrations::with_runner(unit_runner());
    integ.contributors = contributors();
    integ.script_runners = script_runners();
    integ.secrets = Some(Arc::new(devlanes_secrets_fnox::FnoxProvider::default()));
    integ.metrics = metrics_sources(infra.docker.as_ref());
    integ.infra = infra;
    integ.routes = routes;
    integ.ports = Arc::new(SysPorts);
    integ.processes = crate::orphans::process_table();
    integ.shell = devlanes_daemon::integrations::detect_shell().await;
    integ.ca_env = ca_env;
    integ.ca_status = ca_status;
    integ.ca_permitted = ca_permitted;
    integ.presets = presets();
    integ.launch_checks = vec![Arc::new(DistinctTauriIdentifiers)];
    // Windows keeps the default connector, which refuses with `not_supported_yet`
    // until the SSH transport has a Windows mode.
    #[cfg(unix)]
    {
        integ.remote = Arc::new(SshConnector {
            home: paths.home.clone(),
            exe: std::env::current_exe().ok(),
        });
    }
    integ.hosts = hosts_file(hosts_policy, hosts_tld, hosts_probe)
        .await
        .map(Arc::new);
    integ.pressure = pressure_source();
    integ.system_pressure = pressure_source();
    integ.start_gate = Some(start_gate(&user.startup, integ.pressure.clone()));
    integ.resolver_files = Some(Arc::new(OsResolverFiles));
    let mut options = EngineOptions::new(&paths.home, &paths.socket);
    options.db_path = paths.db.clone();
    options.logs = paths.logs.clone();
    options.user = user;
    options.user_config_errors = user_config_errors;
    let engine: Arc<dyn Handler> = Arc::new(Engine::new(options, store, integ, shutdown));
    Ok(engine)
}

/// Bound on the startup `*.localhost` probe; past it the resolver counts as failing.
const HOSTS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

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

/// The hosts file fallback through the `devlanes-hosts` Windows service. It
/// counts as installed when anything answers its pipe other than "no such pipe",
/// as the helper counts as installed when its file exists; a refusal or an old
/// protocol is then reported by the first update.
#[cfg(windows)]
async fn hosts_file(
    policy: devlanes_resolver_hosts::HostsFilePolicy,
    tld: Option<String>,
    probe: Option<tokio::task::JoinHandle<devlanes_resolver_hosts::LocalhostResolution>>,
) -> Option<devlanes_daemon::hosts::HostsFile> {
    use devlanes_resolver_hosts::pipe::PipeHostsClient;
    use devlanes_resolver_hosts::{HostsClient, LocalhostResolution};
    let resolution = match probe {
        // A panicked probe proves nothing about the resolver; treat it like a failure.
        Some(probe) => Some(probe.await.unwrap_or(LocalhostResolution::Fails)),
        None => None,
    };
    tracing::info!(mode = %policy.user_mode(), ?resolution, "hosts file policy");
    let pipe = PipeHostsClient::system();
    let (client, tld_error) = match HostsClient::service(pipe.clone(), tld.as_deref()) {
        Ok(client) => (client, None),
        Err(error) => match HostsClient::service(pipe, None) {
            Ok(client) => (client, Some(error)),
            Err(error) => {
                tracing::warn!(%error, "hosts file fallback unavailable");
                return None;
            }
        },
    };
    let check = client.clone().with_timeout(HOSTS_SERVICE_CHECK_TIMEOUT);
    let installed = !matches!(check.version().await, Err(error) if error.is_missing());
    Some(devlanes_daemon::hosts::HostsFile::spawn(
        client,
        devlanes_daemon::hosts::HostsOptions {
            policy,
            resolution,
            installed,
            tld_error,
            sync: devlanes_resolver_hosts::SyncConfig::default(),
            // Shipped together: the service next to this binary has this version.
            expected_version: env!("CARGO_PKG_VERSION").into(),
        },
    ))
}

/// Bound on the startup question to the hosts service; it answers in
/// milliseconds when it runs.
#[cfg(windows)]
const HOSTS_SERVICE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// `~/.devlanes/config.yaml`, or defaults when absent or invalid (logged).
async fn load_user_config(home: &Path) -> (devlanes_config::UserConfig, Vec<Value>) {
    let path = home.join("config.yaml");
    tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return (devlanes_config::UserConfig::default(), Vec::new());
        }
        // A bad key falls back to its default alone; `daemon status` and doctor
        // list what was left out.
        let (config, errors) = devlanes_config::load_user_lenient(&path);
        let located: Vec<Value> = errors
            .iter()
            .map(|error| {
                tracing::warn!(error = %error.message, "user config setting ignored");
                json!({
                    "message": error.message,
                    "file": error.location.file,
                    "line": error.location.line,
                    "column": error.location.column,
                })
            })
            .collect();
        (config, located)
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

/// Process collectors for this OS plus Docker container stats. Container stats share
/// the infra client, so they reach the engine `DOCKER_HOST` or the Docker context
/// selects; without that client there is no engine to sample.
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
        None => tracing::debug!("container metrics unavailable: no Docker client"),
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

/// Checks the file (the NRPT rule on Windows) `devlanes certs trust --resolver` writes.
struct OsResolverFiles;

#[cfg(unix)]
impl devlanes_daemon::integrations::ResolverFiles for OsResolverFiles {
    /// `{path, installed, matches}`; `matches` is false when the file routes the TLD
    /// elsewhere (another port, a stale install), `{error}` when the OS has no form.
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
}

#[cfg(windows)]
impl devlanes_daemon::integrations::ResolverFiles for OsResolverFiles {
    /// `{namespace, server, installed, matches, foreign, group_policy}` for the
    /// NRPT rule; `foreign` when another program's rule routes the namespace,
    /// `group_policy` when Group Policy rules make Windows ignore local ones.
    fn status(&self, tld: &str, server: std::net::SocketAddr) -> Value {
        crate::resolver_nrpt::status(tld, server)
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
                .no_window()
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
        if !directory.exists() {
            create_private_dir(&directory)?;
        }
        let system = system_roots(ca.certificate_der().as_ref());
        let bundle = write_bundle(&directory, &ca.combined_bundle(&system))?;
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

/// Writes `contents` to `bundle.pem` in `directory` through a temporary file renamed
/// into place, and returns the bundle's path. Blocking.
///
/// When the rename fails and an older bundle exists, that bundle is kept with a
/// warning: units only read it at start, so a stale bundle beats turning HTTPS off.
/// On Windows this happens while a program holds the old file open without delete
/// sharing (most do); on Unix a rename over an open file succeeds.
fn write_bundle(directory: &Path, contents: &str) -> std::io::Result<PathBuf> {
    let bundle = directory.join("bundle.pem");
    let temp = directory.join(".bundle.pem.tmp");
    std::fs::write(&temp, contents)?;
    if let Err(error) = std::fs::rename(&temp, &bundle) {
        // The temporary file is rewritten next time, so a failed cleanup is harmless.
        let _ = std::fs::remove_file(&temp);
        if !bundle.is_file() {
            return Err(error);
        }
        tracing::warn!(%error, path = %bundle.display(), "cannot replace bundle.pem; keeping the previous one");
    }
    Ok(bundle)
}

/// The system's PEM roots for `bundle.pem`, from the first bundle file the OS
/// ships; empty when there is none. Blocking.
#[cfg(unix)]
fn system_roots(_own: &[u8]) -> String {
    [
        "/etc/ssl/cert.pem",
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
    ]
    .iter()
    .find_map(|path| std::fs::read_to_string(path).ok())
    .unwrap_or_default()
}

/// The system's roots for `bundle.pem` as PEM: Mozilla's public roots (built in)
/// plus the current user's `Root` store, which also lists the machine's roots and
/// so carries enterprise and custom roots. Blocking.
///
/// Windows fetches most public roots on first use (automatic root update), so the
/// store alone lacks many of the roots Python's certifi has, and units pointed at
/// `bundle.pem` through `SSL_CERT_FILE` would lose them. Store roots are filtered
/// like rustls-native-certs 0.8 does (valid now, usable for server
/// authentication); a root the store lists but fails that filter (purposes
/// disabled, expired) is left out even when Mozilla's set has it, and so is any
/// certificate in the user's `Disallowed` store. A store that cannot be opened
/// leaves the other source. Roots carrying a "disallowed after" date
/// (`CERT_DISALLOWED_FILETIME_PROP_ID`) are not detected: schannel has no reader
/// for that property.
#[cfg(windows)]
fn system_roots(own: &[u8]) -> String {
    use schannel::cert_context::{HashAlgorithm, ValidUses};
    /// The server authentication extended key usage.
    const SERVER_AUTH: &str = "1.3.6.1.5.5.7.3.1";
    let open = |name: &str| {
        schannel::cert_store::CertStoreBuilder::new()
            .read_only()
            .open_current_user(name)
    };
    let store = match open("Root") {
        Ok(store) => store
            .certs()
            .map(|cert| {
                let server_auth = match cert.valid_uses() {
                    Ok(ValidUses::All) => true,
                    Ok(ValidUses::Oids(oids)) => oids.iter().any(|oid| oid == SERVER_AUTH),
                    Err(_) => false,
                };
                let usable = server_auth && cert.is_time_valid().unwrap_or(false);
                (cert.to_der().to_vec(), usable)
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "cannot read the Windows root store; bundle.pem has Mozilla's roots only");
            Vec::new()
        }
    };
    let disallowed: std::collections::HashSet<Vec<u8>> = match open("Disallowed") {
        Ok(store) => store
            .certs()
            .filter_map(|cert| cert.fingerprint(HashAlgorithm::sha1()).ok())
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "cannot read the Windows Disallowed store; distrusted roots are not filtered");
            std::collections::HashSet::new()
        }
    };
    // A certificate that cannot be hashed cannot be checked, so it is left out.
    let distrusted = |der: &[u8]| {
        schannel::cert_context::CertContext::new(der)
            .and_then(|cert| cert.fingerprint(HashAlgorithm::sha1()))
            .ok()
            .is_none_or(|sha1| disallowed.contains(&sha1))
    };
    let builtin = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| cert.to_vec());
    union_roots(builtin, store, distrusted, own)
}

/// The bundle's roots as PEM: `builtin` and the usable `store` roots (DER, usable
/// flag), each once, without `own`, store roots that are not usable and anything
/// `distrusted` accepts.
#[cfg(windows)]
fn union_roots(
    builtin: impl IntoIterator<Item = Vec<u8>>,
    store: Vec<(Vec<u8>, bool)>,
    distrusted: impl Fn(&[u8]) -> bool,
    own: &[u8],
) -> String {
    let refused: std::collections::HashSet<&[u8]> = store
        .iter()
        .filter(|(_, usable)| !usable)
        .map(|(der, _)| der.as_slice())
        .collect();
    let candidates: Vec<Vec<u8>> = builtin
        .into_iter()
        .chain(
            store
                .iter()
                .filter(|(_, usable)| *usable)
                .map(|(der, _)| der.clone()),
        )
        .filter(|der| !refused.contains(der.as_slice()) && !distrusted(der))
        .collect();
    roots_pem(candidates, own)
}

/// `roots` as PEM blocks, each certificate once and without `own`.
#[cfg(windows)]
fn roots_pem(roots: impl IntoIterator<Item = Vec<u8>>, own: &[u8]) -> String {
    let mut seen = std::collections::HashSet::from([own.to_vec()]);
    roots
        .into_iter()
        .filter(|der| seen.insert(der.clone()))
        .map(|der| pem_certificate(&der))
        .collect()
}

/// One `CERTIFICATE` PEM block with 64-character lines (RFC 7468).
#[cfg(windows)]
fn pem_certificate(der: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    // Base64 is ASCII, so byte chunks are whole characters.
    for line in encoded.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

/// Creates the CA directory readable by the owner only.
#[cfg(unix)]
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
}

/// Creates the CA directory. It inherits the private access list the daemon gives
/// the devlanes home before it loads the CA.
#[cfg(windows)]
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)
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
        Some(_) => match devlanes_proxy::bind_listeners(
            user.proxy.https_port,
            devlanes_config::ProxyConfig::HTTPS_FALLBACK_PORT,
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
        devlanes_config::ProxyConfig::HTTP_FALLBACK_PORT,
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
#[cfg(unix)]
struct SshConnector {
    /// `$DEVLANES_HOME`; control sockets live in its `run/ssh`.
    home: PathBuf,
    /// This binary, which ssh runs as `askpass`; without it ssh runs in batch mode.
    exe: Option<PathBuf>,
}

#[cfg(unix)]
impl RemoteConnector for SshConnector {
    /// One SSH transport (and ControlMaster) per box.
    fn transport(
        &self,
        host: &str,
        prompts: Arc<dyn RemotePrompter>,
        connect: ConnectOptions,
    ) -> Result<Arc<dyn devlanes_transport::Transport>, RpcError> {
        let mut options = devlanes_transport_ssh::SshOptions::new(&self.home);
        // A box without devlanes gets this client's own release, and only with
        // `remote connect --install`; otherwise the plan is reported.
        options.manifest_url = Some(format!(
            "{}/releases/download/v{}/release-manifest.json",
            env!("CARGO_PKG_REPOSITORY"),
            env!("CARGO_PKG_VERSION")
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
#[cfg(unix)]
struct PromptBridge(Arc<dyn RemotePrompter>);

#[cfg(unix)]
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

/// The unit runner the daemon spawns services and tasks with.
#[cfg(unix)]
fn unit_runner() -> Arc<dyn UnitRunner> {
    Arc::new(OsUnits(OsRunner::default()))
}

/// The unit runner the daemon spawns services and tasks with. Graceful stops
/// send console control events through this binary as a helper
/// (`devlanes __console-ctrl`); without it units are terminated at once.
#[cfg(windows)]
fn unit_runner() -> Arc<dyn UnitRunner> {
    let runner = match devlanes_runner_windows::ConsoleHelper::current_exe() {
        Ok(helper) => OsRunner::default().with_console_helper(helper),
        Err(error) => {
            tracing::warn!(%error, "no console helper; units are terminated without a graceful stop");
            OsRunner::default()
        }
    };
    Arc::new(OsUnits(runner))
}

/// The PTY runner: sessions, PTYs and signals.
#[cfg(unix)]
use devlanes_runner_pty::{PtyRunner as OsRunner, PtyUnit as OsUnit};
/// The Windows runner: job objects and pipes.
#[cfg(windows)]
use devlanes_runner_windows::{WindowsRunner as OsRunner, WindowsUnit as OsUnit};

/// The platform's unit runner with identity and socket inspection. Both runners
/// offer the same methods, so the wrappers below are shared.
struct OsUnits(OsRunner);

#[async_trait]
impl UnitRunner for OsUnits {
    /// Spawns per the spec.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        Ok(Arc::new(OsManaged(self.0.spawn_unit(spec).await?)))
    }

    /// Tasks keep daemons they started outside their session (Unix) or job
    /// (Windows) running after they exit.
    async fn spawn_task(
        &self,
        spec: LaunchSpec,
    ) -> Result<Arc<dyn ManagedUnit>, devlanes_core::Error> {
        let runner = self
            .0
            .clone()
            .with_exit_sweep(devlanes_core::ExitSweep::KeepDaemonized);
        Ok(Arc::new(OsManaged(runner.spawn_unit(spec).await?)))
    }
}

/// The platform runner's unit as a daemon unit.
struct OsManaged(Arc<OsUnit>);

#[async_trait]
impl Unit for OsManaged {
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
impl ManagedUnit for OsManaged {
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

    /// Scans listening sockets of all visible processes but this one.
    async fn wildcard_holder(&self, port: u16) -> Option<u32> {
        tokio::task::spawn_blocking(move || {
            let pids: Vec<u32> = devlanes_sys::processes()
                .ok()?
                .iter()
                .map(|p| p.pid)
                .collect();
            let sockets = devlanes_sys::listening_tcp(&pids).ok()?;
            wildcard_listener(&sockets, port, std::process::id())
        })
        .await
        .ok()
        .flatten()
    }
}

/// The first of `sockets` on `port` at a wildcard address, owned by a process other
/// than `own`. Either family counts: a dual-stack `::` listener also takes IPv4.
fn wildcard_listener(
    sockets: &[devlanes_sys::ListeningSocket],
    port: u16,
    own: u32,
) -> Option<u32> {
    sockets
        .iter()
        .find(|socket| {
            socket.addr.port() == port && socket.addr.ip().is_unspecified() && socket.pid != own
        })
        .map(|socket| socket.pid)
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
/// may carry a full shell environment; none of the rest reaches Compose. Windows
/// also keeps the variables Windows programs need to start (a missing
/// `SystemRoot` breaks Winsock) and matches names ignoring case (`Path`).
fn compose_base_env(vars: impl Iterator<Item = (String, String)>) -> BTreeMap<String, String> {
    #[cfg(unix)]
    mod os {
        /// No extra variables on Unix.
        pub const NAMES: &[&str] = &[];

        /// Unix names are case-sensitive.
        pub fn same(name: &str, wanted: &str) -> bool {
            name == wanted
        }
    }
    #[cfg(windows)]
    mod os {
        /// What Windows processes and the docker CLI read on top of `PATH`.
        pub const NAMES: &[&str] = &[
            "SystemRoot",
            "SystemDrive",
            "windir",
            "ComSpec",
            "PATHEXT",
            "USERPROFILE",
            "USERNAME",
            "USERDOMAIN",
            "HOMEDRIVE",
            "HOMEPATH",
            "APPDATA",
            "LOCALAPPDATA",
            "ProgramData",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "CommonProgramFiles",
            "TEMP",
            "TMP",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
            "OS",
        ];

        /// Windows names ignore case.
        pub fn same(name: &str, wanted: &str) -> bool {
            name.eq_ignore_ascii_case(wanted)
        }
    }
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
    vars.filter(|(name, _)| {
        NAMES
            .iter()
            .chain(os::NAMES)
            .any(|wanted| os::same(name, wanted))
            || PREFIXES.iter().any(|prefix| {
                name.get(..prefix.len())
                    .is_some_and(|start| os::same(start, prefix))
            })
    })
    .collect()
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
        if error.is_unreachable() {
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

    /// The last `tail` stdout and stderr lines of a plain container.
    async fn container_logs(
        &self,
        spec: &InfraSpec,
        container: &str,
        tail: usize,
    ) -> Result<Vec<String>, InfraError> {
        use futures_util::StreamExt;
        let docker = self.docker()?;
        let declared = spec
            .containers
            .iter()
            .find(|c| c.name == container)
            .ok_or_else(|| {
                InfraError::new("not_found", format!("no container named {container}"))
            })?;
        let scope = if declared.per_lane {
            devlanes_infra_docker::Scope::Lane(spec.lane)
        } else {
            devlanes_infra_docker::Scope::Shared
        };
        let name = devlanes_infra_docker::container_name(&spec.project, container, scope);
        let options = bollard::query_parameters::LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .tail(&tail.to_string())
            .build();
        let mut stream = docker.client().logs(&name, Some(options));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                docker_error(&format!("logs of container {name}"))(
                    devlanes_infra_docker::Error::Docker(error),
                )
            })?;
            text.push_str(&String::from_utf8_lossy(&chunk.into_bytes()));
        }
        Ok(text.lines().map(str::to_owned).collect())
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
    /// An engine nobody serves is `docker_unavailable`, not a failed call.
    fn unreachable_engine_is_unavailable() {
        let refused = docker_error("list containers of acme")(devlanes_infra_docker::Error::Io(
            std::io::ErrorKind::ConnectionRefused.into(),
        ));
        assert_eq!(refused.code, "docker_unavailable");
        let missing =
            docker_error("list containers of acme")(devlanes_infra_docker::Error::Docker(
                bollard::errors::Error::SocketNotFoundError("/var/run/docker.sock".into()),
            ));
        assert_eq!(missing.code, "docker_unavailable");
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
    /// Windows keeps `Path` and the variables Windows programs need, matching
    /// names ignoring case.
    fn compose_base_env_keeps_windows_variables() {
        let vars = [
            ("Path", r"C:\Windows"),
            ("SystemRoot", r"C:\Windows"),
            ("ComSpec", r"C:\Windows\System32\cmd.exe"),
            ("USERPROFILE", r"C:\Users\acme"),
            ("LocalAppData", r"C:\Users\acme\AppData\Local"),
            ("Docker_Host", "npipe:////./pipe/docker_engine"),
            ("GITHUB_TOKEN", "secret"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let kept: Vec<String> = compose_base_env(vars.into_iter()).into_keys().collect();
        assert_eq!(
            kept,
            [
                "ComSpec",
                "Docker_Host",
                "LocalAppData",
                "Path",
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

    // Windows needs a fake mkcert that is not a shell script.
    #[cfg(unix)]
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
        let bundle = home.join("ca/bundle.pem");
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
        // Windows needs a fake mkcert that is not a shell script.
        #[cfg(unix)]
        unusable_mkcert_caroot_is_reported(&tmp).await;
    }

    /// A CAROOT that is empty or relative leaves HTTPS off with the matching error.
    #[cfg(unix)]
    async fn unusable_mkcert_caroot_is_reported(tmp: &tempfile::TempDir) {
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

    #[cfg(windows)]
    #[tokio::test]
    /// On Windows bundle.pem holds Mozilla's roots and the root store's
    /// certificates, a well-known public root among them, plus the devlanes root
    /// exactly once, and every block parses as a PEM certificate.
    async fn windows_bundle_has_system_roots_and_the_devlanes_root() {
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
        let blocks: Vec<&str> = bundle
            .split_inclusive("-----END CERTIFICATE-----\n")
            .filter(|block| !block.trim().is_empty())
            .collect();
        // Mozilla's set alone has well over a hundred roots; the Windows store
        // often lists fewer than that until roots are fetched on use.
        assert!(
            blocks.len() > webpki_root_certs::TLS_SERVER_ROOT_CERTS.len() / 2,
            "only {} certificates",
            blocks.len()
        );
        let ders: Vec<Vec<u8>> = blocks
            .iter()
            .map(|block| {
                devlanes_trust::CaCertificate::from_pem(block, "root")
                    .expect("PEM certificate")
                    .der()
                    .to_vec()
            })
            .collect();
        let own = ca.certificate_der();
        assert_eq!(
            ders.iter()
                .filter(|der| der.as_slice() == own.as_ref())
                .count(),
            1
        );
        // Subject names are plain ASCII inside the DER.
        let known = [
            "DigiCert",
            "ISRG Root",
            "GlobalSign",
            "USERTrust",
            "Microsoft",
        ];
        assert!(
            ders.iter().any(|der| known.iter().any(|name| der
                .windows(name.len())
                .any(|bytes| bytes == name.as_bytes()))),
            "no well-known root among {} certificates",
            ders.len()
        );
    }

    #[cfg(windows)]
    #[test]
    /// The store lists the devlanes root once `certs trust` added it, and a root
    /// can sit in both the user's and the machine's store: each ends up in the
    /// bundle once, the devlanes root only where `combined_bundle` appends it.
    fn windows_roots_skip_duplicates_and_the_devlanes_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ca =
            devlanes_pki::CertificateAuthority::load_or_create(&tmp.path().join("ca")).expect("ca");
        let own = ca.certificate_der().to_vec();
        let other = vec![0x30, 0x03, 0x02, 0x01, 0x01];
        let pem = roots_pem([other.clone(), own.clone(), other.clone()], &own);
        assert_eq!(pem, pem_certificate(&other));
        let parsed =
            devlanes_trust::CaCertificate::from_pem(&pem_certificate(&own), "root").expect("pem");
        assert_eq!(parsed.der(), own.as_slice());
        assert_eq!(parsed.pem(), pem_certificate(&own));
    }

    #[cfg(windows)]
    #[test]
    /// The bundle is the union of Mozilla's roots and the usable store roots, minus
    /// store roots Windows refuses (also when Mozilla has them) and distrusted ones.
    fn windows_roots_union_mozilla_and_the_store() {
        let cert = |n: u8| vec![0x30, 0x03, 0x02, 0x01, n];
        let (mozilla, both, disabled, enterprise, expired, distrusted, own) = (
            cert(1),
            cert(2),
            cert(3),
            cert(4),
            cert(5),
            cert(6),
            cert(7),
        );
        let pem = union_roots(
            [
                mozilla.clone(),
                both.clone(),
                disabled.clone(),
                distrusted.clone(),
            ],
            vec![
                (both.clone(), true),
                (disabled.clone(), false),
                (enterprise.clone(), true),
                (expired.clone(), false),
                (distrusted.clone(), true),
                (own.clone(), true),
            ],
            |der| der == distrusted.as_slice(),
            &own,
        );
        let expected: String = [mozilla, both, enterprise]
            .iter()
            .map(|der| pem_certificate(der))
            .collect();
        assert_eq!(pem, expected);
    }

    #[cfg(windows)]
    #[test]
    /// Mozilla's set reaches the bundle through the real filter: the Disallowed
    /// store does not hold ISRG Root X1, so it is never left out.
    fn windows_system_roots_include_mozilla_roots() {
        let pem = system_roots(&[]);
        let isrg = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .find(|cert| {
                let name = b"ISRG Root X1";
                cert.windows(name.len()).any(|bytes| bytes == name)
            })
            .expect("ISRG Root X1 in Mozilla's set");
        assert!(pem.contains(&pem_certificate(isrg)));
    }

    #[test]
    /// A mirror is shadowed only by another process's wildcard listener on the
    /// same port, in either family; loopback listeners and the daemon's own are not.
    fn wildcard_listener_is_another_process_on_the_same_port() {
        let socket = |pid: u32, addr: &str| devlanes_sys::ListeningSocket {
            pid,
            addr: addr.parse().expect("address"),
        };
        let own = 10;
        let sockets = [
            socket(own, "0.0.0.0:5432"),
            socket(20, "127.0.0.1:5432"),
            socket(30, "0.0.0.0:6379"),
            socket(40, "[::]:5432"),
        ];
        assert_eq!(wildcard_listener(&sockets, 5432, own), Some(40));
        assert_eq!(wildcard_listener(&sockets, 6379, own), Some(30));
        assert_eq!(wildcard_listener(&sockets[..2], 5432, own), None);
        assert_eq!(wildcard_listener(&sockets, 8080, own), None);
    }

    #[test]
    /// A bundle is written through a rename; one that cannot be replaced with no
    /// older bundle to keep is an error.
    fn bundle_is_written_or_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bundle = write_bundle(tmp.path(), "one").expect("write");
        assert_eq!(std::fs::read_to_string(&bundle).expect("read"), "one");
        let blocked = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(blocked.path().join("bundle.pem/inside")).expect("dir");
        assert!(write_bundle(blocked.path(), "two").is_err());
        assert!(!blocked.path().join(".bundle.pem.tmp").exists());
    }

    #[cfg(windows)]
    #[test]
    /// A bundle held open without delete sharing (what Python's `open` does) cannot
    /// be replaced; the old one stays and the CA still loads.
    fn windows_open_bundle_is_kept() {
        use std::os::windows::fs::OpenOptionsExt;
        /// `FILE_SHARE_READ`: readers may share, renames over the file may not.
        const FILE_SHARE_READ: u32 = 1;
        let tmp = tempfile::tempdir().expect("tempdir");
        let bundle = write_bundle(tmp.path(), "old").expect("write");
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&bundle)
            .expect("open");
        assert_eq!(write_bundle(tmp.path(), "new").expect("kept"), bundle);
        assert_eq!(std::fs::read_to_string(&bundle).expect("read"), "old");
        assert!(!tmp.path().join(".bundle.pem.tmp").exists());
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
        assert!(first.len() < 127, "fits the readiness line");
        assert!(message.contains(&db.display().to_string()));
    }

    #[tokio::test]
    /// A bad user config key falls back to its default alone and keeps the
    /// located error for `daemon status` and `doctor`; a missing file is no error.
    async fn invalid_user_config_is_kept_for_doctor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (_, errors) = load_user_config(tmp.path()).await;
        assert!(errors.is_empty(), "no file, no error");
        std::fs::write(
            tmp.path().join("config.yaml"),
            "proxy:\n  http_port: [not, a, port]\n  https_port: 9443\n",
        )
        .expect("config");
        let (user, errors) = load_user_config(tmp.path()).await;
        assert_eq!(user.proxy.https_port, 9443, "the valid key stays");
        let error = errors.first().expect("error kept");
        assert!(!error["message"].as_str().unwrap_or_default().is_empty());
        assert_eq!(error["line"], 2, "{error}");
        assert!(
            error["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("config.yaml"))
        );
        assert_eq!(
            user.proxy.http_port,
            devlanes_config::UserConfig::default().proxy.http_port
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
    /// legacy database and its sidecars aside (never deleting) so a fresh store opens.
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

        let daemon = devlanes_daemon::lock_singleton(&home).expect("daemon lock");
        let refused = crate::daemon::move_state_aside(&home, &db, "20261021T150405Z")
            .expect_err("a running daemon keeps its state");
        assert_eq!(refused.code, "daemon_running");
        assert!(db.exists(), "nothing moved");
        drop(daemon);

        let moved = crate::daemon::move_state_aside(&home, &db, "20261021T150405Z").expect("moved");
        let aside = home.join("state.db.legacy-20261021T150405Z");
        assert_eq!(
            moved,
            [
                (db.clone(), aside.clone()),
                (
                    home.join("state.db-wal"),
                    home.join("state.db.legacy-20261021T150405Z-wal")
                ),
            ]
        );
        assert_eq!(std::fs::read(&aside).expect("kept"), legacy);
        assert!(crate::daemon::state_files(&db).is_empty());

        // A second reset in the same second must not overwrite the first.
        std::fs::write(&db, &legacy).expect("db again");
        let clash = crate::daemon::move_state_aside(&home, &db, "20261021T150405Z")
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
