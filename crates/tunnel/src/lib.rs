//! Box side of remote operation: `devlanes tunnel --stdio`.
//!
//! Remote is mirroring only. [`services`] answers a client daemon's mux streams
//! from this machine's own daemon, reached over its Unix socket like any other
//! client:
//!
//! - the registry comes from `status --all` and `hosts`, refreshed on daemon events
//!   and published as incremental changes; it carries ports, hostnames and states,
//!   never environments or secrets;
//! - data streams dial loopback ports of running, non-hidden units, nothing else;
//! - syncs stream `sync.status` entries; the client never chooses a path;
//! - daemon requests are refused: a client never runs commands on the box.

mod registry;
mod rpc;
mod sync;

use std::sync::Arc;

use devlanes_client::{Client, Paths};
use devlanes_proto::remote::RemoteClient;
use devlanes_transport::{
    BoxServices, EXIT_DAEMON_STOPPED, LoopbackDialer, ServeError, ServeOptions,
};

/// Login-session variables a daemon started by the tunnel must not keep: the
/// daemon outlives the SSH session, and an agent socket would let its processes
/// use the connecting user's keys.
const SESSION_ENV: [&str; 5] = [
    "SSH_AUTH_SOCK",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "SSH_ORIGINAL_COMMAND",
];

/// Why the tunnel stopped.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// The box daemon could not be reached or started.
    #[error("box daemon: {0}")]
    Daemon(#[from] devlanes_client::Error),
    /// No box daemon runs and the box's user config turns off starting one
    /// (`remote.start_daemon: false`).
    #[error("box daemon is not running")]
    DaemonStopped,
    /// The session with the client failed.
    #[error(transparent)]
    Serve(#[from] ServeError),
}

impl TunnelError {
    /// The process exit status for this failure: [`EXIT_DAEMON_STOPPED`] for a
    /// stopped daemon, which the client's transport reports as temporary, else 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::DaemonStopped => u8::try_from(EXIT_DAEMON_STOPPED).unwrap_or(1),
            Self::Daemon(_) | Self::Serve(_) => 1,
        }
    }
}

/// Box services backed by the daemon behind `paths`.
///
/// `client` is listed by the box's `status` while the registry feed is open.
pub fn services(paths: Paths, client: RemoteClient) -> BoxServices {
    BoxServices {
        registry: Arc::new(registry::DaemonRegistry::new(paths.clone(), client)),
        rpc: Arc::new(rpc::Refuse),
        dialer: Arc::new(LoopbackDialer),
        sync: Arc::new(sync::DaemonSync::new(paths)),
    }
}

/// Version label sent to clients in the handshake: `devlanes <version>`, plus
/// `(linger off)` on Linux boxes whose user services stop at logout.
pub fn server_label() -> String {
    let label = format!("devlanes {}", devlanes_version::VERSION);
    if linger_off() {
        format!("{label} (linger off)")
    } else {
        label
    }
}

/// False: Windows has no systemd to stop the user's processes at logout.
#[cfg(windows)]
fn linger_off() -> bool {
    false
}

/// True on Linux when systemd stops the user's processes at logout, which would
/// stop a daemon the tunnel started once the SSH session ends.
#[cfg(unix)]
fn linger_off() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let user = rustix::process::getuid().as_raw().to_string();
    std::process::Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout.trim_ascii() == b"no")
}

/// Serves one client over this process's stdin and stdout until it leaves.
///
/// Starts the box daemon when it is not running and `start_daemon` (the box's
/// `remote.start_daemon`) allows it; otherwise a stopped daemon fails with
/// [`TunnelError::DaemonStopped`]. Nothing but mux frames may reach stdout, so
/// diagnostics belong on stderr, which ssh hands back to the client.
pub async fn run_stdio(paths: Paths, start_daemon: bool) -> Result<(), TunnelError> {
    drop(daemon_client(&paths, start_daemon).await?);
    let client = RemoteClient {
        pid: std::process::id(),
        ssh: std::env::var("SSH_CONNECTION")
            .ok()
            .filter(|value| !value.is_empty()),
        since_ms: now_ms(),
    };
    // `loginctl` blocks briefly; it runs once, before serving starts.
    let label = tokio::task::spawn_blocking(server_label)
        .await
        .unwrap_or_else(|_| format!("devlanes {}", devlanes_version::VERSION));
    devlanes_transport::serve(
        devlanes_transport::stdio(),
        services(paths, client),
        ServeOptions::new(label),
    )
    .await?;
    Ok(())
}

/// A connection to the box daemon, starting it first only when `start_daemon`.
async fn daemon_client(paths: &Paths, start_daemon: bool) -> Result<Client, TunnelError> {
    if start_daemon {
        return Ok(Client::connect_or_spawn_without(paths, &SESSION_ENV).await?);
    }
    match Client::connect(paths).await {
        Ok(client) => Ok(client),
        Err(error) if error.is_not_running() => Err(TunnelError::DaemonStopped),
        Err(error) => Err(error.into()),
    }
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A box error carried as transport RPC error data, so the client daemon can hand
/// the structured error to its CLI unchanged.
pub(crate) fn rpc_error(error: devlanes_proto::RpcError) -> devlanes_transport::RpcError {
    devlanes_transport::RpcError {
        message: error.code.clone(),
        data: serde_json::to_value(&error).ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Paths of a home in `dir` where no daemon runs.
    fn idle_home(dir: &std::path::Path) -> Paths {
        let socket = dir.join("run").join("d.sock");
        // Windows pipes have their own namespace.
        #[cfg(windows)]
        let socket = std::path::PathBuf::from(format!(
            r"\\.\pipe\devlanes-tunnel-test-{}",
            socket.to_string_lossy().replace(['\\', '/', ':'], "-")
        ));
        Paths {
            run: dir.join("run"),
            socket,
            db: dir.join("state.db"),
            logs: dir.join("logs"),
            home: dir.to_path_buf(),
        }
    }

    /// With `remote.start_daemon: false` a stopped daemon is not started: the
    /// tunnel fails with the status the client's transport retries on.
    #[tokio::test]
    async fn a_stopped_daemon_is_reported_when_starting_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = idle_home(dir.path());
        let result = tokio::time::timeout(Duration::from_secs(30), daemon_client(&paths, false))
            .await
            .expect("timeout");
        let Err(error) = result else {
            panic!("connected to a daemon that does not run");
        };
        assert!(matches!(error, TunnelError::DaemonStopped), "{error}");
        assert_eq!(error.to_string(), "box daemon is not running");
        assert_eq!(i32::from(error.exit_code()), EXIT_DAEMON_STOPPED);
        assert!(!paths.run.exists(), "nothing was prepared for a daemon");
    }
}
