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
    /// Starts a fixture that exits after 10 s, see [`Server::start_with_deadline`].
    async fn start(args: &[&str]) -> Result<Self> {
        Self::start_with_deadline("10s", args).await
    }

    /// Waits for an explicit listening announcement instead of racing a free-port probe.
    /// `exit_after` bounds the fixture and every descendant it spawns.
    async fn start_with_deadline(exit_after: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fixture-server"))
            .args(["--exit-after", exit_after])
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
#[cfg(unix)]
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_fixture-server"));
    command
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
        .kill_on_drop(true);
    // Winsock fails to start without SystemRoot, even in an otherwise empty environment.
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    let output = timeout(Duration::from_secs(5), command.output()).await??;
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
#[cfg(unix)]
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

/// Descendants announce their PID and exit with the inherited deadline.
#[tokio::test]
async fn spawned_grandchild_announces_itself() -> Result {
    let mut server = Server::start_with_deadline("2s", &["--spawn-grandchild"]).await?;
    let mut announced = server
        .startup
        .iter()
        .any(|line| line.starts_with("grandchild pid="));
    // Every holder of stdout exits at the 2 s deadline, which ends the stream.
    // The timeout leaves room for slow process startup on loaded CI machines.
    timeout(Duration::from_secs(8), async {
        while let Some(line) = server.lines.next_line().await? {
            announced |= line.starts_with("grandchild pid=");
        }
        server.child.wait().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    assert!(announced, "grandchild announcement missing");
    Ok(())
}

/// A grandchild started with `DETACHED_PROCESS` still announces itself through the
/// inherited pipe and exits with the inherited deadline.
#[cfg(windows)]
#[tokio::test]
async fn detached_grandchild_announces_itself() -> Result {
    let mut server = Server::start_with_deadline("2s", &["--detached-grandchild"]).await?;
    let mut announced = server
        .startup
        .iter()
        .any(|line| line.starts_with("grandchild pid="));
    timeout(Duration::from_secs(8), async {
        while let Some(line) = server.lines.next_line().await? {
            announced |= line.starts_with("grandchild pid=");
        }
        server.child.wait().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    assert!(announced, "grandchild announcement missing");
    Ok(())
}

/// The console width is a number with a (hidden) console and `none` without one,
/// even though stdout is a pipe in both cases.
#[cfg(windows)]
#[tokio::test]
async fn prints_console_width() -> Result {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    for (flags, expect_width) in [(DETACHED_PROCESS, false), (CREATE_NO_WINDOW, true)] {
        let output = timeout(
            Duration::from_secs(5),
            Command::new(env!("CARGO_BIN_EXE_fixture-server"))
                .args([
                    "--port",
                    "0",
                    "--print-console-width",
                    "--exit-after",
                    "10ms",
                ])
                .creation_flags(flags)
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        let width = stdout
            .lines()
            .find_map(|line| line.strip_prefix("CONSOLE_WIDTH "))
            .ok_or("no CONSOLE_WIDTH line")?;
        if expect_width {
            assert!(width.parse::<u16>().is_ok_and(|w| w > 0), "{stdout}");
        } else {
            assert_eq!(width, "none", "{stdout}");
        }
    }
    Ok(())
}

/// Windows-only flags fail before anything is spawned on Unix.
#[cfg(unix)]
#[tokio::test]
async fn windows_flags_are_refused() -> Result {
    for flag in [
        "--detached-grandchild",
        "--breakaway-grandchild",
        "--print-console-width",
    ] {
        let output = timeout(
            Duration::from_secs(5),
            Command::new(env!("CARGO_BIN_EXE_fixture-server"))
                .args(["--port", "0", flag, "--exit-after", "1s"])
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        assert!(!output.status.success(), "{flag}");
        assert!(output.stdout.is_empty(), "{flag}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(flag));
    }
    Ok(())
}

/// An installed fake checks its arguments, records them, prints expanded output
/// and exits with the configured code; a wrong call exits 2 without recording.
#[tokio::test]
async fn fake_tool_follows_its_behaviour() -> Result {
    use devlanes_testing::fake_tool::{Behaviour, install};
    let bin = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    let calls = bin.path().join("calls");
    let fake = install(
        bin.path(),
        "acme",
        &Behaviour::new()
            .args(["env", "--json"])
            .record(&calls)
            .stderr("loading\n")
            .stdout("text {env:MARK} ")
            .stdout_json(serde_json::json!({"DIR": "{cwd}", "GONE": null}))
            .exit(3),
    )?;
    let output = run_fake(&fake, cwd.path(), &["env", "--json"]).await?;
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(String::from_utf8(output.stderr)?, "loading\n");
    let stdout = String::from_utf8(output.stdout)?;
    let json = stdout.strip_prefix("text seen ").ok_or("text prefix")?;
    let value: serde_json::Value = serde_json::from_str(json)?;
    let dir = cwd.path().canonicalize()?.display().to_string();
    assert_eq!(value, serde_json::json!({"DIR": dir, "GONE": null}));

    let output = run_fake(&fake, cwd.path(), &["other"]).await?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)?.contains("bad args: other"));
    assert_eq!(std::fs::read_to_string(&calls)?, "[\"env\",\"--json\"]\n");
    Ok(())
}

/// Runs an installed fake in `cwd` with only `MARK` in its environment.
async fn run_fake(
    fake: &std::path::Path,
    cwd: &std::path::Path,
    args: &[&str],
) -> Result<std::process::Output> {
    let mut command = Command::new(fake);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("MARK", "seen")
        .kill_on_drop(true);
    Ok(timeout(Duration::from_secs(5), command.output()).await??)
}

/// Without Unix sessions, `--setsid-grandchild` fails before anything is spawned.
#[cfg(windows)]
#[tokio::test]
async fn setsid_grandchild_is_refused() -> Result {
    let output = timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_fixture-server"))
            .args(["--port", "0", "--setsid-grandchild", "--exit-after", "1s"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--setsid-grandchild"));
    Ok(())
}
