//! Readiness checks (plan 4.6). A check never restarts or stops anything; it only
//! moves a unit to `ready` or `failed`.

use std::{
    collections::BTreeMap,
    io::Write,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex, PoisonError},
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
    },
}

impl std::fmt::Debug for Readiness {
    /// Omits the `cmd` environment, which can hold secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started => f.write_str("Started"),
            Self::Tcp { port } => f.debug_struct("Tcp").field("port", port).finish(),
            Self::Http { port, path, status } => f
                .debug_struct("Http")
                .field("port", port)
                .field("path", path)
                .field("status", status)
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
    let deadline = Instant::now() + limit;
    let probe = probe_until_ready(host, &check, deadline);
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

/// Returns `Err` only on timeout or an invalid check definition.
async fn probe_until_ready(
    host: &UnitHost,
    check: &Readiness,
    deadline: Instant,
) -> Result<(), ()> {
    match check {
        Readiness::Started => Ok(()),
        Readiness::Tcp { port } => poll(deadline, || async move { tcp_open(*port).await }).await,
        Readiness::Http { port, path, status } => {
            poll(
                deadline,
                || async move { http_ok(*port, path, *status).await },
            )
            .await
        }
        Readiness::Cmd { command, cwd, env } => {
            poll(deadline, || async move {
                cmd_succeeds(command, cwd, env, deadline).await
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

/// Retries a probe until it succeeds or the deadline passes.
async fn poll<F, Fut>(deadline: Instant, mut probe: F) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    loop {
        if probe().await {
            return Ok(());
        }
        if Instant::now() + POLL >= deadline {
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

/// Minimal HTTP/1.1 probe; avoids a client dependency for one status line.
async fn http_ok(port: u16, path: &str, expected: Option<u16>) -> bool {
    let Some(mut stream) = connect(port).await else {
        return false;
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nUser-Agent: devlanes-ready\r\n\r\n"
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
    match (status, expected) {
        (Some(code), Some(want)) => code == want,
        // Without an explicit status, anything below 500 means the server is up.
        (Some(code), None) => code < 500,
        (None, _) => false,
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
        assert!(tcp_open(port).await);
        assert!(http_ok(port, "/", None).await);
        assert!(http_ok(port, "/", Some(204)).await);
        assert!(!http_ok(port, "/", Some(200)).await);
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

    #[tokio::test]
    async fn closed_port_is_not_ready() -> Result<(), Box<dyn std::error::Error>> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        assert!(!tcp_open(port).await);
        Ok(())
    }
}
