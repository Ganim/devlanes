//! Daemon process entry plus local actions that must run in the user's terminal.

use std::process::ExitCode;

use devlanes_cli::{Action, Runtime};
use devlanes_client::Paths;
use devlanes_daemon::{
    Options, Server,
    process::{Readiness, detach},
};
use devlanes_proto::RpcError;
use serde_json::json;

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
        match server.run(handler, move || readiness.ready()).await {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!(%error, "daemon stopped with an error");
                1
            }
        }
    });
    // Dropping the runtime waits for every spawn_blocking task (git, getaddrinfo, file
    // watchers), which can keep a stopped daemon alive indefinitely; bound the wait.
    tokio.shutdown_timeout(std::time::Duration::from_secs(5));
    ExitCode::from(code)
}

/// The daemon never prints to stdout; everything goes to `logs/daemon.log`.
fn init_logging(paths: &Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let path = paths.logs.join("daemon.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, paths.logs.join("daemon.log.1"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_env("DEVLANES_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Some(guard)
}

/// Opens the TUI client.
pub async fn ui(paths: &Paths) -> Result<u8, RpcError> {
    crate::wiring::ui(paths).await
}

/// Installs a per-user service so the daemon starts outside agent sandboxes and at login.
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
pub async fn uninstall(runtime: &Runtime) -> Result<u8, RpcError> {
    let exe = std::env::current_exe().map_err(io)?;
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    let (path, _, _) = service_definition(&exe, &paths)?;
    let disable: Vec<Vec<String>> = if cfg!(target_os = "macos") {
        vec![vec![
            "launchctl".into(),
            "bootout".into(),
            format!("gui/{}", rustix::process::getuid().as_raw()),
            path.display().to_string(),
        ]]
    } else {
        vec![vec![
            "systemctl".into(),
            "--user".into(),
            "disable".into(),
            "devlanes.service".into(),
        ]]
    };
    for command in disable {
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

/// launchd agent on macOS, systemd user unit on Linux (compile-time choice).
fn service_definition(
    exe: &std::path::Path,
    paths: &Paths,
) -> Result<(std::path::PathBuf, String, Vec<Vec<String>>), RpcError> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| RpcError::new("no_home", json!({}), 1))?;
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
        // KillMode=process: stopping the unit must not kill lanes (decision 16); the daemon
        // decides what to stop on an explicit `devlanes daemon stop`.
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
