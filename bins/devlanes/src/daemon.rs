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
    // Before `prepare`, so an elevated run creates nothing under the home.
    if let Err(error) = devlanes_daemon::refuse_elevation() {
        eprintln!("error: {error}");
        let _ = readiness.failed(&error.to_string());
        return ExitCode::from(1);
    }
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
        watch_signals(server.shutdown_token());
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
/// like `devlanes daemon stop`: units stop, exits are recorded, state.db closes.
#[cfg(unix)]
fn watch_signals(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut interrupt) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(term), Ok(interrupt)) => (term, interrupt),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "signal handlers not installed; signals end the daemon without a shutdown");
            return;
        }
    };
    tokio::spawn(async move {
        let name = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
            () = shutdown.cancelled() => return,
        };
        tracing::info!(signal = name, "shutting down");
        shutdown.cancel();
    });
}

/// Ctrl-C and Ctrl-Break shut down a daemon run by hand in a console, like
/// `devlanes daemon stop`. A detached daemon has no console and gets neither;
/// it stops through `daemon stop`.
#[cfg(windows)]
fn watch_signals(shutdown: CancellationToken) {
    use tokio::signal::windows::{ctrl_break, ctrl_c};
    let (mut interrupt, mut brk) = match (ctrl_c(), ctrl_break()) {
        (Ok(interrupt), Ok(brk)) => (interrupt, brk),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "console handlers not installed; Ctrl-C ends the daemon without a shutdown");
            return;
        }
    };
    tokio::spawn(async move {
        let name = tokio::select! {
            _ = interrupt.recv() => "Ctrl-C",
            _ = brk.recv() => "Ctrl-Break",
            () = shutdown.cancelled() => return,
        };
        tracing::info!(signal = name, "shutting down");
        shutdown.cancel();
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

/// Installs a per-user service so the daemon starts outside agent sandboxes and at login.
#[cfg(unix)]
pub async fn install(runtime: &Runtime) -> Result<u8, RpcError> {
    let exe = std::env::current_exe().map_err(io)?;
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    let (path, contents, enable): (std::path::PathBuf, String, Vec<Vec<String>>) =
        service_definition(&exe, &paths)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io)?;
    }
    tokio::fs::write(&path, contents).await.map_err(io)?;
    for command in enable {
        let status = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .status()
            .await
            .map_err(io)?;
        if !status.success() {
            return Err(RpcError::new(
                "service_install_failed",
                json!({"command": command.join(" ")}),
                1,
            ));
        }
    }
    let _ = runtime;
    println!("installed {}", path.display());
    Ok(0)
}

