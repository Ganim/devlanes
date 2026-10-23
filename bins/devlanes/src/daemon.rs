//! Daemon process entry plus local actions that must run in the user's terminal.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use devlanes_cli::{Action, Runtime};
use devlanes_client::Paths;
use devlanes_daemon::{
    Options, Server,
    process::{Readiness, detach},
};
use devlanes_proto::RpcError;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Rotates once at startup; a dev daemon log never needs more than one old file.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// `devlanes daemon run`: detach, take the lock, reconcile, announce readiness, serve.
pub fn run() -> ExitCode {
    // Leave the spawning terminal's session before any thread exists.
    if let Err(error) = detach() {
        eprintln!("error: cannot detach daemon: {error}");
        return ExitCode::from(1);
    }
    let readiness = Readiness::from_env();
    let paths = match Paths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = paths.prepare() {
        eprintln!("error: {error}");
        return ExitCode::from(1);
    }
    let _log_guard = init_logging(&paths);
    // Panics would otherwise go to the detached stderr and leave no trace.
    devlanes_daemon::process::install_panic_hook();
    let tokio = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(tokio) => tokio,
        Err(error) => {
            tracing::error!(%error, "runtime");
            return ExitCode::from(1);
        }
    };
    let code = tokio.block_on(async move {
        let options = Options {
            home: paths.home.clone(),
            socket: paths.socket.clone(),
        };
        let server = match Server::bind(options).await {
            Ok(server) => server,
            Err(devlanes_daemon::Error::AlreadyRunning) => {
                // Losing the race is success for the client that spawned us.
                let _ = readiness.already_running();
                return 0;
            }
            Err(error) => {
                tracing::error!(%error, "bind failed");
                let message = match &error {
                    devlanes_daemon::Error::UnsafeDirectory(path) => {
                        let data = crate::driver::unsafe_directory(path).data;
                        match data.get("fix").and_then(serde_json::Value::as_str) {
                            Some(fix) => format!("{error}; fix: {fix}"),
                            None => error.to_string(),
                        }
                    }
                    _ => error.to_string(),
                };
                let _ = readiness.failed(&message);
                return 1;
            }
        };
        let handler = match crate::wiring::engine(&paths, server.shutdown_token()).await {
            Ok(handler) => handler,
            Err(error) => {
                tracing::error!(%error, "engine startup failed");
                let _ = readiness.failed(&error.to_string());
                return 1;
            }
        };
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
            "daemon started"
        );
        watch_signals(server.shutdown_token(), server.interrupt_token());
        // Taken by whichever comes first: readiness, or a failed reconcile.
        let readiness = std::sync::Mutex::new(Some(readiness));
        let take = || readiness.lock().ok().and_then(|mut slot| slot.take());
        let served = server
            .run(handler, || take().map_or(Ok(()), Readiness::ready))
            .await;
        match served {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!(%error, "daemon stopped with an error");
                if let (devlanes_daemon::Error::Reconcile(message), Some(readiness)) =
                    (&error, take())
                {
                    let _ = readiness.failed(message);
                }
                1
            }
        }
    });
    // Dropping the runtime waits for every spawn_blocking task (git, getaddrinfo, file
    // watchers), which can keep a stopped daemon alive indefinitely; bound the wait.
    tokio.shutdown_timeout(std::time::Duration::from_secs(5));
    ExitCode::from(code)
}

/// SIGTERM and SIGINT (a service manager, logout, `kill`) shut the daemon down
/// like `devlanes daemon stop` without its `lane.stop` tasks: units stop, exits
/// are recorded, state.db closes. A signal during an explicit shutdown
/// interrupts its `lane.stop` tasks (`interrupt`), so the units still stop
/// before the service manager's SIGKILL.
fn watch_signals(shutdown: CancellationToken, interrupt: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut int) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(term), Ok(int)) => (term, int),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "signal handlers not installed; signals end the daemon without a shutdown");
            return;
        }
    };
    // Runs until the process exits: once tokio handles a signal its default
    // action never comes back, so a later signal must still be acted on here.
    tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                Some(()) = term.recv() => "SIGTERM",
                Some(()) = int.recv() => "SIGINT",
                else => return,
            };
            if shutdown.is_cancelled() {
                tracing::info!(signal = name, "interrupting lane.stop tasks");
            } else {
                tracing::info!(signal = name, "shutting down");
            }
            interrupt.cancel();
            shutdown.cancel();
        }
    });
}

