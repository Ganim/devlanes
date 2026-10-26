//! Thin daemon client shared by CLI, completion, and TUI frontends, over the
//! local endpoint of [`devlanes_ipc::local`].

mod paths;
pub mod restart;
pub mod update;

#[cfg(unix)]
use std::process::Stdio;
use std::{collections::VecDeque, path::PathBuf, time::Duration};

use bytes::Bytes;
pub use devlanes_ipc::Frame;
use devlanes_ipc::{
    Connection,
    local::{self, ConnectError, LocalStream, ProbeError},
};
pub use devlanes_proto::{Context, Hello, Method, RpcError};
use devlanes_proto::{Control, Outcome, PROTOCOL_VERSION, Request};
pub use paths::{Paths, user_home};
use serde_json::Value;
#[cfg(unix)]
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout};

pub type Result<T> = std::result::Result<T, Error>;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest a spawned daemon may stay in one startup step before the client
/// treats it as hung. Each step line restarts this wait, so it bounds one step,
/// not the whole start. It is three times the longest bound a step sets itself
/// (Windows' 10 s `git --exec-path`), which also leaves room for a machine that
/// stalls outright: a CI runner once could not start a process for 15 s.
const STARTUP_STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hang guard for the whole wait on a daemon that holds its singleton lock but
/// does not answer yet, a start this client cannot follow step by step. A
/// `devlanes daemon run` reports nine steps (singleton lock, user config, state
/// database, certificate authority, proxy, docker, shell on Windows, hosts,
/// reconcile) and its own spawning client gives up once one of them outlasts
/// [`STARTUP_STEP_TIMEOUT`]. A start that client accepts therefore answers
/// within nine such bounds; the guard allows ten, one more for the hello after
/// `ready`.
const STARTING_WAIT_LIMIT: Duration = Duration::from_secs(10 * STARTUP_STEP_TIMEOUT.as_secs());
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;
const MAX_PENDING_FRAMES: usize = 256;
/// Most bytes read from a spawned daemon's readiness pipe, newline included.
/// Bounds a misbehaving child's output; a failure reason such as an invalid user
/// config with its path and message still fits.
pub const READY_LINE_LIMIT: u64 = 4096;

/// Prefix of the lines a spawned daemon writes to its readiness pipe as each
/// startup step begins (`starting: state database`), before its one answer
/// line.
pub const STARTUP_STEP: &str = "starting: ";

/// Set to `1` in a spawned daemon's environment by clients that read
/// [`STARTUP_STEP`] lines. Without it the daemon writes only its answer line,
/// which a client from before step lines reads as the whole reply; that client
/// can spawn a newer daemon when devlanes was upgraded while it ran.
pub const READY_STEPS_ENV: &str = "DEVLANES_READY_STEPS";

/// The step a whole [`STARTUP_STEP`] line reports; `None` for any other line.
/// A step line without its newline was cut at end of file or at
/// [`READY_LINE_LIMIT`], so it ends the pipe like an answer: what follows a
/// cut could only be read as a line of its own.
fn startup_step(line: &[u8]) -> Option<String> {
    let step = line
        .strip_suffix(b"\n")?
        .strip_prefix(STARTUP_STEP.as_bytes())?;
    Some(String::from_utf8_lossy(step).trim().to_owned())
}

/// Readiness-line code of a daemon that refused an invalid user config. The line is
/// `USER_CONFIG_INVALID: {"message","file","line","column"}`; JSON keeps a message
/// with colons or newlines whole.
pub const USER_CONFIG_INVALID: &str = "USER_CONFIG_INVALID";

/// The `config_error` behind a daemon's [`USER_CONFIG_INVALID`] startup line, so
/// every client reports it like a strict load of the file; `None` for any other line
/// or one that does not parse.
pub fn user_config_startup_error(reason: &str) -> Option<RpcError> {
    let details = reason
        .strip_prefix(USER_CONFIG_INVALID)?
        .strip_prefix(':')?
        .trim();
    let details: Value = serde_json::from_str(details).ok()?;
    let file = details.get("file").and_then(Value::as_str)?;
    Some(RpcError::new(
        "config_error",
        serde_json::json!({
            "message": details.get("message").and_then(Value::as_str).unwrap_or_default(),
            "file": file,
            "line": details.get("line").and_then(Value::as_u64).unwrap_or_default(),
            "column": details.get("column").and_then(Value::as_u64).unwrap_or_default(),
        }),
        1,
    ))
}

/// What [`Error::StartupStalled`] says, as the CLI and the TUI word
/// `daemon_start_stalled`.
fn stalled_message(step: Option<&str>) -> String {
    let waited = STARTUP_STEP_TIMEOUT.as_secs();
    match step {
        Some(step) => format!(
            "the daemon has not finished starting: step {step:?} made no progress for {waited}s"
        ),
        None => format!("the daemon reported nothing for {waited}s after it was launched"),
    }
}

