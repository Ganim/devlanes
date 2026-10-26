//! Readiness checks (plan 4.6). A check never restarts or stops anything; it only
//! moves a unit to `ready` or `failed`.

use std::{
    collections::BTreeMap,
    io::Write,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use devlanes_proto::RpcError;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, sleep, timeout},
};

use crate::units::{UnitHost, UnitState};

/// What "ready" means for one unit, after config evaluation.
#[derive(Clone)]
pub enum Readiness {
    /// Running is enough.
    Started,
    Tcp {
        port: u16,
    },
    Http {
        port: u16,
        path: String,
        status: Option<u16>,
        /// Host header to send; `None` sends `127.0.0.1:<port>`.
        host: Option<String>,
    },
    /// Regex over ANSI-stripped output lines.
    Log {
        pattern: String,
    },
    /// Shell command retried until it exits 0, in the unit's own environment.
    Cmd {
        command: String,
        cwd: std::path::PathBuf,
        /// The unit's assembled environment; the daemon's own is never inherited.
        env: BTreeMap<String, String>,
        /// Git for Windows' `sh.exe`, set with [`Readiness::with_shell`];
        /// without one the check fails with `shell_missing` at once.
        #[cfg(windows)]
        shell: Option<std::path::PathBuf>,
    },
}

#[cfg(windows)]
impl Readiness {
    /// Runs a `cmd` check through `shell`; other checks stay as they are.
    pub fn with_shell(self, shell: Option<std::path::PathBuf>) -> Self {
        match self {
            Self::Cmd {
                command, cwd, env, ..
            } => Self::Cmd {
                command,
                cwd,
                env,
                shell,
            },
            other => other,
        }
    }
}

impl std::fmt::Debug for Readiness {
    /// Omits the `cmd` environment, which can hold secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started => f.write_str("Started"),
            Self::Tcp { port } => f.debug_struct("Tcp").field("port", port).finish(),
            Self::Http {
                port,
                path,
                status,
                host,
            } => f
                .debug_struct("Http")
                .field("port", port)
                .field("path", path)
                .field("status", status)
                .field("host", host)
                .finish(),
            Self::Log { pattern } => f.debug_struct("Log").field("pattern", pattern).finish(),
            Self::Cmd { command, cwd, .. } => f
                .debug_struct("Cmd")
                .field("command", command)
                .field("cwd", cwd)
                .finish_non_exhaustive(),
        }
    }
}

/// Polling interval for probes; short enough to feel instant, long enough to stay cheap.
const POLL: Duration = Duration::from_millis(150);
/// Upper bound for one `cmd` attempt, so a hanging probe cannot eat the whole timeout.
const CMD_ATTEMPT: Duration = Duration::from_secs(10);

