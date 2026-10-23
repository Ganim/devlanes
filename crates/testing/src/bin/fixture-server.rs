//! Native HTTP/process fixture. Deliberate misbehavior is selected by CLI flags.

use std::{collections::BTreeMap, convert::Infallible, io::Write, net::SocketAddr, time::Duration};

use axum::{
    Json, Router,
    extract::{WebSocketUpgrade, ws::Message},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Sse, sse::Event},
    routing::get,
};
use clap::Parser;
use futures_util::stream;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::{net::TcpListener, process::Command};

/// Options for deterministic server and process-lifecycle tests.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "PORT", default_value_t = 3000)]
    port: u16,
    #[arg(long)]
    ignore_sigterm: bool,
    #[arg(long)]
    spawn_grandchild: bool,
    #[arg(long)]
    setsid_grandchild: bool,
    #[arg(long)]
    log_line: Vec<String>,
    #[arg(long, value_parser = humantime::parse_duration)]
    exit_after: Option<Duration>,
    #[arg(long)]
    bind_other_port: Option<u16>,
    #[arg(long)]
    bind_ipv6_only: bool,
    #[arg(long)]
    print_env: bool,
    /// Prints `got <line>` for every line read from stdin.
    #[arg(long)]
    echo_input: bool,
    #[arg(long, hide = true, value_parser = ["child", "grandchild"])]
    descendant: Option<String>,
}

/// Starts the fixture; errors produce a nonzero exit and diagnostics on stderr.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Refused before anything binds or spawns.
    #[cfg(windows)]
    if args.setsid_grandchild {
        return Err("--setsid-grandchild needs Unix sessions".into());
    }
    #[cfg(unix)]
    let mut terminate = signal(SignalKind::terminate())?;
    #[cfg(unix)]
    if args.descendant.as_deref() == Some("grandchild") && args.setsid_grandchild {
        rustix::process::setsid()?;
    }

    for line in &args.log_line {
        println!("{line}");
    }
    if args.print_env {
        let env: BTreeMap<_, _> = std::env::vars_os()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        println!("ENV {}", serde_json::to_string(&env)?);
    }

    // Bind before spawning so a port conflict cannot leave an unowned process tree.
    let listener = if args.descendant.is_none() {
        Some(bind(&args)?)
    } else {
        None
    };
    let mut child = if args.descendant.as_deref() == Some("child") {
        Some(spawn_descendant(&args, "grandchild")?)
    } else if args.descendant.is_none() && (args.spawn_grandchild || args.setsid_grandchild) {
        Some(spawn_descendant(&args, "child")?)
    } else {
        None
    };

    if let Some(role) = &args.descendant {
        #[cfg(unix)]
        println!(
            "{role} pid={} sid={}",
            std::process::id(),
            rustix::process::getsid(None)?.as_raw_nonzero()
        );
        // Windows has no sessions to report.
        #[cfg(windows)]
        println!("{role} pid={}", std::process::id());
    } else if let Some(listener) = &listener {
        println!("LISTENING {}", listener.local_addr()?);
    }
    std::io::stdout().flush()?;
    if args.echo_input {
        // Blocking reads; the thread ends with the process.
        std::thread::spawn(|| {
            for line in std::io::stdin().lines().map_while(Result::ok) {
                println!("got {}", line.trim_end_matches('\r'));
            }
        });
    }

    let server = async {
        if let Some(listener) = listener {
            axum::serve(listener, app()).await
        } else {
            std::future::pending().await
        }
    };
    let deadline = async {
        if let Some(duration) = args.exit_after {
            tokio::time::sleep(duration).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let shutdown = async {
        loop {
            terminate.recv().await;
            if !args.ignore_sigterm {
                break;
            }
        }
    };
    #[cfg(unix)]
    let interrupt = tokio::signal::ctrl_c();
    // Windows stops send Ctrl-Break for SIGTERM and Ctrl-C for SIGINT; with
    // `--ignore-sigterm` both are swallowed, so only termination ends it.
    #[cfg(windows)]
    let (mut ctrl_break, mut ctrl_c) = (
        tokio::signal::windows::ctrl_break()?,
        tokio::signal::windows::ctrl_c()?,
    );
    #[cfg(windows)]
    let shutdown = async {
        loop {
            tokio::select! {
                _ = ctrl_break.recv() => {}
                _ = ctrl_c.recv() => {}
            }
            if !args.ignore_sigterm {
                break;
            }
        }
    };
    #[cfg(windows)]
    let interrupt = std::future::pending::<std::io::Result<()>>();
    tokio::select! {
        result = server => result?,
        () = deadline => {},
        () = shutdown => {},
        result = interrupt => result?,
    }
    // Descendants intentionally survive parent termination for supervisor cleanup tests.
    // A supplied exit deadline is inherited so failed tests can bound their lifetime.
    if let Some(child) = child.as_mut() {
        let _ = child.try_wait()?;
    }
    Ok(())
}

/// Spawns another copy without inheriting server bindings or environment output flags.
fn spawn_descendant(args: &Args, role: &str) -> std::io::Result<tokio::process::Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["--descendant", role]);
    if args.setsid_grandchild {
        command.arg("--setsid-grandchild");
    }
    if args.ignore_sigterm {
        command.arg("--ignore-sigterm");
    }
    if let Some(duration) = args.exit_after {
        command.args([
            "--exit-after",
            &humantime::format_duration(duration).to_string(),
        ]);
    }
    command.spawn()
}