/// The daemon never prints to stdout; everything goes to `logs/daemon.log`, at
/// the level `DEVLANES_LOG` sets (RUST_LOG syntax, default `info`). Requests
/// from `-v` clients also get their own records through the diagnostics layer,
/// whatever `DEVLANES_LOG` says.
fn init_logging(paths: &Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
    let path = paths.logs.join("daemon.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, paths.logs.join("daemon.log.1"));
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    // Without the file, verbose clients still get their diagnostics.
    let (file, guard) = match opened {
        Ok(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let filter = tracing_subscriber::EnvFilter::try_from_env("DEVLANES_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_filter(filter);
            (Some(layer), Some(guard))
        }
        Err(_) => (None, None),
    };
    let _ = tracing_subscriber::registry()
        .with(file)
        .with(devlanes_daemon::diagnostics::layer())
        .try_init();
    guard
}

/// Opens the TUI client.
pub async fn ui(paths: &Paths) -> Result<u8, RpcError> {
    crate::wiring::ui(paths).await
}

/// How long the installed service gives the daemon between SIGTERM and SIGKILL
/// (launchd `ExitTimeOut`, systemd `TimeoutStopSec`). Units stop at the same
/// time, so a shutdown takes about the longest unit stop timeout (10s unless
/// configured) plus a few seconds to record exits and close state.db; 60s
/// covers stop timeouts up to about 55s.
const STOP_WINDOW_S: u64 = 60;

/// launchd's window for a job without `ExitTimeOut`. launchd.plist(5) calls it
/// system-defined; `launchctl print` shows `exit timeout = 5` for such agents
/// on macOS 26 (old manuals said 20s).
const LAUNCHD_DEFAULT_WINDOW_S: u64 = 5;

/// systemd's `DefaultTimeoutStopSec`, 90s in the user manager (systemd-system.conf(5)).
const SYSTEMD_DEFAULT_WINDOW_S: u64 = 90;

/// The launchd job label, also the plist's file name.
const LAUNCHD_LABEL: &str = "run.devlanes.daemon";

/// The per-user service manager: launchd on macOS, systemd elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Launchd,
    Systemd,
}

/// A service manager command of `daemon install`.
#[derive(Debug, PartialEq, Eq)]
struct Step {
    argv: Vec<String>,
    /// Failing is expected at times (a bootout of a job that is gone) and ignored.
    may_fail: bool,
    /// Retried for a while: launchd may still be removing the job a bootout ended.
    retry: bool,
}

impl Step {
    /// A command that must succeed the first time.
    fn new(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            may_fail: false,
            retry: false,
        }
    }
}

