//! Focused fixture contracts, independent of the devlanes daemon.

use futures_util::{SinkExt, StreamExt};
use std::{net::SocketAddr, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    net::TcpStream,
    process::{Child, ChildStdout, Command},
    time::timeout,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Owns and kills the direct fixture process if a test fails.
struct Server {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    addr: SocketAddr,
    startup: Vec<String>,
}

impl Server {
    /// Waits for an explicit listening announcement instead of racing a free-port probe.
    async fn start(args: &[&str]) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fixture-server"))
            .args(["--exit-after", "10s"])
            .args(args)
            .env("PORT", "0")
            .env("FIXTURE_MARKER", "test-value")
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or("stdout missing")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut startup = Vec::new();
        let addr = timeout(Duration::from_secs(5), async {
            while let Some(line) = lines.next_line().await? {
                if let Some(addr) = line.strip_prefix("LISTENING ") {
                    return Ok::<SocketAddr, Box<dyn std::error::Error>>(addr.parse()?);
                }
                startup.push(line);
            }
            Err("fixture exited before listening".into())
        })
        .await??;
        Ok(Self {
            child,
            lines,
            addr,
            startup,
        })
    }

    /// Sends a raw HTTP request with a caller-chosen Host header.
    async fn request(&self, path: &str) -> Result<TcpStream> {
        let mut socket = TcpStream::connect(self.addr).await?;
        socket
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: fixture.example\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
        Ok(socket)
    }
}

/// HTTP diagnostics retain Host/path and streaming events arrive before shutdown.
#[tokio::test]
async fn http_and_sse() -> Result {
    timeout(Duration::from_secs(5), async {
        let server = Server::start(&[]).await?;
        let mut response = String::new();
        server
            .request("/healthz?probe=1")
            .await?
            .read_to_string(&mut response)
            .await?;
        assert!(response.contains("200 OK"));
        assert!(response.contains("fixture.example"));
        assert!(response.contains("/healthz?probe=1"));
        let mut socket = server.request("/sse").await?;
        let mut body = Vec::new();
        let mut chunk = [0; 1024];
        while !String::from_utf8_lossy(&body).contains("data: 1") {
            let size = socket.read(&mut chunk).await?;
            assert!(size > 0, "SSE ended before the second event");
            body.extend_from_slice(&chunk[..size]);
        }
        assert!(String::from_utf8_lossy(&body).contains("text/event-stream"));
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}

/// WebSocket text and binary frames survive an actual upgrade connection.
#[tokio::test]
async fn websocket_echo() -> Result {
    timeout(Duration::from_secs(5), async {
        let server = Server::start(&[]).await?;
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{}/ws", server.addr)).await?;
        for frame in [
            tokio_tungstenite::tungstenite::Message::Text("hello".into()),
            tokio_tungstenite::tungstenite::Message::Binary(vec![0, 255, 10].into()),
        ] {
            socket.send(frame.clone()).await?;
            assert_eq!(socket.next().await.ok_or("WebSocket closed")??, frame);
        }
        socket.close(None).await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}

/// Alternate binding ignores PORT and IPv6-only sockets reject IPv4 clients.
#[tokio::test]
async fn alternate_ipv6_binding() -> Result {
    let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let port = occupied.local_addr()?.port().to_string();
    let server = Server::start(&[
        "--port",
        &port,
        "--bind-other-port",
        "0",
        "--bind-ipv6-only",
    ])
    .await?;
    assert!(server.addr.is_ipv6());
    assert!(
        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, server.addr.port()))
            .await
            .is_err()
    );
    timeout(Duration::from_secs(2), server.request("/")).await??;
    Ok(())
}

/// SIGTERM resistance leaves the listener alive until explicitly killed.
#[tokio::test]
async fn ignores_sigterm() -> Result {
    let mut server = Server::start(&["--ignore-sigterm"]).await?;
    let pid = rustix::process::Pid::from_raw(server.child.id().ok_or("missing pid")? as i32)
        .ok_or("invalid pid")?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.child.try_wait()?.is_none());
    timeout(Duration::from_secs(2), server.request("/")).await??;
    server.child.kill().await?;
    Ok(())
}

/// Explicit environment output, log lines and exit deadlines work without a daemon.
#[tokio::test]
async fn output_and_deadline() -> Result {
    let output = timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_fixture-server"))
            .args([
                "--port",
                "0",
                "--log-line",
                "ready marker",
                "--print-env",
                "--exit-after",
                "20ms",
            ])
            .env_clear()
            .env("FIXTURE_MARKER", "value")
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("ready marker\n"));
    assert!(stdout.contains("\"FIXTURE_MARKER\":\"value\""));
    Ok(())
}

/// A failed bind must not create descendants which outlive the failed server.
#[tokio::test]
async fn bind_failure_does_not_spawn() -> Result {
    let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let port = occupied.local_addr()?.port().to_string();
    let output = timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_fixture-server"))
            .args(["--port", &port, "--spawn-grandchild", "--exit-after", "1s"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "failed listener spawned descendants"
    );
    Ok(())
}

/// A real grandchild can escape into its own session and remains deadline-bounded.
#[tokio::test]
async fn escaped_grandchild() -> Result {
    let mut server = Server::start(&["--setsid-grandchild"]).await?;
    let line = timeout(Duration::from_secs(5), async {
        if let Some(line) = server
            .startup
            .iter()
            .find(|line| line.starts_with("grandchild "))
        {
            return Ok(line.clone());
        }
        while let Some(line) = server.lines.next_line().await? {
            if line.starts_with("grandchild ") {
                return Ok::<_, Box<dyn std::error::Error>>(line);
            }
        }
        Err("grandchild announcement missing".into())
    })
    .await??;
    let values: Vec<_> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.split_once('=').map(|(_, value)| value))
        .collect();
    assert_eq!(values.len(), 2);
    assert_eq!(
        values[0], values[1],
        "escaped grandchild must lead its session"
    );
    // Waiting for inherited deadlines avoids leaving descendants after a successful test.
    timeout(Duration::from_secs(12), async {
        while server.lines.next_line().await?.is_some() {}
        server.child.wait().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}
