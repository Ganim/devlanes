//! Thin daemon client shared by CLI, completion, and TUI frontends. It talks over the
//! local endpoint from [`devlanes_ipc::local`].

mod paths;
pub mod restart;
pub mod update;

use std::{collections::VecDeque, path::PathBuf, process::Stdio, time::Duration};

use bytes::Bytes;
pub use devlanes_ipc::Frame;
use devlanes_ipc::{
    Connection,
    local::{self, LocalStream, ProbeError},
};
pub use devlanes_proto::{Context, Hello, Method, RpcError};
use devlanes_proto::{Control, Outcome, PROTOCOL_VERSION, Request};
pub use paths::{Paths, user_home};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
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
                client_version = env!("CARGO_PKG_VERSION"),
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
            if !stream.peer_is_current_user()? {
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
            tracing::debug!(reason, "no daemon, and a sandbox forbids spawning one");
            return Err(Error::Sandbox(reason));
        }
        tracing::debug!("no daemon is running; spawning one");
        let span = tracing::debug_span!("spawn_daemon", home = %paths.home.display());
        tracing::Instrument::instrument(Self::spawn_daemon(paths, remove_env), span).await
    }

    /// Spawns `devlanes daemon run`, waits for its readiness line and connects.
    async fn spawn_daemon(paths: &Paths, remove_env: &[&str]) -> Result<Self> {
        let begun = Instant::now();
        let owned_paths = paths.clone();
        tokio::task::spawn_blocking(move || {
            refuse_elevation()?;
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
        // The daemon outlives this shell and serves every later one, so it keeps
        // only what it needs itself; services get each request's client env.
        command
            .env_clear()
            .envs(daemon_env(std::env::vars_os(), remove_env));
        command
            .args(["daemon", "run"])
            .env("DEVLANES_HOME", &paths.home)
            .env("DEVLANES_READY_FD", "1")
            .current_dir(&paths.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        let mut child = spawn_daemon(&mut command)?;
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
        tracing::debug!(
            answer = %String::from_utf8_lossy(&message).trim(),
            timing = "daemon spawn until ready",
            elapsed_ms = begun.elapsed().as_millis() as u64,
            "daemon announced readiness (reconciled, socket bound)"
        );
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

/// Unix spawns the daemon whoever the user is.
#[cfg(unix)]
fn refuse_elevation() -> Result<()> {
    Ok(())
}

/// The daemon would refuse to run elevated anyway; failing here, before
/// `prepare()`, also keeps an elevated shell from creating the home.
#[cfg(windows)]
fn refuse_elevation() -> Result<()> {
    use devlanes_sys::windows::token::{ELEVATED_REASON, elevation_refused};
    if elevation_refused()? {
        return Err(Error::Startup(ELEVATED_REASON.into()));
    }
    Ok(())
}

/// Starts the daemon; on Unix it leaves the terminal's session itself (`setsid`).
#[cfg(unix)]
fn spawn_daemon(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    command.spawn()
}

/// Starts the daemon without a console, so the terminal's Ctrl-C and close events
/// never reach it, and outside the terminal's job object, which some terminals kill
/// when they close. A job that forbids breaking away refuses the second flag with
/// access denied; the daemon then starts inside it, and `doctor` reports whether
/// that job kills on close.
///
/// The daemon also must not inherit this process's standard handles: a shell
/// reading devlanes' output (`$(devlanes status)`) would wait for the daemon to
/// exit.
#[cfg(windows)]
fn spawn_daemon(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    use devlanes_sys::windows::process::{
        CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS, without_inherited_std_handles,
    };
    without_inherited_std_handles(|| {
        command.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::debug!(
                    "the terminal's job refuses breakaway; the daemon starts inside it"
                );
                command.creation_flags(DETACHED_PROCESS);
                command.spawn()
            }
            result => result,
        }
    })
}

/// Detects explicit sandbox markers before any auto-spawn filesystem or process operations.
pub fn sandbox_reason() -> Option<&'static str> {
    ["CODEX_SANDBOX", "APP_SANDBOX_CONTAINER_ID"]
        .into_iter()
        .chain(PLATFORM_SANDBOX_MARKERS.iter().copied())
        .find(|name| std::env::var_os(name).is_some())
        .or_else(sandboxed_token)
}

/// No markers beyond the shared ones on Unix.
#[cfg(unix)]
const PLATFORM_SANDBOX_MARKERS: &[&str] = &[];

/// Codex sets `CODEX_SANDBOX` only for macOS Seatbelt. On Windows its sandboxed
/// commands carry `CODEX_SANDBOX_NETWORK_DISABLED=1` (codex-rs/core/src/spawn.rs
/// and sandboxing/mod.rs) and, from its unelevated Windows sandbox,
/// `SBX_NONET_ACTIVE=1` (codex-rs/windows-sandbox-rs/src/env.rs and
/// spawn_prep.rs), both only when network access is off.
#[cfg(windows)]
const PLATFORM_SANDBOX_MARKERS: &[&str] = &["CODEX_SANDBOX_NETWORK_DISABLED", "SBX_NONET_ACTIVE"];

/// Unix relies on the markers and the bind probe.
#[cfg(unix)]
fn sandboxed_token() -> Option<&'static str> {
    None
}