impl Manager {
    /// This platform's manager.
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else {
            Self::Systemd
        }
    }

    /// Where `daemon install` writes the definition, under the user's `home`.
    fn path(self, home: &Path) -> PathBuf {
        match self {
            Self::Launchd => home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")),
            Self::Systemd => home.join(".config/systemd/user/devlanes.service"),
        }
    }

    /// The service's name in messages.
    fn noun(self) -> &'static str {
        match self {
            Self::Launchd => "launchd agent",
            Self::Systemd => "systemd user unit",
        }
    }

    /// The stop window when the definition sets none.
    fn default_window(self) -> u64 {
        match self {
            Self::Launchd => LAUNCHD_DEFAULT_WINDOW_S,
            Self::Systemd => SYSTEMD_DEFAULT_WINDOW_S,
        }
    }

    /// The definition that runs `exe daemon run` with `DEVLANES_HOME=devlanes_home`.
    fn definition(self, exe: &Path, devlanes_home: &Path) -> String {
        match self {
            Self::Launchd => format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string><string>run</string></array>
  <key>EnvironmentVariables</key><dict><key>DEVLANES_HOME</key><string>{home}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>ExitTimeOut</key><integer>{STOP_WINDOW_S}</integer>
</dict></plist>
"#,
                exe = xml_escape(&exe.display().to_string()),
                home = xml_escape(&devlanes_home.display().to_string()),
            ),
            // KillMode=process: systemd signals only the daemon, never the lanes'
            // processes (decision 16); its SIGTERM is a graceful shutdown that stops
            // units by policy within TimeoutStopSec.
            Self::Systemd => format!(
                "[Unit]\nDescription=devlanes daemon\n\n[Service]\nExecStart={exe} daemon run\nEnvironment=DEVLANES_HOME={home}\nKillMode=process\nTimeoutStopSec={STOP_WINDOW_S}\n\n[Install]\nWantedBy=default.target\n",
                exe = exe.display(),
                home = devlanes_home.display(),
            ),
        }
    }

    /// The stop window, in seconds, a definition sets; `None` when it sets none
    /// (one written before devlanes set it) or one this does not read.
    fn window(self, contents: &str) -> Option<u64> {
        match self {
            Self::Launchd => {
                let (_, after) = contents.split_once("<key>ExitTimeOut</key>")?;
                let value = after.trim_start().strip_prefix("<integer>")?;
                value.split_once("</integer>")?.0.trim().parse().ok()
            }
            Self::Systemd => contents.lines().rev().find_map(|line| {
                let line = line.trim();
                let value = line
                    .strip_prefix("TimeoutStopSec=")
                    .or_else(|| line.strip_prefix("TimeoutSec="))?
                    .trim();
                match value.strip_suffix("min") {
                    Some(minutes) => minutes.trim().parse::<u64>().ok()?.checked_mul(60),
                    None => value.strip_suffix('s').unwrap_or(value).trim().parse().ok(),
                }
            }),
        }
    }

    /// Commands that make the manager run the definition at `path`. launchd reads
    /// a plist only when the job loads, so a `changed` definition of a `loaded`
    /// job is booted out and in again; systemd rereads its units on
    /// `daemon-reload`, and a running unit keeps running.
    fn activate(self, path: &Path, uid: u32, loaded: bool, changed: bool) -> Vec<Step> {
        let path = path.display().to_string();
        let domain = format!("gui/{uid}");
        match self {
            Self::Launchd if loaded && !changed => Vec::new(),
            Self::Launchd => {
                let mut steps = Vec::new();
                if loaded {
                    steps.push(Step {
                        may_fail: true,
                        ..Step::new(&["launchctl", "bootout", &domain, &path])
                    });
                }
                steps.push(Step {
                    retry: loaded,
                    ..Step::new(&["launchctl", "bootstrap", &domain, &path])
                });
                steps
            }
            Self::Systemd => vec![
                Step::new(&["systemctl", "--user", "daemon-reload"]),
                Step::new(&["systemctl", "--user", "enable", "--now", "devlanes.service"]),
            ],
        }
    }

    /// Commands that stop the manager from starting the daemon.
    fn deactivate(self, path: &Path, uid: u32) -> Vec<Vec<String>> {
        match self {
            Self::Launchd => vec![vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}"),
                path.display().to_string(),
            ]],
            Self::Systemd => vec![vec![
                "systemctl".into(),
                "--user".into(),
                "disable".into(),
                "devlanes.service".into(),
            ]],
        }
    }
}

/// The user's home directory, under which the service definition lives.
fn user_home() -> Result<PathBuf, RpcError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| RpcError::new("no_home", json!({}), 1))
}

