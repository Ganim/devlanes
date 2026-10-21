//! Thin Unix-socket client shared by CLI, completion, and TUI frontends.

mod paths;
pub mod update;

use std::{collections::VecDeque, path::PathBuf, process::Stdio, time::Duration};

use bytes::Bytes;
use devlanes_ipc::Connection;
pub use devlanes_ipc::Frame;
pub use devlanes_proto::{Context, Hello, Method, RpcError};
use devlanes_proto::{Control, Outcome, PROTOCOL_VERSION, Request};
pub use paths::Paths;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
    process::Command,
    time::{Instant, sleep, timeout},
};

pub type Result<T> = std::result::Result<T, Error>;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;
const MAX_PENDING_FRAMES: usize = 256;

/// Transport errors stay separate from structured daemon failures for exit-code rendering.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("client I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Ipc(#[from] devlanes_ipc::Error),
    #[error("the daemon answered {} (exit {})", .0.code, .0.exit_code)]
    Rpc(RpcError),
    #[error("daemon v{version} (build {build_id}) runs {managed_processes} processes with protocol {protocol}; run devlanes daemon restart", version = .peer.version, build_id = .peer.build_id, managed_processes = .peer.managed_processes, protocol = .peer.protocol)]
    Incompatible { peer: Hello },
    #[error("daemon peer uid does not match this user")]
    WrongPeer,
    #[error("expected {0} from daemon")]
    UnexpectedFrame(&'static str),
    #[error("daemon connection or startup timed out")]
    Timeout,
    #[error(
        "auto-spawn refused in sandbox ({0}); start devlanes outside the sandbox or run devlanes daemon install"
    )]
    Sandbox(&'static str),
    #[error("HOME or DEVLANES_HOME is required")]
    MissingHome,
    #[error("unsafe daemon directory: {0}")]
    UnsafeDirectory(PathBuf),
    #[error("socket path has no parent directory")]
    InvalidSocketPath,
    #[error("daemon startup failed: {0}")]
    Startup(String),
    #[error("stream queue is full; reconnect and resynchronize")]
    StreamOverflow,
    #[error("request identifiers exhausted")]
    RequestIdsExhausted,
    #[error("blocking startup task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl Error {
    /// Whether this connect error means no daemon listens: the socket is missing or
    /// refuses connections. Anything else (timeout, wrong peer, protocol) means one
    /// runs and must not be treated as stopped.
    pub fn is_not_running(&self) -> bool {
        matches!(
            self,
            Self::Io(error) if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        )
    }

    /// Whether a sandbox, not a missing daemon, blocked the connection.
    pub fn is_permission(&self) -> bool {
        matches!(
            self,
            Self::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
        )
    }
}

/// One client serializes requests; use begin_request/next_frame for streaming consumers.
pub struct Client {
    connection: Connection<UnixStream>,
    pub peer: Hello,
    /// True when this connection's `connect_or_spawn` started the daemon; `devlanes with`
    /// stops only a daemon it started itself.
    pub spawned: bool,
    next_id: u64,
    pending: VecDeque<Frame>,
    pending_bytes: usize,
}

impl Client {
    /// Connects without creating state or spawning, as required by shell completions.
    pub async fn connect(paths: &Paths) -> Result<Self> {
        Self::handshake(paths, true).await
    }

    /// Like [`Client::connect`], but keeps the connection when the daemon speaks
    /// another protocol; check [`Client::compatible`]. Only for the frozen methods
    /// every daemon answers on any protocol: `daemon.shutdown` and `daemon.status`.
    pub async fn connect_any(paths: &Paths) -> Result<Self> {
        Self::handshake(paths, false).await
    }

    /// Whether the daemon speaks this client's protocol.
    pub fn compatible(&self) -> bool {
        self.peer.protocol == PROTOCOL_VERSION
    }