/// Waits for readiness, process exit or the timeout, then records the result on the host.
/// `listening` reports the ports the unit's tree bound, for the tcp timeout diagnostic.
pub async fn run_check<F, Fut>(host: &UnitHost, check: Readiness, limit: Duration, listening: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Vec<u16>>,
{
    if matches!(check, Readiness::Started) {
        host.set_state(UnitState::Ready);
        return;
    }
    // A pattern that cannot compile never matches; waiting for the timeout would
    // only disguise a config mistake as a slow service.
    if let Readiness::Log { pattern } = &check
        && let Err(error) = regex::Regex::new(pattern)
    {
        host.set_state(UnitState::Failed(Some(RpcError::new(
            "config_error",
            json!({"path": "ready.log", "pattern": pattern, "message": error.to_string()}),
            1,
        ))));
        return;
    }
    // Without the shell no attempt can pass; failing now names the cause.
    #[cfg(windows)]
    if let Readiness::Cmd { shell: None, .. } = &check {
        host.set_state(UnitState::Failed(Some(windows::shell_missing(
            json!({"check": "cmd"}),
        ))));
        return;
    }
    let deadline = Instant::now() + limit;
    tracing::debug!(
        unit = %host.info().name,
        check = describe(&check),
        port = match &check {
            Readiness::Tcp { port } | Readiness::Http { port, .. } => Some(*port),
            _ => None,
        },
        timeout_ms = limit.as_millis() as u64,
        "readiness check"
    );
    let last = LastStatus::default();
    let probe = probe_until_ready(host, &check, deadline, &last);
    let exited = host.wait_exit();
    tokio::select! {
        result = probe => match result {
            Ok(()) => host.set_state(UnitState::Ready),
            Err(()) => {
                let mut data = json!({"check": describe(&check), "timeout_ms": limit.as_millis()});
                if let Readiness::Tcp { port } | Readiness::Http { port, .. } = &check {
                    data["expected_port"] = json!(port);
                    data["listening"] = json!(listening().await);
                }
                if let Readiness::Http { port, path, status, host } = &check {
                    last.describe(&mut data, path, &host_header(*port, host.as_deref()), *status);
                }
                host.set_state(UnitState::Failed(Some(RpcError::new("ready_timeout", data, 1))));
            }
        },
        // The waiter task classifies the exit itself.
        _ = exited => {}
    }
}

/// Short machine name for error data.
fn describe(check: &Readiness) -> &'static str {
    match check {
        Readiness::Started => "started",
        Readiness::Tcp { .. } => "tcp",
        Readiness::Http { .. } => "http",
        Readiness::Log { .. } => "log",
        Readiness::Cmd { .. } => "cmd",
    }
}