/// Removes the per-user service definition (the running daemon keeps running).
#[cfg(unix)]
pub async fn uninstall(runtime: &Runtime) -> Result<u8, RpcError> {
    let exe = std::env::current_exe().map_err(io)?;
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    let (path, _, _) = service_definition(&exe, &paths)?;
    for command in disable_commands(&path) {
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

/// `daemon install` on Windows: refused until the daemon can start at logon there.
#[cfg(windows)]
pub async fn install(_: &Runtime) -> Result<u8, RpcError> {
    Err(service_unsupported())
}

/// `daemon uninstall` on Windows: refused, since `install` installs nothing yet.
#[cfg(windows)]
pub async fn uninstall(_: &Runtime) -> Result<u8, RpcError> {
    Err(service_unsupported())
}

/// The error for `daemon install`/`uninstall` on Windows.
#[cfg(windows)]
fn service_unsupported() -> RpcError {
    RpcError::new(
        "not_supported_yet",
        json!({"feature": "daemon install on Windows"}),
        1,
    )
}

/// Unloads the launchd agent at `path`.
#[cfg(target_os = "macos")]
fn disable_commands(path: &Path) -> Vec<Vec<String>> {
    vec![vec![
        "launchctl".into(),
        "bootout".into(),
        format!("gui/{}", rustix::process::getuid().as_raw()),
        path.display().to_string(),
    ]]
}

/// Disables the systemd user unit; `path` is the unit file, removed afterwards.
#[cfg(all(unix, not(target_os = "macos")))]
fn disable_commands(_path: &Path) -> Vec<Vec<String>> {
    vec![vec![
        "systemctl".into(),
        "--user".into(),
        "disable".into(),
        "devlanes.service".into(),
    ]]
}

/// launchd agent on macOS, systemd user unit on Linux (compile-time choice).
#[cfg(unix)]
fn service_definition(
    exe: &std::path::Path,
    paths: &Paths,
) -> Result<(std::path::PathBuf, String, Vec<Vec<String>>), RpcError> {
    let home =
        devlanes_client::user_home().ok_or_else(|| RpcError::new("no_home", json!({}), 1))?;
    #[cfg(target_os = "macos")]
    {
        let path = home.join("Library/LaunchAgents/run.devlanes.daemon.plist");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>run.devlanes.daemon</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string><string>run</string></array>
  <key>EnvironmentVariables</key><dict><key>DEVLANES_HOME</key><string>{home}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict></plist>
"#,
            exe = xml_escape(&exe.display().to_string()),
            home = xml_escape(&paths.home.display().to_string()),
        );
        let domain = format!("gui/{}", rustix::process::getuid().as_raw());
        Ok((
            path.clone(),
            plist,
            vec![vec![
                "launchctl".into(),
                "bootstrap".into(),
                domain,
                path.display().to_string(),
            ]],
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let path = home.join(".config/systemd/user/devlanes.service");
        // KillMode=process: systemd signals only the daemon, never the lanes' processes
        // (decision 16); its SIGTERM is a graceful shutdown that stops units by policy.
        let unit = format!(
            "[Unit]\nDescription=devlanes daemon\n\n[Service]\nExecStart={exe} daemon run\nEnvironment=DEVLANES_HOME={home}\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
            exe = exe.display(),
            home = paths.home.display(),
        );
        Ok((
            path,
            unit,
            vec![
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    "devlanes.service".into(),
                ],
            ],
        ))
    }
}

/// Minimal escaping for plist string values.
#[cfg(target_os = "macos")]
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
/// pair stays readable with SQLite. Nothing is deleted; an existing target fails
/// the whole move before anything is renamed. Returns `(from, to)` pairs.
pub fn move_state_aside(
    home: &Path,
    db: &Path,
    stamp: &str,
) -> Result<Vec<(PathBuf, PathBuf)>, RpcError> {
    let _lock = hold_daemon_lock(home)?;
    let base = with_suffix(db, &format!(".legacy-{stamp}"));
    let moves: Vec<(PathBuf, PathBuf)> = STATE_SUFFIXES
        .iter()
        .map(|suffix| (with_suffix(db, suffix), with_suffix(&base, suffix)))
        .filter(|(from, _)| from.symlink_metadata().is_ok())
        .collect();
    if let Some((_, taken)) = moves.iter().find(|(_, to)| to.symlink_metadata().is_ok()) {
        return Err(RpcError::new(
            "io",
            json!({"message": format!("{} already exists; nothing was moved", taken.display())}),
            1,
        ));
    }
    for (from, to) in &moves {
        std::fs::rename(from, to).map_err(io)?;
    }
    Ok(moves)
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

    #[cfg(unix)]
    #[tokio::test]
    /// SIGTERM cancels the server's shutdown token, the path `daemon stop` takes.
    async fn sigterm_starts_the_graceful_shutdown() {
        let shutdown = CancellationToken::new();
        watch_signals(shutdown.clone());
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .expect("signal");
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.cancelled())
            .await
            .expect("shutdown requested");
    }

    #[test]
    /// Known instants, including a leap day.
    fn utc_stamps() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(1_792_595_045), "20261021T150405Z");
    }
}