    /// Hello exchange; `strict` rejects another protocol with [`Error::Incompatible`].
    async fn handshake(paths: &Paths, strict: bool) -> Result<Self> {
        timeout(CONNECT_TIMEOUT, async {
            let stream = UnixStream::connect(&paths.socket).await?;
            if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() {
                return Err(Error::WrongPeer);
            }
            let mut connection = Connection::new(stream);
            connection
                .send(Frame::Control(Control::Hello(Hello::current(0))))
                .await?;
            let Frame::Control(Control::Hello(peer)) = connection.recv().await? else {
                return Err(Error::UnexpectedFrame("hello"));
            };
            if strict && peer.protocol != PROTOCOL_VERSION {
                return Err(Error::Incompatible { peer });
            }
            Ok(Self {
                connection,
                peer,
                spawned: false,
                next_id: 1,
                pending: VecDeque::new(),
                pending_bytes: 0,
            })
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    /// Starts the current executable only when the socket is absent or refuses connections.
    pub async fn connect_or_spawn(paths: &Paths) -> Result<Self> {
        Self::connect_or_spawn_without(paths, &[]).await
    }

    /// Like [`Client::connect_or_spawn`], with `remove_env` dropped from a spawned
    /// daemon's environment, such as the SSH variables of a tunnel's login session.
    pub async fn connect_or_spawn_without(paths: &Paths, remove_env: &[&str]) -> Result<Self> {
        match Self::connect(paths).await {
            Ok(client) => return Ok(client),
            Err(error) if error.is_not_running() => {}
            // EPERM/EACCES on a socket that exists: a sandbox forbids the connection.
            Err(error) if error.is_permission() => {
                return Err(Error::Sandbox(
                    "connecting to the daemon socket is not permitted",
                ));
            }
            Err(error) => return Err(error),
        }
        if let Some(reason) = sandbox_reason() {
            return Err(Error::Sandbox(reason));
        }
        let owned_paths = paths.clone();
        tokio::task::spawn_blocking(move || {
            owned_paths.prepare()?;
            probe_socket(&owned_paths)
        })
        .await?
        .map_err(|error| {
            if error.is_permission() {
                Error::Sandbox("creating the daemon directory is not permitted")
            } else {
                error
            }
        })?;
        let mut command = Command::new(std::env::current_exe()?);
        for name in remove_env {
            command.env_remove(name);
        }
        let mut child = command
            .args(["daemon", "run"])
            .env("DEVLANES_HOME", &paths.home)
            .env("DEVLANES_READY_FD", "1")
            .current_dir(&paths.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Startup("missing readiness pipe".into()))?;
        // Tokio reaps the child after this handle is dropped; never kill a potentially live daemon.
        drop(child);
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + START_TIMEOUT;
        let mut message = Vec::new();
        timeout(START_TIMEOUT, async {
            // Bound an invalid child's output rather than buffering arbitrary stdout.
            use tokio::io::AsyncReadExt;
            (&mut reader)
                .take(128)
                .read_until(b'\n', &mut message)
                .await
        })
        .await
        .map_err(|_| Error::Timeout)??;
        if message != b"ready\n" && message != b"already-running\n" {
            let reason = String::from_utf8_lossy(&message).trim().to_owned();
            // A daemon that died before writing a reason left it in its own log.
            return Err(Error::Startup(if reason.is_empty() {
                format!(
                    "the daemon exited without a reason; see {}",
                    paths.logs.join("daemon.log").display()
                )
            } else {
                reason
            }));
        }
        // Losing the race to another client's daemon means this invocation did not spawn it.
        let spawned = message == b"ready\n";
        loop {
            match Self::connect(paths).await {
                Ok(mut client) => {
                    client.spawned = spawned;
                    return Ok(client);
                }
                Err(error) if error.is_not_running() => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    /// Returns a terminal response while retaining intervening events and bytes for next_frame.
    /// Do not mix this helper with outstanding begin_request calls on the same connection.
    pub async fn request(
        &mut self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<Value> {
        let id = self.begin_request(method, params, context).await?;
        loop {
            let frame = self.connection.recv().await?;
            match frame {
                Frame::Control(Control::Response(response)) if response.id == id => {
                    return match response.result {
                        Outcome::Ok { data } => Ok(data),
                        Outcome::Error { error } => Err(Error::Rpc(error)),
                    };
                }
                Frame::Control(Control::Hello(_) | Control::Request(_)) => {
                    return Err(Error::UnexpectedFrame("response or event"));
                }
                // A newer daemon's frame type: nothing here can use it.
                Frame::Control(Control::Unknown) => {}
                frame => {
                    let size = frame_size(&frame)?;
                    if self.pending.len() >= MAX_PENDING_FRAMES
                        || self.pending_bytes + size > MAX_PENDING_BYTES
                    {
                        return Err(Error::StreamOverflow);
                    }
                    self.pending_bytes += size;
                    self.pending.push_back(frame);
                }
            }
        }
    }

    /// Sends a streaming request; callers correlate terminal responses using its returned id.
    pub async fn begin_request(
        &mut self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<u64> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(Error::RequestIdsExhausted)?;
        self.connection
            .send(Frame::Control(Control::Request(Request {
                id,
                method,
                params,
                context,
            })))
            .await?;
        Ok(id)
    }

    /// Delivers queued events first, preserving their order relative to raw terminal output.
    pub async fn next_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.pending.pop_front() {
            self.pending_bytes -= frame_size(&frame)?;
            return Ok(frame);
        }
        loop {
            match self.connection.recv().await? {
                Frame::Control(Control::Unknown) => {}
                frame => return Ok(frame),
            }
        }
    }

    /// Sends terminal input without UTF-8 conversion after an attach response supplies its id.
    pub async fn send_bytes(&mut self, stream_id: u64, data: Bytes) -> Result<()> {
        Ok(self
            .connection
            .send(Frame::Bytes { stream_id, data })
            .await?)
    }
}

/// Detects explicit sandbox markers before any auto-spawn filesystem or process operations.
pub fn sandbox_reason() -> Option<&'static str> {
    ["CODEX_SANDBOX", "APP_SANDBOX_CONTAINER_ID"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
}

/// A bind probe catches socket restrictions even when the launcher omitted sandbox markers.
fn probe_socket(paths: &Paths) -> Result<()> {
    let parent = paths.socket.parent().ok_or(Error::InvalidSocketPath)?;
    let path = tempfile::Builder::new()
        .prefix("p")
        .rand_bytes(3)
        .tempfile_in(parent)?
        .into_temp_path();
    std::fs::remove_file(&path)?;
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .map_err(|_| Error::Sandbox("Unix socket bind probe failed"))?;
    drop(listener);
    Ok(())
}

/// Accounts for control payloads too, so a short event count cannot conceal excessive memory.
fn frame_size(frame: &Frame) -> Result<usize> {
    match frame {
        Frame::Bytes { data, .. } => Ok(data.len()),
        Frame::Control(control) => serde_json::to_vec(control)
            .map(|data| data.len())
            .map_err(|error| Error::Ipc(error.into())),
    }
}