/// Returns `Err` only on timeout or an invalid check definition. An http probe
/// records each status it receives in `last`.
async fn probe_until_ready(
    host: &UnitHost,
    check: &Readiness,
    deadline: Instant,
    last: &LastStatus,
) -> Result<(), ()> {
    match check {
        Readiness::Started => Ok(()),
        Readiness::Tcp { port } => poll(deadline, || async move { tcp_open(*port).await }).await,
        Readiness::Http {
            port,
            path,
            status,
            host,
        } => {
            let host = &host_header(*port, host.as_deref());
            poll(deadline, || async move {
                http_ok(*port, path, host, *status, last).await
            })
            .await
        }
        #[cfg(unix)]
        Readiness::Cmd { command, cwd, env } => {
            poll(deadline, || async move {
                cmd_succeeds(command, cwd, env, deadline).await
            })
            .await
        }
        #[cfg(windows)]
        Readiness::Cmd {
            command,
            cwd,
            env,
            shell,
        } => {
            // `run_check` fails a check without a shell before probing.
            let Some(shell) = shell else { return Err(()) };
            poll(deadline, || async move {
                windows::cmd_succeeds(shell, command, cwd, env, deadline).await
            })
            .await
        }
        Readiness::Log { pattern } => {
            let Ok(regex) = regex::Regex::new(pattern) else {
                return Err(());
            };
            // Subscribe first, then scan history, so a line printed in between is not lost.
            let mut output = host.subscribe_output();
            if host.tail(200).await.iter().any(|line| regex.is_match(line)) {
                return Ok(());
            }
            let mut stripper = Stripper::new();
            let mut pending = String::new();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match timeout(remaining, output.recv()).await {
                    Err(_) => return Err(()),
                    Ok(Ok(bytes)) => {
                        pending.push_str(&stripper.feed(&bytes));
                        while let Some(end) = pending.find(['\n', '\r']) {
                            if regex.is_match(&pending[..end]) {
                                return Ok(());
                            }
                            pending.drain(..=end);
                        }
                        // A partial line can also match (prompts without newline).
                        if regex.is_match(&pending) {
                            return Ok(());
                        }
                        if pending.len() > 64 * 1024 {
                            pending.clear();
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return Err(()),
                }
            }
        }
    }
}

/// Runs one `cmd` attempt in the unit's environment. Each attempt is bounded by
/// [`CMD_ATTEMPT`] and the overall deadline; a timed-out probe is killed, not left
/// running alongside the next attempt.
#[cfg(unix)]
async fn cmd_succeeds(
    command: &str,
    cwd: &std::path::Path,
    env: &BTreeMap<String, String>,
    deadline: Instant,
) -> bool {
    let limit = deadline
        .saturating_duration_since(Instant::now())
        .min(CMD_ATTEMPT);
    let status = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(timeout(limit, status).await, Ok(Ok(status)) if status.success())
}

/// ANSI stripping that keeps parser state across chunks, so an escape sequence or a
/// UTF-8 character split between two reads is neither garbled nor leaked as text.
struct Stripper {
    writer: strip_ansi_escapes::Writer<Collected>,
    collected: Arc<Mutex<Vec<u8>>>,
    /// An incomplete UTF-8 sequence at the end of the last chunk. vte 0.14 keeps
    /// escape state across writes but drops a byte after a split character.
    carry: Vec<u8>,
}

/// The stripper's sink: bytes accumulate until [`Stripper::feed`] takes them.
struct Collected(Arc<Mutex<Vec<u8>>>);

impl Write for Collected {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Stripper {
    /// Starts with an empty parser state.
    fn new() -> Self {
        let collected = Arc::new(Mutex::new(Vec::new()));
        Self {
            writer: strip_ansi_escapes::Writer::new(Collected(collected.clone())),
            collected,
            carry: Vec::new(),
        }
    }

    /// Feeds one chunk and returns the text completed so far.
    fn feed(&mut self, bytes: &[u8]) -> String {
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(bytes);
        let keep = incomplete_utf8_tail(&input);
        self.carry = input.split_off(input.len() - keep);
        // Writing into memory cannot fail; the flush pushes out partial lines.
        let _ = self.writer.write_all(&input);
        let _ = self.writer.flush();
        let taken = std::mem::take(
            &mut *self
                .collected
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        String::from_utf8_lossy(&taken).into_owned()
    }
}

/// Length of a UTF-8 sequence cut off at the end of `bytes`, or 0 when it ends on a
/// character boundary (or in bytes that are not UTF-8 at all).
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            // A continuation byte; the lead byte is further back.
            continue;
        }
        let needed = match byte {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

/// Waits for `infra.containers.<name>.ready` once the container is up. `ports`
/// maps its port names to published host ports in declaration order; `env` runs
/// a `cmd` probe (on Windows through `shell`, Git's `sh.exe`); `logs` reads the
/// container's last lines for a `log` probe. Fails with `config_error` for an
/// unknown port name or pattern, and with `container_not_ready` when the check
/// does not pass within its timeout.
pub(crate) async fn container_ready<F, Fut>(
    name: &str,
    ready: &devlanes_resolve::ContainerReady,
    ports: &[(String, u16)],
    env: &BTreeMap<String, String>,
    #[cfg(windows)] shell: Option<&std::path::Path>,
    logs: F,
) -> Result<(), RpcError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<String>>,
{
    use devlanes_resolve::ContainerCheck;
    let path = format!("infra.containers.{name}.ready");
    let host_port = |wanted: &Option<String>| {
        let found = match wanted {
            Some(wanted) => ports.iter().find(|(port, _)| port == wanted),
            None => ports.first(),
        };
        found.map(|(_, port)| *port).ok_or_else(|| {
            RpcError::new(
                "config_error",
                json!({
                    "path": path,
                    "message": match wanted {
                        Some(wanted) => format!("the container has no port named `{wanted}`"),
                        None => "a tcp or http check needs a container port".to_owned(),
                    },
                }),
                1,
            )
        })
    };
    let deadline = Instant::now() + ready.timeout;
    tracing::debug!(
        container = name,
        timeout_ms = ready.timeout.as_millis() as u64,
        "waiting until the container is ready"
    );
    let last = LastStatus::default();
    // The path and Host header an http check sent, for the timeout's error data.
    let mut http_request = None;
    let (kind, passed) = match &ready.check {
        ContainerCheck::Tcp { port } => {
            let port = host_port(port)?;
            ("tcp", poll(deadline, || tcp_open(port)).await)
        }
        ContainerCheck::Http {
            port,
            path,
            status,
            host,
        } => {
            let port = host_port(port)?;
            let host = host_header(port, host.as_deref());
            let probe = || http_ok(port, path, &host, *status, &last);
            let passed = poll(deadline, probe).await;
            http_request = Some((path, host));
            ("http", passed)
        }
        ContainerCheck::Log { pattern } => {
            let regex = regex::Regex::new(pattern).map_err(|error| {
                RpcError::new(
                    "config_error",
                    json!({"path": path, "pattern": pattern, "message": error.to_string()}),
                    1,
                )
            })?;
            let regex = &regex;
            let logs = &logs;
            let probe = || async move { logs().await.iter().any(|line| regex.is_match(line)) };
            ("log", poll(deadline, probe).await)
        }
        #[cfg(unix)]
        ContainerCheck::Cmd { shell, cwd } => {
            let probe = || cmd_succeeds(shell, cwd, env, deadline);
            ("cmd", poll(deadline, probe).await)
        }
        #[cfg(windows)]
        ContainerCheck::Cmd {
            shell: command,
            cwd,
        } => {
            let Some(shell) = shell else {
                return Err(windows::shell_missing(
                    json!({"container": name, "check": "cmd"}),
                ));
            };
            let probe = || windows::cmd_succeeds(shell, command, cwd, env, deadline);
            ("cmd", poll(deadline, probe).await)
        }
    };
    passed.map_err(|()| {
        let mut data = json!({
            "container": name,
            "check": kind,
            "timeout_ms": ready.timeout.as_millis(),
        });
        if let (ContainerCheck::Http { status, .. }, Some((path, host))) =
            (&ready.check, &http_request)
        {
            last.describe(&mut data, path, host, *status);
        }
        RpcError::new("container_not_ready", data, 1)
    })
}

/// The last HTTP status an http probe received, so a timeout can say what the
/// service answered instead of only that it never passed. 0 means no answer yet.
#[derive(Default)]
struct LastStatus(AtomicU16);

impl LastStatus {
    /// Remembers `code` as the latest answer.
    fn record(&self, code: u16) {
        self.0.store(code, Ordering::Relaxed);
    }

    /// The latest answer, if any request got a status line.
    fn get(&self) -> Option<u16> {
        Some(self.0.load(Ordering::Relaxed)).filter(|code| *code != 0)
    }

    /// Adds the request the probe sent (`http_path`, `http_host`), `expected_status`
    /// (null when unset) and, once anything answered, `last_status` to timeout data.
    fn describe(
        &self,
        data: &mut serde_json::Value,
        path: &str,
        host: &str,
        expected: Option<u16>,
    ) {
        data["http_path"] = json!(path);
        data["http_host"] = json!(host);
        data["expected_status"] = json!(expected);
        if let Some(code) = self.get() {
            data["last_status"] = json!(code);
        }
    }
}

/// Retries a probe until it succeeds or the deadline passes.
async fn poll<F, Fut>(deadline: Instant, mut probe: F) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = Instant::now();
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        if probe().await {
            tracing::debug!(
                attempts,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "probe passed"
            );
            return Ok(());
        }
        tracing::trace!(attempts, "probe not passing yet");
        if Instant::now() + POLL >= deadline {
            tracing::debug!(
                attempts,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "probe timed out"
            );
            return Err(());
        }
        sleep(POLL).await;
    }
}

/// 127.0.0.1 first, then ::1, because some dev servers bind only one family.
async fn connect(port: u16) -> Option<TcpStream> {
    for address in [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ] {
        if let Ok(Ok(stream)) =
            timeout(Duration::from_millis(500), TcpStream::connect(address)).await
        {
            return Some(stream);
        }
    }
    None
}

/// The port accepts a connection.
pub async fn tcp_open(port: u16) -> bool {
    connect(port).await.is_some()
}

/// The Host header an http probe sends: the configured `host`, else `127.0.0.1:<port>`.
fn host_header(port: u16, host: Option<&str>) -> String {
    host.map_or_else(|| format!("127.0.0.1:{port}"), str::to_owned)
}

/// Minimal HTTP/1.1 probe; avoids a client dependency for one status line. Sends
/// `host` as the Host header. Passes on exactly `expected`, or on any 2xx status when
/// `expected` is unset. Records any status it receives in `last`.
async fn http_ok(
    port: u16,
    path: &str,
    host: &str,
    expected: Option<u16>,
    last: &LastStatus,
) -> bool {
    let Some(mut stream) = connect(port).await else {
        return false;
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: devlanes-ready\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0_u8; 64];
    let Ok(Ok(read)) = timeout(Duration::from_secs(2), stream.read(&mut head)).await else {
        return false;
    };
    let status = std::str::from_utf8(&head[..read])
        .ok()
        .and_then(|text| text.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    let Some(code) = status else {
        return false;
    };
    last.record(code);
    match expected {
        Some(want) => code == want,
        // Redirects are not followed, so a 3xx (often a login page) or a 404 from a
        // half-started router must not count as healthy.
        None => (200..300).contains(&code),
    }
}

/// `cmd` probes on Windows: Git's `sh.exe` in a job that ends the probe's
/// whole tree when the attempt is over or abandoned, and when the daemon dies.
#[cfg(windows)]
mod windows {
    use std::{collections::BTreeMap, io, path::Path, sync::Arc};

    use devlanes_proto::RpcError;
    use devlanes_sys_windows::{
        job::{Job, JobOptions},
        spawn::{self, Child, CreationFlags, Environment, Spawn, Stdio, Stream},
    };
    use serde_json::{Value, json};
    use tokio::time::{Instant, timeout};

    use super::CMD_ATTEMPT;

    /// The failure of a `cmd` check without Git's `sh.exe`: `spawn_failed`
    /// with reason `shell_missing`, plus the check's own `data`.
    pub(super) fn shell_missing(mut data: Value) -> RpcError {
        data["program"] = json!("/bin/sh");
        data["reason"] = json!("shell_missing");
        data["message"] = json!(
            "`ready.cmd` needs Git for Windows' sh.exe, and no `git` on PATH has one; install Git for Windows and restart the daemon"
        );
        RpcError::new("spawn_failed", data, 1)
    }

    /// Runs one `cmd` attempt with `shell`, like the Unix probe: the unit's
    /// environment only (plus the shell's variables and Git's tools at the
    /// end of `PATH`), no input or output, bounded by [`CMD_ATTEMPT`] and the
    /// deadline. Whatever is left of the attempt's process tree is ended when
    /// it returns or is dropped.
    pub(super) async fn cmd_succeeds(
        shell: &Path,
        command: &str,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        deadline: Instant,
    ) -> bool {
        let limit = deadline
            .saturating_duration_since(Instant::now())
            .min(CMD_ATTEMPT);
        let mut env = env.clone();
        devlanes_shell_windows::shell_env(&mut env, shell);
        // The per-drive `=C:` entries cannot go into an environment block.
        env.retain(|name, _| !name.starts_with('='));
        let line = devlanes_shell_windows::shell_command_line(shell, command);
        let (shell, cwd) = (shell.to_path_buf(), cwd.to_path_buf());
        // Process creation touches the disk, so it stays off the executor.
        let started = tokio::task::spawn_blocking(move || start(&shell, &line, &cwd, &env)).await;
        let Ok(Ok((job, child))) = started else {
            return false;
        };
        let child = Arc::new(child);
        let waiter = tokio::task::spawn_blocking({
            let child = child.clone();
            move || child.wait()
        });
        let passed = matches!(timeout(limit, waiter).await, Ok(Ok(Ok(0))));
        // Closing the job's only handle ends every process still in it. A
        // dropped attempt drops `job` too, which also ends the blocking wait.
        drop(job);
        passed
    }

    /// Starts the shell in a new kill-on-close job, its standard streams on
    /// `NUL`. Blocking.
    fn start(
        shell: &Path,
        line: &std::ffi::OsStr,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> io::Result<(Job, Child)> {
        let environment = Environment::new(env)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let job = Job::create(JobOptions {
            kill_on_close: true,
            die_on_unhandled_exception: true,
        })?;
        // Default flags: the probe shares the daemon's hidden console, so no
        // window opens and no console host is created per attempt.
        let child = spawn::spawn(&Spawn {
            program: shell,
            command_line: line,
            environment: Some(&environment),
            cwd: Some(cwd),
            stdio: Stdio::Handles {
                stdin: Stream::Null,
                stdout: Stream::Null,
                stderr: Stream::Null,
            },
            inherit: &[],
            job: Some(&job),
            flags: CreationFlags::default(),
        })?;
        Ok((job, child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tcp_and_http_probes_see_a_local_listener() -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0_u8; 512];
                let _ = stream.read(&mut buffer).await;
                let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await;
            }
        });
        let last = LastStatus::default();
        assert!(tcp_open(port).await);
        assert!(http_ok(port, "/", "127.0.0.1", None, &last).await);
        assert!(http_ok(port, "/", "127.0.0.1", Some(204), &last).await);
        assert!(!http_ok(port, "/", "127.0.0.1", Some(200), &last).await);
        Ok(())
    }

    /// Serves HTTP on a local port, answering each request with the status its
    /// path names, such as `/404`. `/health` answers 200 only with
    /// `Host: api.example.invalid` and 404 otherwise, like an app that routes by host.
    async fn status_server() -> std::io::Result<u16> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0_u8; 512];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request.split_whitespace().nth(1).unwrap_or("/500");
                let code = if path == "/health" {
                    let own = request.contains("\r\nHost: api.example.invalid\r\n");
                    if own { "200" } else { "404" }.to_owned()
                } else {
                    path.strip_prefix('/').unwrap_or("500").to_owned()
                };
                let response = format!("HTTP/1.1 {code} Status\r\nContent-Length: 0\r\n\r\n");
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Ok(port)
    }

    /// Without `status` only 2xx passes; with it, only that exact code does.
    #[tokio::test]
    async fn http_probe_needs_2xx_unless_a_status_is_set() -> Result<(), Box<dyn std::error::Error>>
    {
        let port = status_server().await?;
        let last = LastStatus::default();
        assert!(http_ok(port, "/200", "127.0.0.1", None, &last).await);
        assert!(http_ok(port, "/299", "127.0.0.1", None, &last).await);
        assert!(!http_ok(port, "/302", "127.0.0.1", None, &last).await);
        assert!(!http_ok(port, "/404", "127.0.0.1", None, &last).await);
        assert!(!http_ok(port, "/500", "127.0.0.1", None, &last).await);
        assert!(http_ok(port, "/404", "127.0.0.1", Some(404), &last).await);
        assert!(http_ok(port, "/302", "127.0.0.1", Some(302), &last).await);
        assert!(!http_ok(port, "/200", "127.0.0.1", Some(204), &last).await);
        Ok(())
    }

    /// The probe sends the configured Host header, and `127.0.0.1:<port>` without one.
    #[tokio::test]
    async fn http_probe_sends_the_host_header() -> Result<(), Box<dyn std::error::Error>> {
        let port = status_server().await?;
        let last = LastStatus::default();
        let default = host_header(port, None);
        assert_eq!(default, format!("127.0.0.1:{port}"));
        assert!(!http_ok(port, "/health", &default, None, &last).await);
        assert_eq!(last.get(), Some(404));
        let own = host_header(port, Some("api.example.invalid"));
        assert!(http_ok(port, "/health", &own, None, &last).await);
        Ok(())
    }

    /// A probe remembers the last status it received, and a closed port leaves it
    /// untouched.
    #[tokio::test]
    async fn http_probe_records_the_last_status() -> Result<(), Box<dyn std::error::Error>> {
        let port = status_server().await?;
        let last = LastStatus::default();
        assert_eq!(last.get(), None);
        assert!(!http_ok(port, "/302", "127.0.0.1", None, &last).await);
        assert_eq!(last.get(), Some(302));
        let closed = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let closed_port = closed.local_addr()?.port();
        drop(closed);
        assert!(!http_ok(closed_port, "/", "127.0.0.1", None, &last).await);
        assert_eq!(last.get(), Some(302));
        Ok(())
    }

    /// A container http check that times out says what it requested, what the
    /// service last answered and whether a status was configured.
    #[tokio::test]
    async fn container_http_timeout_reports_the_last_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let port = status_server().await?;
        let ready = devlanes_resolve::ContainerReady {
            check: devlanes_resolve::ContainerCheck::Http {
                port: None,
                path: "/302".into(),
                status: None,
                host: None,
            },
            timeout: Duration::from_millis(400),
        };
        let ports = [("http".to_owned(), port)];
        let env = BTreeMap::new();
        #[cfg(unix)]
        let check = container_ready("web", &ready, &ports, &env, || async { Vec::new() });
        #[cfg(windows)]
        let check = container_ready("web", &ready, &ports, &env, None, || async { Vec::new() });
        let Err(error) = timeout(Duration::from_secs(10), check).await? else {
            return Err("a 302 must not pass a check without `status`".into());
        };
        assert_eq!(error.code, "container_not_ready");
        assert_eq!(error.data["check"], "http");
        assert_eq!(error.data["last_status"], 302);
        assert!(error.data["expected_status"].is_null());
        assert_eq!(error.data["http_path"], "/302");
        assert_eq!(error.data["http_host"], format!("127.0.0.1:{port}"));
        Ok(())
    }

    /// An escape sequence and a multi-byte character split across chunks are stripped
    /// as if they had arrived whole.
    #[test]
    fn stripping_keeps_state_across_chunks() {
        let mut stripper = Stripper::new();
        let mut text = String::new();
        for chunk in [
            &b"\x1b[3"[..],
            b"2mread",
            b"y \xc3",
            b"\xa9t\xc3\xa9\x1b[0",
            b"m\n",
        ] {
            text.push_str(&stripper.feed(chunk));
        }
        assert_eq!(text, "ready \u{e9}t\u{e9}\n");
    }

    /// `cmd` probes see the unit's environment only, and a hanging attempt is cut
    /// at the deadline instead of blocking the check.
    #[cfg(unix)]
    #[tokio::test]
    async fn cmd_probe_uses_unit_env_and_is_bounded() {
        let cwd = std::env::temp_dir();
        let env = BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("DL_READY".to_owned(), "yes".to_owned()),
        ]);
        let soon = Instant::now() + Duration::from_secs(5);
        assert!(cmd_succeeds("test \"$DL_READY\" = yes", &cwd, &env, soon).await);
        // The daemon's own HOME is not inherited.
        assert!(!cmd_succeeds("test -n \"$HOME\"", &cwd, &env, soon).await);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(300);
        assert!(!cmd_succeeds("sleep 30", &cwd, &env, deadline).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The environment Windows programs need, `PATH` holding only System32,
    /// plus `extra`.
    #[cfg(windows)]
    fn windows_env(extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        let system = devlanes_sys_windows::file::system_directory().expect("System32");
        let mut env: BTreeMap<String, String> = ["SYSTEMROOT", "WINDIR", "TEMP", "TMP"]
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
            .collect();
        env.insert("PATH".into(), system.to_string_lossy().into_owned());
        env.extend(
            extra
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        env
    }

    /// `cmd` probes run through Git's `sh.exe` with the unit's environment
    /// only, find Git's tools through the directory appended to `PATH`, and
    /// pass only on exit 0.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_probe_runs_through_git_sh() {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let cwd = std::env::temp_dir();
        let env = windows_env(&[("DL_READY", "yes")]);
        let probe = |command: &'static str| {
            let (shell, cwd, env) = (shell.clone(), cwd.clone(), env.clone());
            async move {
                let soon = Instant::now() + Duration::from_secs(30);
                windows::cmd_succeeds(&shell, command, &cwd, &env, soon).await
            }
        };
        assert!(probe("test \"$DL_READY\" = yes").await);
        // The daemon's own variables are not inherited.
        assert!(probe("test -z \"$USERPROFILE\"").await);
        assert!(probe("sleep 0 && exit 0").await);
        assert!(!probe("sleep 0 && exit 3").await);
    }

    /// An attempt cut at the deadline ends its whole process tree, including
    /// a program the shell started in the background.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_probe_timeout_ends_the_tree() {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("pid");
        let env = windows_env(&[("MARK", &marker.to_string_lossy().replace('\\', "/"))]);
        // `/proc/<pid>/winpid` is the Windows PID behind an MSYS one.
        let script =
            "sleep 60 & cat /proc/$!/winpid > \"$MARK.tmp\" && mv \"$MARK.tmp\" \"$MARK\"; wait";
        let watcher = {
            let marker = marker.clone();
            tokio::spawn(async move {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&marker)
                        && let Ok(pid) = text.trim().parse::<u32>()
                        && let Ok(Some(identity)) = devlanes_sys::identity(pid)
                    {
                        return identity;
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            })
        };
        // `sh.exe` on a loaded runner has taken more than 5 s to start the
        // background sleep and write the marker; the attempt must outlast that,
        // or nothing is left to check.
        let deadline = Instant::now() + Duration::from_secs(15);
        let passed = timeout(
            Duration::from_secs(40),
            windows::cmd_succeeds(&shell, script, dir.path(), &env, deadline),
        )
        .await
        .expect("the attempt ends at its deadline");
        assert!(!passed);
        let background = timeout(Duration::from_secs(5), watcher)
            .await
            .expect("the background sleep started")
            .expect("watcher");
        // Ending a job's processes is asynchronous; allow it a moment.
        let mut alive = true;
        for _ in 0..100 {
            alive = devlanes_sys::matches(&background).unwrap_or(false);
            if !alive {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        if alive {
            let _ = devlanes_sys::terminate_verified(&background, 1);
        }
        assert!(!alive, "the background sleep outlived its probe");
    }

    /// A `cmd` check without Git's `sh.exe` fails at once with
    /// `shell_missing`, for units and containers alike.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_probe_without_shell_fails_with_shell_missing() {
        let ready = devlanes_resolve::ContainerReady {
            check: devlanes_resolve::ContainerCheck::Cmd {
                shell: "exit 0".into(),
                cwd: std::env::temp_dir(),
            },
            timeout: Duration::from_secs(60),
        };
        let env = BTreeMap::new();
        let check = container_ready("db", &ready, &[], &env, None, || async { Vec::new() });
        let error = timeout(Duration::from_secs(5), check)
            .await
            .expect("fails at once")
            .expect_err("no shell");
        assert_eq!(error.code, "spawn_failed");
        assert_eq!(error.data["reason"], "shell_missing");
        assert_eq!(error.data["container"], "db");
        let check = Readiness::Cmd {
            command: "exit 0".into(),
            cwd: std::env::temp_dir(),
            env: BTreeMap::new(),
            shell: Some("sh.exe".into()),
        }
        .with_shell(None);
        assert!(matches!(check, Readiness::Cmd { shell: None, .. }));
    }

    #[tokio::test]
    async fn closed_port_is_not_ready() -> Result<(), Box<dyn std::error::Error>> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        assert!(!tcp_open(port).await);
        Ok(())
    }
}