/// A restricted, AppContainer or low-integrity token, as Codex's unelevated
/// Windows sandbox runs commands with. A daemon started from it would inherit
/// the token, and clients refuse such a daemon anyway. A token that cannot be
/// read is left to the checks that follow.
#[cfg(windows)]
fn sandboxed_token() -> Option<&'static str> {
    use devlanes_sys::windows::token::{MEDIUM_INTEGRITY, current_facts};
    let facts = current_facts().ok()?;
    (facts.restricted || facts.app_container || facts.integrity < MEDIUM_INTEGRITY)
        .then_some("restricted process token")
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

/// devlanes variables that match a prefix above but never reach an auto-spawned
/// daemon. The askpass pair belongs to one ssh run: a daemon spawned from a
/// shell that has them would hand them to every unit. The daemon drops them
/// from request environments too.
const DAEMON_ENV_DENIED: &[&str] = devlanes_proto::remote::ASKPASS_ENV;

/// Nothing extra on Unix.
#[cfg(unix)]
const PLATFORM_ENV: &[&str] = &[];

/// What Windows programs expect: Winsock fails to start without `SystemRoot`
/// (WSA error 10106), and helpers such as git and docker read the profile and
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
    "COMPUTERNAME",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "TEMP",
    "TMP",
    "HOMEDRIVE",
    "HOMEPATH",
    "OS",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
    "PSModulePath",
];

/// Names compare exactly on Unix.
#[cfg(unix)]
fn same_name(name: &str, listed: &str) -> bool {
    name == listed
}

/// Windows names compare without case: the environment usually spells `Path`.
#[cfg(windows)]
fn same_name(name: &str, listed: &str) -> bool {
    name.eq_ignore_ascii_case(listed)
}

/// Prefix match with the platform's name comparison.
fn has_prefix(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len())
        .is_some_and(|start| same_name(start, prefix))
}

/// The part of this process's environment a daemon started from it keeps, as
/// [`Client::connect_or_spawn`] passes it: tokens and project variables stay
/// out. For launchers that start `devlanes daemon run` themselves (the Windows
/// logon launcher); they clear the rest of the environment.
pub fn daemon_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    daemon_env(std::env::vars_os(), &[])
}

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
        !remove
            .iter()
            .chain(DAEMON_ENV_DENIED)
            .any(|removed| same_name(name, removed))
            && (DAEMON_ENV
                .iter()
                .chain(PLATFORM_ENV)
                .any(|listed| same_name(name, listed))
                || DAEMON_ENV_PREFIXES
                    .iter()
                    .any(|prefix| has_prefix(name, prefix)))
    })
    .collect()
}

/// A bind probe catches socket restrictions even when the launcher omitted sandbox markers.
fn probe_socket(paths: &Paths) -> Result<()> {
    paths.socket.parent().ok_or(Error::InvalidSocketPath)?;
    match local::probe(&paths.socket) {
        Ok(()) => Ok(()),
        Err(ProbeError::Setup(error)) => Err(error.into()),
        Err(ProbeError::Endpoint(_)) => Err(Error::Sandbox("local endpoint bind probe failed")),
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

    use super::daemon_env;

    /// Allowlisted names and prefixes pass; secrets, removed names and the
    /// denied devlanes variables do not.
    #[test]
    fn daemon_env_is_allowlisted() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("SSH_CONNECTION", "1.2.3.4"),
            ("LC_ALL", "C"),
            ("DOCKER_HOST", "unix:///var/run/docker.sock"),
            ("DEVLANES_HOME", "/tmp/dl"),
            ("DEVLANES_ASKPASS", "1"),
            ("DEVLANES_ASKPASS_SOCKET", "/tmp/dl/run/ssh/a.sock"),
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

    /// Windows keeps the system variables programs need and matches names without
    /// case, as Windows does.
    #[cfg(windows)]
    #[test]
    fn daemon_env_keeps_windows_variables() {
        let vars = [
            ("Path", r"C:\Windows"),
            ("SystemRoot", r"C:\Windows"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("devlanes_home", r"C:\dl"),
            ("devlanes_askpass_socket", r"\\.\pipe\devlanes-askpass-1-0"),
            ("GITHUB_TOKEN", "secret"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let kept: Vec<String> = daemon_env(vars.into_iter(), &["systemroot"])
            .into_iter()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        assert_eq!(kept, ["Path", "ProgramFiles(x86)", "devlanes_home"]);
    }
}
