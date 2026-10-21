//! Readiness checks (plan 4.6). A check never restarts or stops anything; it only
//! moves a unit to `ready` or `failed`.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
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
#[derive(Debug, Clone)]
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
    /// Shell command retried until it exits 0.
    Cmd {
        command: String,
        cwd: std::path::PathBuf,
    },
}

/// Polling interval for probes; short enough to feel instant, long enough to stay cheap.
const POLL: Duration = Duration::from_millis(150);

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
        Readiness::Cmd { command, cwd } => {
            poll(deadline, || async move {
                tokio::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(cwd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|status| status.success())
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
            let mut pending = String::new();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match timeout(remaining, output.recv()).await {
                    Err(_) => return Err(()),
                    Ok(Ok(bytes)) => {
                        let text = String::from_utf8_lossy(&strip_ansi_escapes::strip(&bytes))
                            .into_owned();
                        pending.push_str(&text);
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

    #[tokio::test]
    async fn closed_port_is_not_ready() -> Result<(), Box<dyn std::error::Error>> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        assert!(!tcp_open(port).await);
        Ok(())
    }
}