/// Whether launchd has the devlanes job loaded in the user's GUI domain.
async fn launchd_loaded(uid: u32) -> bool {
    tokio::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Runs one install step; a retried step tries again until the old job's stop
/// window (and a margin) has passed.
async fn run_step(step: &Step) -> Result<(), RpcError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(STOP_WINDOW_S + 5);
    loop {
        let output = tokio::process::Command::new(&step.argv[0])
            .args(&step.argv[1..])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(io)?;
        if output.status.success() || step.may_fail {
            return Ok(());
        }
        if !step.retry || tokio::time::Instant::now() >= deadline {
            return Err(RpcError::new(
                "service_install_failed",
                json!({
                    "command": step.argv.join(" "),
                    "message": String::from_utf8_lossy(&output.stderr).trim(),
                }),
                1,
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Installs a per-user service so the daemon starts outside agent sandboxes and
/// at login. Running it again rewrites a definition that differs (another
/// binary, an older one without a stop window) and has the manager reload it.
/// An invalid user config fails first: a reload stops the running daemon with its
/// lanes, and the one the manager starts would refuse the file.
pub async fn install(runtime: &Runtime) -> Result<u8, RpcError> {
    let exe = std::env::current_exe().map_err(io)?;
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    crate::user_config::load(&paths).await?;
    let manager = Manager::current();
    let path = manager.path(&user_home()?);
    let contents = manager.definition(&exe, &paths.home);
    let previous = tokio::fs::read_to_string(&path).await.ok();
    let changed = previous.as_deref() != Some(contents.as_str());
    if changed {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io)?;
        }
        tokio::fs::write(&path, contents).await.map_err(io)?;
    }
    let uid = rustix::process::getuid().as_raw();
    let loaded = manager == Manager::Launchd && launchd_loaded(uid).await;
    for step in manager.activate(&path, uid, loaded, changed) {
        run_step(&step).await?;
    }
    let _ = runtime;
    let verb = match (changed, previous.is_some()) {
        (false, _) => "up to date",
        (true, true) => "updated",
        (true, false) => "installed",
    };
    println!(
        "{} {verb}: {}; the daemon gets {STOP_WINDOW_S}s to stop",
        manager.noun(),
        path.display()
    );
    if changed && loaded {
        println!(
            "reloaded the agent; a daemon it ran was stopped with its units and started again"
        );
    }
    Ok(0)
}

/// `doctor`'s `service`: `{manager, path, installed, stop_window_s,
/// window_source, recommended_window_s}`, where `stop_window_s` is how long the
/// service manager waits between SIGTERM and SIGKILL. `window_source` is
/// `installed` when the definition sets it and `default` when the manager's
/// default applies (nothing installed, or a definition from before devlanes set
/// one).
pub async fn service_report() -> serde_json::Value {
    let manager = Manager::current();
    let Ok(home) = user_home() else {
        return serde_json::Value::Null;
    };
    let path = manager.path(&home);
    let contents = tokio::fs::read_to_string(&path).await.ok();
    service_facts(manager, &path, contents.as_deref())
}

/// [`service_report`] for a definition read from `path` (`None`: not installed).
fn service_facts(manager: Manager, path: &Path, contents: Option<&str>) -> serde_json::Value {
    let set = contents.and_then(|contents| manager.window(contents));
    json!({
        "manager": manager.noun(),
        "path": path,
        "installed": contents.is_some(),
        "stop_window_s": set.unwrap_or(manager.default_window()),
        "window_source": if set.is_some() { "installed" } else { "default" },
        "recommended_window_s": STOP_WINDOW_S,
    })
}

/// Removes the per-user service definition (the running daemon keeps running).
pub async fn uninstall(runtime: &Runtime) -> Result<u8, RpcError> {
    let manager = Manager::current();
    let path = manager.path(&user_home()?);
    for command in manager.deactivate(&path, rustix::process::getuid().as_raw()) {
        let _ = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .status()
            .await;
    }
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    let _ = runtime;
    println!("removed {}", path.display());
    Ok(0)
}

/// Minimal escaping for plist string values.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Local-only actions: trust, unlock, tunnel, askpass.
pub async fn local_action(runtime: &Runtime, action: Action) -> Result<u8, RpcError> {
    crate::local::run(runtime, action).await
}

/// I/O errors keep their message as data.
fn io(error: std::io::Error) -> RpcError {
    RpcError::new("io", json!({"message": error.to_string()}), 1)
}

/// SQLite's files for `db`: the database, its WAL and shared memory.
const STATE_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

/// `db` with `suffix` appended to its file name.
fn with_suffix(db: &Path, suffix: &str) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Which of the state files exist (symlinks included, never followed).
pub fn state_files(db: &Path) -> Vec<PathBuf> {
    STATE_SUFFIXES
        .iter()
        .map(|suffix| with_suffix(db, suffix))
        .filter(|path| path.symlink_metadata().is_ok())
        .collect()
}

/// Refuses while a daemon holds the singleton lock under `home`; otherwise the
/// returned file keeps any daemon from starting until it is dropped.
pub fn hold_daemon_lock(home: &Path) -> Result<std::fs::File, RpcError> {
    match devlanes_daemon::lock_singleton(home) {
        Ok(lock) => Ok(lock),
        Err(devlanes_daemon::Error::AlreadyRunning) => Err(RpcError::new(
            "daemon_running",
            json!({"message": "a daemon is running or starting, so state.db was not moved; retry once it answers, or after `devlanes daemon stop`"}),
            1,
        )),
        Err(error) => Err(RpcError::new(
            "io",
            json!({"message": error.to_string()}),
            1,
        )),
    }
}

/// `daemon reset-state` without a daemon: under the daemon lock, renames
/// `state.db`, `-wal` and `-shm` to `state.db.legacy-<stamp>[-wal|-shm]`, so the
/// pair stays readable with SQLite, then moves the project folders of
/// `data_root` into `<data_root>/.legacy-<stamp>`. Lane data folders are named by
/// lane record id, which a new database hands out again from 1, so left in place
/// they would give new lanes old data. Nothing is deleted; an existing target
/// fails the whole move before anything is renamed. Returns `(from, to)` pairs.
pub fn move_state_aside(
    home: &Path,
    db: &Path,
    data_root: &Path,
    stamp: &str,
) -> Result<Vec<(PathBuf, PathBuf)>, RpcError> {
    let _lock = hold_daemon_lock(home)?;
    let base = with_suffix(db, &format!(".legacy-{stamp}"));
    let mut moves: Vec<(PathBuf, PathBuf)> = STATE_SUFFIXES
        .iter()
        .map(|suffix| (with_suffix(db, suffix), with_suffix(&base, suffix)))
        .filter(|(from, _)| from.symlink_metadata().is_ok())
        .collect();
    let data_aside = data_root.join(format!(".legacy-{stamp}"));
    let data_moves: Vec<(PathBuf, PathBuf)> = project_data_folders(data_root)?
        .into_iter()
        .map(|name| (data_root.join(&name), data_aside.join(&name)))
        .collect();
    if let Some((_, taken)) = moves
        .iter()
        .chain(&data_moves)
        .find(|(_, to)| to.symlink_metadata().is_ok())
    {
        return Err(RpcError::new(
            "io",
            json!({"message": format!("{} already exists; nothing was moved", taken.display())}),
            1,
        ));
    }
    for (from, to) in &moves {
        std::fs::rename(from, to).map_err(io)?;
    }
    if !data_moves.is_empty() {
        std::fs::create_dir_all(&data_aside).map_err(io)?;
    }
    for (from, to) in &data_moves {
        std::fs::rename(from, to).map_err(io)?;
    }
    moves.extend(data_moves);
    Ok(moves)
}

/// Names of the folders under `data_root` shaped like the daemon's project data
/// folders: real directories holding only lane folders named by number. Anything
/// else stays, since `lanes.data_dir` may name a folder shared with other files.
fn project_data_folders(data_root: &Path) -> Result<Vec<std::ffi::OsString>, RpcError> {
    let entries = match std::fs::read_dir(data_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io(error)),
    };
    let is_dir = |path: &Path| path.symlink_metadata().is_ok_and(|meta| meta.is_dir());
    let holds_only_lanes = |path: &Path| -> bool {
        let Ok(children) = std::fs::read_dir(path) else {
            return false;
        };
        let mut any = false;
        for child in children {
            let Ok(child) = child else { return false };
            let numbered = child
                .file_name()
                .to_str()
                .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()));
            if !numbered || !is_dir(&child.path()) {
                return false;
            }
            any = true;
        }
        any
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io)?;
        let path = entry.path();
        if is_dir(&path) && holds_only_lanes(&path) {
            names.push(entry.file_name());
        }
    }
    names.sort();
    Ok(names)
}