/// The `daemon_start_stalled` error (exit 4) behind [`Error::StartupStalled`],
/// `{step?, log, waited_s}`, so every client words and hints it the same way.
pub fn startup_stalled_error(step: Option<&str>, log: &std::path::Path) -> RpcError {
    let mut data = serde_json::json!({
        "log": log,
        "waited_s": STARTUP_STEP_TIMEOUT.as_secs(),
    });
    if let Some(step) = step {
        data["step"] = Value::from(step);
    }
    RpcError::new("daemon_start_stalled", data, 4)
}

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
    #[cfg_attr(unix, error("daemon peer uid does not match this user"))]
    #[cfg_attr(windows, error("the daemon's pipe belongs to another account"))]
    WrongPeer,
    #[error("expected {0} from daemon")]
    UnexpectedFrame(&'static str),
    #[error("daemon connection or startup timed out")]
    Timeout,
    /// A spawned daemon stayed in one startup step, or reported none since it
    /// was launched (`step` is `None`), for the client's per-step wait (30 s).
    #[error("{}; see {}", stalled_message(.step.as_deref()), .log.display())]
    StartupStalled { step: Option<String>, log: PathBuf },
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

impl From<ConnectError> for Error {
    /// I/O failures keep their kind, so [`Error::is_not_running`] still reads them.
    fn from(error: ConnectError) -> Self {
        match error {
            ConnectError::Io(error) => Self::Io(error),
            ConnectError::WrongPeer => Self::WrongPeer,
        }
    }
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
    connection: Connection<LocalStream>,
    pub peer: Hello,
    /// True when this connection's `connect_or_spawn` started the daemon; `devlanes with`
    /// stops only a daemon it started itself.
    pub spawned: bool,
    next_id: u64,
    pending: VecDeque<Frame>,
    pending_bytes: usize,
    /// Requests awaiting their response, for the durations in diagnostics.
    sent: Vec<(u64, Method, Instant)>,
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
        let begun = Instant::now();
        let result = Self::exchange(paths, strict).await;
        match &result {
            Ok(client) => tracing::debug!(
                socket = %paths.socket.display(),
                timing = "connect and hello",
                elapsed_ms = begun.elapsed().as_millis() as u64,
                daemon_version = %client.peer.version,
                daemon_build = %client.peer.build_id,
                daemon_protocol = client.peer.protocol,
                client_version = devlanes_version::VERSION,
                client_protocol = PROTOCOL_VERSION,
                managed_processes = client.peer.managed_processes,
                features = %client.peer.features.join(","),
                "connected"
            ),
            Err(error) => tracing::debug!(
                socket = %paths.socket.display(),
                elapsed_ms = begun.elapsed().as_millis() as u64,
                %error,
                "no connection"
            ),
        }
        result
    }

    /// Connects to the socket and exchanges hellos within [`CONNECT_TIMEOUT`].
    async fn exchange(paths: &Paths, strict: bool) -> Result<Self> {
        timeout(CONNECT_TIMEOUT, async {
            let stream = local::connect(&paths.socket).await?;
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
                sent: Vec::new(),
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
        // Read before the attempt too: a daemon that removes its marker while
        // this hello waits has just begun to accept, and is worth one more try.
        let was_starting = marker_exists(paths).await;
        match Self::connect(paths).await {
            Ok(client) => return Ok(client),
            Err(error) if error.is_not_running() => {}
            // EPERM/EACCES on a socket that exists: a sandbox forbids the connection.
            Err(error) if error.is_permission() => {
                return Err(Error::Sandbox(
                    "connecting to the daemon socket is not permitted",
                ));
            }
            // A daemon binds its endpoint before its startup steps and answers
            // only after reconciling, so an unanswered hello while it holds the
            // lock and its starting marker is one still starting. Without the
            // marker it has started and hangs, which the timeout reports.
            Err(Error::Timeout)
                if (was_starting && lock_held(paths).await) || starting(paths).await =>
            {
                tracing::debug!("a daemon holds the lock but does not answer yet; waiting");
                return Self::connect_while_starting(paths).await;
            }
            Err(error) => return Err(error),
        }
        if let Some(reason) = sandbox_reason() {
            tracing::debug!(reason, "no daemon, and a sandbox forbids spawning one");
            return Err(Error::Sandbox(reason));
        }
        tracing::debug!("no daemon is running; spawning one");
        let span = tracing::debug_span!("spawn_daemon", home = %paths.home.display());
        tracing::Instrument::instrument(Self::spawn_daemon(paths, remove_env), span).await
    }

    /// Spawns `devlanes daemon run`, waits for its readiness line and connects.
    /// The wait lasts while the daemon keeps reporting startup steps and ends
    /// as soon as it exits; only one step outlasting [`STARTUP_STEP_TIMEOUT`]
    /// fails it, with [`Error::StartupStalled`].
    async fn spawn_daemon(paths: &Paths, remove_env: &[&str]) -> Result<Self> {
        let begun = Instant::now();
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
        let lines = start_daemon(paths, remove_env, begun).await?;
        let Answer {
            line: message,
            step,
        } = read_answer(paths, lines, begun).await?;
        if message != b"ready\n" && message != b"already-running\n" {
            let reason = String::from_utf8_lossy(&message).trim().to_owned();
            // A refused user config is a structured error, so the CLI and the TUI word it
            // like any `config_error` instead of a transport failure.
            if let Some(error) = user_config_startup_error(&reason) {
                return Err(Error::Rpc(error));
            }
            // A daemon that died before writing a reason left it in its own log.
            let log = paths.logs.join("daemon.log");
            return Err(Error::Startup(match (reason.is_empty(), step) {
                (false, _) => reason,
                (true, Some(step)) => format!(
                    "the daemon exited in step {step:?} without a reason; see {}",
                    log.display()
                ),
                (true, None) => {
                    format!("the daemon exited without a reason; see {}", log.display())
                }
            }));
        }
        // Losing the race to another client's daemon means this invocation did not spawn it.
        let spawned = message == b"ready\n";
        tracing::debug!(
            answer = %String::from_utf8_lossy(&message).trim(),
            timing = "daemon spawn until ready",
            elapsed_ms = begun.elapsed().as_millis() as u64,
            "daemon announced readiness (reconciled, socket bound)"
        );
        // After `already-running` the other client's daemon may still be
        // binding its endpoint or in any of its steps.
        let mut client = Self::connect_while_starting(paths).await?;
        client.spawned = spawned;
        Ok(client)
    }

    /// Connects to a daemon that may hold its singleton lock without
    /// answering yet, while it starts. Retries while the lock is held, fails at
    /// once when the lock is free and nothing listens (that daemon exited), and
    /// gives up after [`STARTING_WAIT_LIMIT`] on one that never answers. An
    /// unanswered hello without the [`STARTING_MARKER`] fails at once: that
    /// daemon finished starting and hangs.
    async fn connect_while_starting(paths: &Paths) -> Result<Self> {
        let deadline = Instant::now() + STARTING_WAIT_LIMIT;
        loop {
            // A marker gone during an attempt was removed right before the
            // first accept, so the next attempt is answered.
            let was_starting = marker_exists(paths).await;
            let error = match Self::connect(paths).await {
                Ok(client) => return Ok(client),
                Err(Error::Timeout) if !was_starting && !marker_exists(paths).await => {
                    return Err(Error::Timeout);
                }
                Err(error) if error.is_not_running() || matches!(error, Error::Timeout) => error,
                Err(error) => return Err(error),
            };
            if !lock_held(paths).await {
                return Err(if error.is_not_running() {
                    Error::Startup(format!(
                        "the daemon exited before it accepted connections; see {}",
                        paths.logs.join("daemon.log").display()
                    ))
                } else {
                    error
                });
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
            if diagnostics::consume(&frame) {
                continue;
            }
            match frame {
                Frame::Control(Control::Response(response)) if response.id == id => {
                    self.answered(&response);
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
        let mut context = context;
        context.verbose = context.verbose.max(diagnostics::verbose());
        tracing::debug!(
            id,
            method = method_name(method),
            verbose = context.verbose,
            "request sent"
        );
        self.connection
            .send(Frame::Control(Control::Request(Request {
                id,
                method,
                params,
                context,
            })))
            .await?;
        self.sent.push((id, method, Instant::now()));
        Ok(id)
    }

    /// Logs the answer to one of this connection's requests with its duration.
    fn answered(&mut self, response: &devlanes_proto::Response) {
        let Some(index) = self.sent.iter().position(|(id, _, _)| *id == response.id) else {
            return;
        };
        let (id, method, sent) = self.sent.swap_remove(index);
        let error = match &response.result {
            Outcome::Ok { .. } => None,
            Outcome::Error { error } => Some(error.code.as_str()),
        };
        tracing::debug!(
            id,
            method = method_name(method),
            timing = %format!("request {}", method_name(method)),
            elapsed_ms = sent.elapsed().as_millis() as u64,
            error,
            "response received"
        );
    }

    /// Delivers queued events first, preserving their order relative to raw terminal output.
    /// Diagnostic events go to the [`diagnostics`] sink and never come out here.
    pub async fn next_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.pending.pop_front() {
            self.pending_bytes -= frame_size(&frame)?;
            if let Frame::Control(Control::Response(response)) = &frame {
                self.answered(response);
            }
            return Ok(frame);
        }
        loop {
            let frame = self.connection.recv().await?;
            if diagnostics::consume(&frame) {
                continue;
            }
            match frame {
                Frame::Control(Control::Unknown) => {}
                frame => {
                    if let Frame::Control(Control::Response(response)) = &frame {
                        self.answered(response);
                    }
                    return Ok(frame);
                }
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

/// The wire name of `method` (`start`, `lane.list`).
pub fn method_name(method: Method) -> String {
    serde_json::to_value(method)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Verbose mode: every request asks the daemon for its diagnostics, and the
/// [`devlanes_proto::events::DIAGNOSTIC`] events that come back go to one sink
/// (the CLI writes them to stderr) instead of the caller's event stream.
pub mod diagnostics {
    use std::sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    };

    use devlanes_proto::{Control, Event, events};

    use crate::Frame;

    /// The process's verbosity, 0 when off.
    static VERBOSE: AtomicU8 = AtomicU8::new(0);
    /// Where diagnostic events go.
    type Sink = Box<dyn Fn(&Event) + Send + Sync>;
    static SINK: OnceLock<Sink> = OnceLock::new();

    /// Turns on verbose requests at `level` (1 debug, 2 trace) with `sink`
    /// receiving the daemon's diagnostics. Only the first sink is kept.
    pub fn enable(level: u8, sink: impl Fn(&Event) + Send + Sync + 'static) {
        let _ = SINK.set(Box::new(sink));
        VERBOSE.store(level, Ordering::SeqCst);
    }

    /// The level requests ask for.
    pub fn verbose() -> u8 {
        VERBOSE.load(Ordering::Relaxed)
    }

    /// Hands a diagnostic event to the sink; true when `frame` was one. Without
    /// a sink they are dropped, so they never reach rendering.
    pub(crate) fn consume(frame: &Frame) -> bool {
        match frame {
            Frame::Control(Control::Event(event)) if event.event == events::DIAGNOSTIC => {
                if let Some(sink) = SINK.get() {
                    sink(event);
                }
                true
            }
            _ => false,
        }
    }
}

/// Detects explicit sandbox markers before any auto-spawn filesystem or process operations.
pub fn sandbox_reason() -> Option<&'static str> {
    ["CODEX_SANDBOX", "APP_SANDBOX_CONTAINER_ID"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
}

/// Variables an auto-spawned daemon inherits from the spawning shell, by exact name.
const DAEMON_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TZ",
    "TMPDIR",
    "TERM",
    // Local ssh to boxes (`remote connect`) authenticates through the agent.
    "SSH_AUTH_SOCK",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    // CI markers, so the daemon's update check still skips in CI; keep in step
    // with `ci_detected` in devlanes-daemon.
    "CI",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "BUILDKITE",
    "CIRCLECI",
    "TF_BUILD",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "BITBUCKET_BUILD_NUMBER",
];

/// Prefixes of further inherited variables: locale, XDG directories, the Docker
/// CLI's settings and devlanes' own.
const DAEMON_ENV_PREFIXES: &[&str] = &["LC_", "XDG_", "DOCKER_", "DEVLANES_"];

/// Nothing beyond the shared list on Unix.
#[cfg(unix)]
const PLATFORM_ENV: &[&str] = &[];

/// What Windows programs the daemon runs expect: Winsock fails to start
/// without `SystemRoot`, and tools such as git and docker read the profile and
/// program directories.
#[cfg(windows)]
const PLATFORM_ENV: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
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
    "HOMEDRIVE",
    "HOMEPATH",
    "PSModulePath",
];

/// The allowlisted part of `vars` for an auto-spawned daemon, minus `remove`.
/// Anything else a shell carries (tokens, project variables) stays out of a
/// process that serves every later shell.
fn daemon_env(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    remove: &[&str],
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.filter(|(name, _)| {
        let Some(name) = name.to_str() else {
            return false;
        };
        !remove.iter().any(|removed| same_name(name, removed))
            && (DAEMON_ENV
                .iter()
                .chain(PLATFORM_ENV)
                .any(|listed| same_name(name, listed))
                || DAEMON_ENV_PREFIXES.iter().any(|prefix| {
                    name.get(..prefix.len())
                        .is_some_and(|start| same_name(start, prefix))
                }))
    })
    .collect()
}

/// Whether two environment variable names are the same variable: by exact
/// name on Unix.
#[cfg(unix)]
fn same_name(name: &str, listed: &str) -> bool {
    name == listed
}

/// Whether two environment variable names are the same variable: Windows
/// ignores case, and its environment usually spells `Path`.
#[cfg(windows)]
fn same_name(name: &str, listed: &str) -> bool {
    name.eq_ignore_ascii_case(listed)
}

/// The lines of a spawned daemon's readiness pipe, each cut to
/// [`READY_LINE_LIMIT`]: step reports, then its answer, or an empty line when
/// the pipe closed first. The channel closes after the answer.
type ReadyLines = tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>;

/// Most lines the readiness reader holds before the client takes them.
const READY_LINES_QUEUED: usize = 16;

/// Spawns `devlanes daemon run` and returns the lines of its readiness pipe.
#[cfg(unix)]
async fn start_daemon(paths: &Paths, remove_env: &[&str], begun: Instant) -> Result<ReadyLines> {
    let mut command = Command::new(std::env::current_exe()?);
    // The daemon outlives this shell and serves every later one, so it keeps
    // only what it needs itself; services get each request's client env.
    command
        .env_clear()
        .envs(daemon_env(std::env::vars_os(), remove_env));
    let mut child = command
        .args(["daemon", "run"])
        .env("DEVLANES_HOME", &paths.home)
        .env("DEVLANES_READY_FD", "1")
        .env(READY_STEPS_ENV, "1")
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
    tracing::debug!(
        pid = child.id(),
        elapsed_ms = begun.elapsed().as_millis() as u64,
        "daemon process started; waiting for its readiness line"
    );
    // Tokio reaps the child after this handle is dropped; never kill a potentially live daemon.
    drop(child);
    Ok(read_lines(stdout))
}

/// Whether the reader goes on after `read`: only a whole step line has more
/// lines after it.
fn more_lines(read: &std::io::Result<Vec<u8>>) -> bool {
    read.as_ref().is_ok_and(|line| startup_step(line).is_some())
}

/// Reads the lines of a readiness pipe in a task, up to the first line that
/// is no whole step line (the answer, a cut line, or an empty one at end of
/// file) or a read error, which it sends last.
#[cfg(any(unix, test))]
fn read_lines(pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> ReadyLines {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let (sender, lines) = tokio::sync::mpsc::channel(READY_LINES_QUEUED);
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        loop {
            let mut line = Vec::new();
            let read = (&mut reader)
                .take(READY_LINE_LIMIT)
                .read_until(b'\n', &mut line)
                .await
                .map(|_| line);
            let more = more_lines(&read);
            if sender.send(read).await.is_err() || !more {
                return;
            }
        }
    });
    lines
}

/// [`read_lines`] on a plain thread, for pipes without async reads (Windows'
/// anonymous pipes). Unlike `spawn_blocking`, the thread never holds up the
/// runtime's shutdown when a daemon hangs without answering; it ends with the
/// pipe or the process.
#[cfg(any(windows, test))]
fn read_lines_blocking(pipe: impl std::io::Read + Send + 'static) -> std::io::Result<ReadyLines> {
    use std::io::{BufRead, BufReader, Read};
    let (sender, lines) = tokio::sync::mpsc::channel(READY_LINES_QUEUED);
    std::thread::Builder::new()
        .name("daemon-ready".into())
        .spawn(move || {
            let mut reader = BufReader::new(pipe);
            loop {
                let mut line = Vec::new();
                let read = (&mut reader)
                    .take(READY_LINE_LIMIT)
                    .read_until(b'\n', &mut line)
                    .map(|_| line);
                let more = more_lines(&read);
                if sender.blocking_send(read).is_err() || !more {
                    return;
                }
            }
        })?;
    Ok(lines)
}

/// Spawns `devlanes daemon run` and returns the lines of its readiness pipe.
///
/// The daemon inherits only the readiness pipe (as stdout, so
/// `DEVLANES_READY_FD=1` holds) and NUL: a shell reading this command's output,
/// as Git Bash's `$(...)` does, would otherwise wait for the daemon to exit. It
/// gets a hidden console of its own (`CREATE_NO_WINDOW`), which the programs it
/// starts share without a window, and its own process group, so the
/// terminal's Ctrl-C and closing never reach it. It also leaves the terminal's
/// job, which some terminals end when they close; a job that refuses
/// breakaway fails the spawn with access denied, and the daemon then starts
/// inside it (`doctor` reports a job that kills on close).
#[cfg(windows)]
async fn start_daemon(paths: &Paths, remove_env: &[&str], begun: Instant) -> Result<ReadyLines> {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::io::AsHandle;

    use devlanes_sys_windows::spawn::{self, CreationFlags, Environment, Spawn, Stdio, Stream};

    let exe = std::env::current_exe()?;
    // The daemon's own variables replace inherited ones; Windows would refuse
    // two spellings of one name. The askpass relay of an ssh run never reaches
    // a daemon, which would hand it to every unit.
    let own = [
        "DEVLANES_HOME",
        "DEVLANES_READY_FD",
        READY_STEPS_ENV,
        devlanes_proto::remote::ASKPASS_ENV,
    ];
    let mut variables: Vec<(OsString, OsString)> = daemon_env(std::env::vars_os(), remove_env)
        .into_iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_none_or(|name| !own.iter().any(|own| same_name(name, own)))
        })
        .collect();
    variables.push(("DEVLANES_HOME".into(), paths.home.clone().into()));
    variables.push(("DEVLANES_READY_FD".into(), "1".into()));
    variables.push((READY_STEPS_ENV.into(), "1".into()));
    let environment = Environment::new(variables)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    // A path holds no `"`, so quoting is enough for the program's argv[0].
    let mut command_line = OsString::from("\"");
    command_line.push(&exe);
    command_line.push("\" daemon run");
    let (reader, writer) = spawn::pipe()?;
    let spec = |breakaway_from_job: bool| Spawn {
        program: &exe,
        command_line: OsStr::new(&command_line),
        environment: Some(&environment),
        cwd: Some(&paths.home),
        stdio: Stdio::Handles {
            stdin: Stream::Null,
            stdout: Stream::Handle(writer.as_handle()),
            stderr: Stream::Null,
        },
        inherit: &[],
        job: None,
        flags: CreationFlags {
            no_window: true,
            new_process_group: true,
            breakaway_from_job,
            ..CreationFlags::default()
        },
    };
    let child = match spawn::spawn(&spec(true)) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::debug!("the terminal's job refuses breakaway; the daemon starts inside it");
            spawn::spawn(&spec(false))?
        }
        result => result?,
    };
    // Only the daemon may hold the writing end, or the reader never sees its end.
    drop(writer);
    tracing::debug!(
        pid = child.pid(),
        elapsed_ms = begun.elapsed().as_millis() as u64,
        "daemon process started; waiting for its readiness line"
    );
    // Dropping the handle leaves the daemon running.
    drop(child);
    Ok(read_lines_blocking(reader)?)
}

/// The end of a spawned daemon's readiness pipe.
#[derive(Debug)]
struct Answer {
    /// `ready`, `already-running`, a failure reason, or empty when the pipe
    /// closed (the daemon exited) without one.
    line: Vec<u8>,
    /// The last startup step the daemon reported.
    step: Option<String>,
}

/// Reads `lines` up to the daemon's answer. Each step report restarts the
/// [`STARTUP_STEP_TIMEOUT`] wait, so only a single step is bounded, and a
/// closed pipe ends the wait at once. `begun` is when the spawn began, for the
/// logged step times.
async fn read_answer(paths: &Paths, mut lines: ReadyLines, begun: Instant) -> Result<Answer> {
    let mut step: Option<String> = None;
    loop {
        let line = match timeout(STARTUP_STEP_TIMEOUT, lines.recv()).await {
            Ok(Some(line)) => line?,
            Ok(None) => return Err(Error::Startup("the readiness reader stopped".into())),
            // Without a step the process has not reached its first one, or
            // is a daemon from before step lines.
            Err(_) => {
                return Err(Error::StartupStalled {
                    step,
                    log: paths.logs.join("daemon.log"),
                });
            }
        };
        match startup_step(&line) {
            Some(next) => {
                tracing::debug!(
                    step = %next,
                    elapsed_ms = begun.elapsed().as_millis() as u64,
                    "daemon startup step"
                );
                step = Some(next);
            }
            None => return Ok(Answer { line, step }),
        }
    }
}

/// What a failed endpoint probe says.
#[cfg(unix)]
const PROBE_FAILED: &str = "Unix socket bind probe failed";

/// What a failed endpoint probe says.
#[cfg(windows)]
const PROBE_FAILED: &str = "named pipe probe failed";

/// Name of the file under `paths.run` that a daemon keeps from taking its lock
/// until it accepts connections; the daemon names it
/// `devlanes_daemon::STARTING_MARKER`.
pub const STARTING_MARKER: &str = "daemon.starting";

/// Whether the [`STARTING_MARKER`] exists. Alone it proves nothing: a daemon
/// that died while starting leaves it behind with its lock free.
async fn marker_exists(paths: &Paths) -> bool {
    tokio::fs::try_exists(paths.run.join(STARTING_MARKER))
        .await
        .unwrap_or(false)
}

/// Whether a daemon holds its lock and has not accepted a connection yet, so
/// an unanswered hello is worth waiting for.
async fn starting(paths: &Paths) -> bool {
    marker_exists(paths).await && lock_held(paths).await
}

/// Whether a daemon holds its singleton lock under `paths.run`, so it runs or
/// is starting. A probe that fails cannot tell, and counts as free so that no
/// caller waits on it.
async fn lock_held(paths: &Paths) -> bool {
    let run = paths.run.clone();
    match tokio::task::spawn_blocking(move || daemon_lock_held(&run)).await {
        Ok(Ok(held)) => held,
        Ok(Err(error)) => {
            tracing::debug!(%error, "cannot probe the daemon lock");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "the daemon lock probe failed");
            false
        }
    }
}

/// Probes the daemon's `daemon.lock` in `run` (blocking I/O) with the call the
/// daemon takes it with: std's `try_lock` is `flock(LOCK_EX | LOCK_NB)` on
/// Unix, as the daemon's `fs2` lock is, and `LockFileEx` with
/// `LOCKFILE_FAIL_IMMEDIATELY` on Windows, where the daemon uses std too. A
/// lock the probe gets is released at once. A missing file means no daemon
/// ever took it.
fn daemon_lock_held(run: &std::path::Path) -> std::io::Result<bool> {
    let lock = match std::fs::File::open(run.join("daemon.lock")) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match lock.try_lock() {
        // While the probe holds it, a daemon that starts answers
        // `already-running`. Its client then waits the same way, and fails
        // like one whose daemon exited only when no other daemon took the lock
        // meanwhile.
        Ok(()) => {
            lock.unlock()?;
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// A bind probe catches socket restrictions even when the launcher omitted sandbox markers.
fn probe_socket(paths: &Paths) -> Result<()> {
    match local::probe(&paths.socket) {
        Ok(()) => Ok(()),
        Err(ProbeError::NoParent) => Err(Error::InvalidSocketPath),
        Err(ProbeError::Setup(error)) => Err(error.into()),
        Err(ProbeError::Endpoint(_)) => Err(Error::Sandbox(PROBE_FAILED)),
    }
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use std::time::Duration;

    use tokio::time::Instant;

    use tokio::io::AsyncWriteExt;

    use super::{
        Answer, CONNECT_TIMEOUT, Client, Error, Paths, READY_LINE_LIMIT, ReadyLines,
        STARTING_MARKER, STARTING_WAIT_LIMIT, STARTUP_STEP_TIMEOUT, daemon_env, daemon_lock_held,
        local, read_answer, read_lines, read_lines_blocking, user_config_startup_error,
    };

    /// A readiness pipe that already holds `lines`, and its sender, which
    /// keeps it open until dropped.
    fn pipe(
        lines: &[&str],
    ) -> (
        ReadyLines,
        tokio::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        for line in lines {
            sender
                .try_send(Ok(line.as_bytes().to_vec()))
                .expect("queued");
        }
        (receiver, sender)
    }

    /// Steps keep the client waiting past the step bound in total, and the
    /// answer comes back with the last step.
    #[tokio::test(start_paused = true)]
    async fn steps_keep_a_slow_start_waiting() {
        let paths = Paths::new("acme-home").expect("paths");
        let (lines, sender) = pipe(&[]);
        let feeder = tokio::spawn(async move {
            for step in ["singleton lock", "state database", "reconcile"] {
                tokio::time::sleep(STARTUP_STEP_TIMEOUT - Duration::from_secs(1)).await;
                let line = format!("{}{step}\n", super::STARTUP_STEP);
                sender.send(Ok(line.into_bytes())).await.expect("sent");
            }
            sender.send(Ok(b"ready\n".to_vec())).await.expect("sent");
        });
        let begun = Instant::now();
        let Answer { line, step } = read_answer(&paths, lines, begun).await.expect("answer");
        assert_eq!(line, b"ready\n");
        assert_eq!(step.as_deref(), Some("reconcile"));
        assert!(begun.elapsed() > STARTUP_STEP_TIMEOUT * 2);
        feeder.await.expect("feeder");
    }

    /// A step that outlasts the bound fails with that step, a daemon that
    /// reports none fails without one, and a pipe that closes after a step
    /// fails at once with an empty answer.
    #[tokio::test(start_paused = true)]
    async fn a_stuck_step_stalls_and_an_exit_ends_the_wait() {
        let paths = Paths::new("acme-home").expect("paths");
        let (lines, _open) = pipe(&["starting: state database\n"]);
        let stalled = read_answer(&paths, lines, Instant::now()).await;
        let Err(error @ Error::StartupStalled { step, .. }) = &stalled else {
            panic!("{stalled:?}");
        };
        assert_eq!(step.as_deref(), Some("state database"));
        assert!(
            error.to_string().starts_with(
                "the daemon has not finished starting: step \"state database\" made no progress for 30s; see "
            ),
            "{error}"
        );

        let (lines, _open) = pipe(&[]);
        let stalled = read_answer(&paths, lines, Instant::now()).await;
        let Err(error @ Error::StartupStalled { step: None, .. }) = &stalled else {
            panic!("{stalled:?}");
        };
        assert!(
            error
                .to_string()
                .starts_with("the daemon reported nothing for 30s after it was launched; see "),
            "{error}"
        );

        let (lines, closed) = pipe(&["starting: proxy\n", ""]);
        drop(closed);
        let begun = Instant::now();
        let Answer { line, step } = read_answer(&paths, lines, begun).await.expect("answer");
        assert!(line.is_empty());
        assert_eq!(step.as_deref(), Some("proxy"));
        assert_eq!(begun.elapsed(), Duration::ZERO);
    }

    /// Every line `lines` delivers until it closes.
    async fn collect(mut lines: ReadyLines) -> Vec<Vec<u8>> {
        let mut all = Vec::new();
        while let Some(line) = lines.recv().await {
            all.push(line.expect("read"));
        }
        all
    }

    /// What the async and the thread reader deliver for a pipe that holds
    /// `bytes` and then closes.
    async fn read_both(bytes: Vec<u8>) -> [Vec<Vec<u8>>; 2] {
        // Large enough for every byte, so the writer never waits for a
        // reader that stopped at its answer.
        let (mut writer, pipe) = tokio::io::duplex(bytes.len().max(1));
        writer.write_all(&bytes).await.expect("written");
        drop(writer);
        let lines = collect(read_lines(pipe)).await;
        let blocking = read_lines_blocking(std::io::Cursor::new(bytes)).expect("reader thread");
        [lines, collect(blocking).await]
    }

    /// Both readers pass step lines and stop after the first other line: the
    /// answer, a step line cut at end of file or at the line bound (whose
    /// rest is never read as a line of its own), or the empty line of a pipe
    /// that closed first.
    #[tokio::test]
    async fn readers_stop_at_the_first_line_that_is_no_whole_step() {
        let answered = b"starting: proxy\nready\nleftover\n".to_vec();
        for lines in read_both(answered).await {
            assert_eq!(lines, [&b"starting: proxy\n"[..], b"ready\n"]);
        }

        let cut = b"starting: singleton lock\nstarting: sta".to_vec();
        for lines in read_both(cut).await {
            assert_eq!(
                lines,
                [&b"starting: singleton lock\n"[..], b"starting: sta"]
            );
        }

        let limit = usize::try_from(READY_LINE_LIMIT).expect("fits");
        let mut long = format!("starting: {}\n", "x".repeat(limit)).into_bytes();
        long.extend_from_slice(b"ready\n");
        for lines in read_both(long.clone()).await {
            assert_eq!(lines, [&long[..limit]]);
        }

        for lines in read_both(Vec::new()).await {
            assert_eq!(lines, [b""]);
        }
    }

    /// Takes `file`'s lock the way the daemon takes `daemon.lock`.
    fn lock_as_the_daemon(file: &std::fs::File) {
        #[cfg(unix)]
        fs2::FileExt::try_lock_exclusive(file).expect("locked");
        #[cfg(windows)]
        file.try_lock().expect("locked");
    }

    /// Releases a lock [`lock_as_the_daemon`] took. Windows may release a lock
    /// some time after its handle closes, so the tests unlock explicitly.
    fn release(file: std::fs::File) {
        #[cfg(unix)]
        fs2::FileExt::unlock(&file).expect("unlocked");
        #[cfg(windows)]
        file.unlock().expect("unlocked");
    }

    /// The probe sees a lock taken as the daemon takes it, and leaves a free
    /// lock free.
    #[test]
    fn the_lock_probe_sees_the_daemon_lock() {
        let run = tempfile::tempdir().expect("run");
        assert!(!daemon_lock_held(run.path()).expect("no file"));
        let path = run.path().join("daemon.lock");
        let file = std::fs::File::create(&path).expect("lock file");
        assert!(!daemon_lock_held(run.path()).expect("free"));
        lock_as_the_daemon(&file);
        assert!(daemon_lock_held(run.path()).expect("held"));
        release(file);
        assert!(!daemon_lock_held(run.path()).expect("released"));
    }

    /// A daemon that holds its lock and accepts but never answers: with the
    /// starting marker the client waits to the hang guard, without it (the
    /// daemon finished starting and hangs) it gives up after one connect
    /// timeout. The paused clock jumps over each wait.
    #[tokio::test(start_paused = true)]
    async fn only_a_starting_daemon_is_waited_for() {
        let home = tempfile::tempdir().expect("home");
        let paths = Paths::new(home.path()).expect("paths");
        // A long temporary path puts the Unix socket outside `run`, in a
        // directory only `prepare` creates.
        paths.prepare().expect("directories");
        let file = std::fs::File::create(paths.run.join("daemon.lock")).expect("lock file");
        lock_as_the_daemon(&file);
        let mut listener = local::LocalListener::bind(&paths.socket)
            .await
            .expect("listener");
        // Holds every connection open without a hello.
        let silent = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok(stream) = listener.accept().await {
                held.push(stream);
            }
        });
        let marker = paths.run.join(STARTING_MARKER);
        for (starting, waited) in [(false, CONNECT_TIMEOUT), (true, STARTING_WAIT_LIMIT)] {
            if starting {
                std::fs::write(&marker, b"").expect("marker");
            }
            let begun = Instant::now();
            let ended = Client::connect_while_starting(&paths).await.map(drop);
            assert!(matches!(ended, Err(Error::Timeout)), "{ended:?}");
            let elapsed = begun.elapsed();
            assert!(
                elapsed >= waited && elapsed < waited + CONNECT_TIMEOUT * 2,
                "starting {starting}: {elapsed:?}"
            );
        }
        // The marker goes while a hello waits, as when the daemon begins to
        // accept: one more attempt, then the timeout of a daemon that hangs.
        let removal = tokio::spawn({
            let marker = marker.clone();
            async move {
                tokio::time::sleep(CONNECT_TIMEOUT / 2).await;
                std::fs::remove_file(&marker).expect("marker removed");
            }
        });
        let begun = Instant::now();
        let ended = Client::connect_while_starting(&paths).await.map(drop);
        assert!(matches!(ended, Err(Error::Timeout)), "{ended:?}");
        let elapsed = begun.elapsed();
        assert!(
            elapsed >= CONNECT_TIMEOUT * 2 && elapsed < CONNECT_TIMEOUT * 3,
            "marker removed mid-attempt: {elapsed:?}"
        );
        removal.await.expect("removal");
        silent.abort();
        release(file);
    }

    /// While the lock is held and nothing listens the client keeps waiting;
    /// once it is free the wait fails at once, as when the daemon exited.
    #[tokio::test]
    async fn a_held_lock_keeps_the_client_waiting() {
        let home = tempfile::tempdir().expect("home");
        let paths = Paths::new(home.path()).expect("paths");
        std::fs::create_dir_all(&paths.run).expect("run");
        let file = std::fs::File::create(paths.run.join("daemon.lock")).expect("lock file");
        lock_as_the_daemon(&file);
        let waiting = tokio::spawn({
            let paths = paths.clone();
            async move { Client::connect_while_starting(&paths).await.map(drop) }
        });
        // It cannot end while the lock is held, however few probes this
        // sleep allows.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!waiting.is_finished());
        release(file);
        let ended = tokio::time::timeout(Duration::from_secs(120), waiting)
            .await
            .expect("hang guard")
            .expect("task");
        assert!(
            matches!(&ended, Err(Error::Startup(message)) if message.starts_with("the daemon exited before it accepted connections")),
            "{ended:?}"
        );
    }

    /// A refused user config becomes a located `config_error`; other or cut lines do not.
    #[test]
    fn user_config_startup_lines_become_config_errors() {
        let error = user_config_startup_error(
            r#"USER_CONFIG_INVALID: {"message":"tui.theme: unknown variant `neon`: pick one","file":"/home/acme/.devlanes/config.yaml","line":4,"column":10}"#,
        )
        .expect("config error");
        assert_eq!((error.code.as_str(), error.exit_code), ("config_error", 1));
        assert_eq!(
            error.data,
            serde_json::json!({
                "message": "tui.theme: unknown variant `neon`: pick one",
                "file": "/home/acme/.devlanes/config.yaml",
                "line": 4,
                "column": 10,
            })
        );
        assert!(user_config_startup_error("USER_CONFIG_INVALID: cut{").is_none());
        assert!(user_config_startup_error("STATE_LEGACY_SCHEMA: old").is_none());
    }

    /// Allowlisted names and prefixes pass, secrets and removed names do not.
    #[test]
    fn daemon_env_is_allowlisted() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("SSH_CONNECTION", "1.2.3.4"),
            ("LC_ALL", "C"),
            ("DOCKER_HOST", "unix:///var/run/docker.sock"),
            ("DEVLANES_HOME", "/tmp/dl"),
            ("XDG_CONFIG_HOME", "/tmp/xdg"),
            ("https_proxy", "http://proxy.example.invalid"),
            ("CI", "true"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("GITHUB_TOKEN", "secret"),
            ("DATABASE_URL", "postgres://example.invalid"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let kept: Vec<String> = daemon_env(vars.into_iter(), &["SSH_AUTH_SOCK"])
            .into_iter()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        assert_eq!(
            kept,
            [
                "PATH",
                "LC_ALL",
                "DOCKER_HOST",
                "DEVLANES_HOME",
                "XDG_CONFIG_HOME",
                "https_proxy",
                "CI"
            ]
        );
    }

    /// Windows keeps the system variables programs need and matches names
    /// without case, as Windows does.
    #[cfg(windows)]
    #[test]
    fn daemon_env_keeps_windows_variables() {
        let vars = [
            ("Path", r"C:\Windows"),
            ("SystemRoot", r"C:\Windows"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("devlanes_home", r"C:\dl"),
            ("ssh_auth_sock", r"\\.\pipe\openssh-ssh-agent"),
            ("GITHUB_TOKEN", "secret"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let kept: Vec<String> = daemon_env(vars.into_iter(), &["SSH_AUTH_SOCK"])
            .into_iter()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        assert_eq!(
            kept,
            ["Path", "SystemRoot", "ProgramFiles(x86)", "devlanes_home"]
        );
    }
}