/// Binds loopback with explicit IPv6-only behavior; port zero requests an ephemeral port.
fn bind(args: &Args) -> std::io::Result<TcpListener> {
    let port = args.bind_other_port.unwrap_or(args.port);
    let addr = if args.bind_ipv6_only {
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))
    } else {
        SocketAddr::from(([127, 0, 0, 1], port))
    };
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    if args.bind_ipv6_only {
        socket.set_only_v6(true)?;
    }
    // A killed earlier server leaves its connections in TIME_WAIT on this port;
    // without this, a test reusing the port right after fails with AddrInUse.
    // A live listener still refuses the bind. Windows lets TIME_WAIT ports be
    // bound anyway, and there the option would let the bind take over a port
    // another socket listens on.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    TcpListener::from_std(socket.into())
}

/// Exposes HTTP diagnostics, WebSocket echo and an unbounded SSE stream.
fn app() -> Router {
    Router::new()
        .route("/ws", get(websocket))
        .route("/sse", get(events))
        .fallback(http)
}

/// Reflects request routing data without disclosing process environment.
async fn http(uri: Uri, headers: HeaderMap) -> Json<serde_json::Value> {
    let headers: BTreeMap<_, _> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or("<binary>")))
        .collect();
    Json(
        serde_json::json!({"path": uri.to_string(), "headers": headers, "pid": std::process::id()}),
    )
}

/// Echoes text and binary frames; the WebSocket implementation handles control frames.
async fn websocket(upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(async |mut socket| {
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                Message::Text(_) | Message::Binary(_) => {
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

/// Emits numbered events every 100 ms so streaming tests detect proxy buffering.
async fn events() -> impl IntoResponse {
    Sse::new(stream::unfold(0_u64, |sequence| async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Some((
            Ok::<_, Infallible>(
                Event::default()
                    .id(sequence.to_string())
                    .data(sequence.to_string()),
            ),
            sequence + 1,
        ))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port overrides and human durations have predictable CLI parsing.
    #[test]
    fn arguments() -> Result<(), clap::Error> {
        let args = Args::try_parse_from([
            "fixture-server",
            "--port",
            "0",
            "--bind-other-port",
            "1234",
            "--exit-after",
            "25ms",
        ])?;
        assert_eq!(args.port, 0);
        assert_eq!(args.bind_other_port, Some(1234));
        assert_eq!(args.exit_after, Some(Duration::from_millis(25)));
        assert!(Args::try_parse_from(["fixture-server", "--exit-after", "bad"]).is_err());
        Ok(())
    }
}