/// `YYYYMMDDTHHMMSSZ` for Unix seconds, in UTC.
pub fn utc_stamp(seconds: u64) -> String {
    // Days to civil date (Howard Hinnant's algorithm), proleptic Gregorian.
    let days = i64::try_from(seconds / 86_400).unwrap_or_default() + 719_468;
    let rest = seconds % 86_400;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    /// SIGTERM cancels the server's shutdown token, the path `daemon stop` takes.
    async fn sigterm_starts_the_graceful_shutdown() {
        let shutdown = CancellationToken::new();
        watch_signals(shutdown.clone(), CancellationToken::new());
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .expect("signal");
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.cancelled())
            .await
            .expect("shutdown requested");
    }

    #[tokio::test]
    /// SIGTERM during an explicit shutdown interrupts its `lane.stop` tasks.
    async fn sigterm_during_a_shutdown_interrupts_its_tasks() {
        let shutdown = CancellationToken::new();
        let interrupt = CancellationToken::new();
        watch_signals(shutdown.clone(), interrupt.clone());
        shutdown.cancel();
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .expect("signal");
        tokio::time::timeout(std::time::Duration::from_secs(5), interrupt.cancelled())
            .await
            .expect("tasks interrupted");
    }

    #[test]
    /// Both definitions set the stop window and read it back; definitions from
    /// before it was set fall back to the manager's default.
    fn service_definitions_set_the_stop_window() {
        let exe = Path::new("/opt/acme & co/devlanes");
        let home = Path::new("/home/acme/.devlanes");
        for manager in [Manager::Launchd, Manager::Systemd] {
            let definition = manager.definition(exe, home);
            assert_eq!(
                manager.window(&definition),
                Some(STOP_WINDOW_S),
                "{definition}"
            );
            let path = manager.path(Path::new("/home/acme"));
            let facts = service_facts(manager, &path, Some(&definition));
            assert_eq!(facts["installed"], true);
            assert_eq!(facts["stop_window_s"], STOP_WINDOW_S);
            assert_eq!(facts["window_source"], "installed");
        }
        let plist = Manager::Launchd.definition(exe, home);
        assert!(
            plist.contains("<string>/opt/acme &amp; co/devlanes</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<key>ExitTimeOut</key><integer>60</integer>"),
            "{plist}"
        );
        let unit = Manager::Systemd.definition(exe, home);
        assert!(
            unit.contains("KillMode=process\nTimeoutStopSec=60\n"),
            "{unit}"
        );

        let old_plist = plist.replace("  <key>ExitTimeOut</key><integer>60</integer>\n", "");
        let facts = service_facts(Manager::Launchd, Path::new("/p"), Some(&old_plist));
        assert_eq!(facts["stop_window_s"], LAUNCHD_DEFAULT_WINDOW_S);
        assert_eq!(facts["window_source"], "default");
        let facts = service_facts(Manager::Systemd, Path::new("/p"), None);
        assert_eq!(facts["installed"], false);
        assert_eq!(facts["stop_window_s"], SYSTEMD_DEFAULT_WINDOW_S);
        assert_eq!(Manager::Systemd.window("TimeoutStopSec=2min\n"), Some(120));
        assert_eq!(Manager::Systemd.window("TimeoutSec=45s\n"), Some(45));
        assert_eq!(Manager::Systemd.window("TimeoutStopSec=infinity\n"), None);
    }

    #[test]
    /// launchd reloads a changed plist of a loaded job (bootout, then a retried
    /// bootstrap), leaves an unchanged one alone and bootstraps a missing job;
    /// systemd always reloads and enables.
    fn install_reloads_a_changed_definition() {
        let path = Path::new("/u/Library/LaunchAgents/run.devlanes.daemon.plist");
        let argv = |steps: &[Step]| -> Vec<String> {
            steps.iter().map(|step| step.argv.join(" ")).collect()
        };
        let reload = Manager::Launchd.activate(path, 501, true, true);
        assert_eq!(
            argv(&reload),
            [
                format!("launchctl bootout gui/501 {}", path.display()),
                format!("launchctl bootstrap gui/501 {}", path.display()),
            ]
        );
        assert!(reload[0].may_fail && !reload[0].retry);
        assert!(!reload[1].may_fail && reload[1].retry);
        assert!(Manager::Launchd.activate(path, 501, true, false).is_empty());
        let fresh = Manager::Launchd.activate(path, 501, false, true);
        assert_eq!(
            argv(&fresh),
            [format!("launchctl bootstrap gui/501 {}", path.display())]
        );
        assert!(!fresh[0].retry);
        assert_eq!(
            argv(&Manager::Systemd.activate(path, 1000, false, false)),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now devlanes.service"
            ]
        );
    }

    #[test]
    /// Known instants, including a leap day.
    fn utc_stamps() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(1_792_595_045), "20261021T150405Z");
    }
}
