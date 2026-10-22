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
    detached_grandchild: bool,
    #[arg(long)]
    breakaway_grandchild: bool,
    #[arg(long)]
    print_console_width: bool,
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
    #[arg(long, hide = true, value_parser = ["child", "grandchild"])]
    descendant: Option<String>,
}

/// Starts the fixture; errors produce a nonzero exit and diagnostics on stderr.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    platform::check(&args)?;
    let mut terminate = platform::Terminate::new()?;
    if args.descendant.as_deref() == Some("grandchild") && args.setsid_grandchild {
        platform::setsid()?;
    }

    for line in &args.log_line {
        println!("{line}");
    }
    if args.print_console_width {
        match platform::console_width() {
            Some(width) => println!("CONSOLE_WIDTH {width}"),
            None => println!("CONSOLE_WIDTH none"),
        }
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
    } else if args.descendant.is_none()
        && (args.spawn_grandchild
            || args.setsid_grandchild
            || args.detached_grandchild
            || args.breakaway_grandchild)
    {
        Some(spawn_descendant(&args, "child")?)
    } else {
        None
    };

    if let Some(role) = &args.descendant {
        println!("{role} {}", platform::identity()?);
    } else if let Some(listener) = &listener {
        println!("LISTENING {}", listener.local_addr()?);
    }
    std::io::stdout().flush()?;

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
    let shutdown = async {
        loop {
            terminate.recv().await;
            if !args.ignore_sigterm {
                break;
            }
        }
    };
    tokio::select! {
        result = server => result?,
        () = deadline => {},
        () = shutdown => {},
        result = platform::interrupt() => result?,
    }
    // Descendants intentionally survive parent termination for supervisor cleanup tests.
    // A supplied exit deadline is inherited so failed tests can bound their lifetime.
    if let Some(child) = child.as_mut() {
        let _ = child.try_wait()?;
    }
    Ok(())
}

/// Unix signals and sessions.
#[cfg(unix)]
mod platform {
    use tokio::signal::unix::{Signal, SignalKind, signal};

    use super::Args;

    /// Refuses the Windows-only flags, before anything is spawned.
    pub fn check(args: &Args) -> Result<(), String> {
        for (set, flag) in [
            (args.detached_grandchild, "--detached-grandchild"),
            (args.breakaway_grandchild, "--breakaway-grandchild"),
            (args.print_console_width, "--print-console-width"),
        ] {
            if set {
                return Err(format!("{flag} needs a Windows console or job"));
            }
        }
        Ok(())
    }

    /// SIGINT ends the server even with `--ignore-sigterm`.
    pub async fn interrupt() -> std::io::Result<()> {
        tokio::signal::ctrl_c().await
    }

    /// Unreachable: [`check`] rejects `--print-console-width` first.
    pub fn console_width() -> Option<u16> {
        None
    }

    /// Nothing to add: `--setsid-grandchild` acts in the grandchild itself.
    pub fn configure_grandchild(_command: &mut tokio::process::Command, _args: &Args) {}

    /// SIGTERM deliveries, subscribed before anything else runs so an early stop
    /// isn't lost.
    pub struct Terminate(Signal);

    impl Terminate {
        /// Subscribes to SIGTERM.
        pub fn new() -> std::io::Result<Self> {
            Ok(Self(signal(SignalKind::terminate())?))
        }

        /// Waits for the next SIGTERM.
        pub async fn recv(&mut self) {
            self.0.recv().await;
        }
    }

    /// Makes this process lead a new session (`--setsid-grandchild`).
    pub fn setsid() -> rustix::io::Result<()> {
        rustix::process::setsid()?;
        Ok(())
    }

    /// `pid=<pid> sid=<session>`, announced by descendants for session assertions.
    pub fn identity() -> rustix::io::Result<String> {
        Ok(format!(
            "pid={} sid={}",
            std::process::id(),
            rustix::process::getsid(None)?.as_raw_nonzero()
        ))
    }
}

/// Windows console control events and process creation flags.
#[cfg(windows)]
mod platform {
    use tokio::signal::windows::{CtrlBreak, CtrlC, ctrl_break, ctrl_c};

    use super::Args;

    /// `DETACHED_PROCESS`: the grandchild gets no console at all.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    /// `CREATE_BREAKAWAY_FROM_JOB`: the grandchild leaves the unit's job, which
    /// fails with access denied unless the job allows breakaway.
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    /// Refuses flags that need Unix sessions, before anything is spawned.
    pub fn check(args: &Args) -> Result<(), String> {
        if args.setsid_grandchild {
            return Err("--setsid-grandchild needs Unix sessions".into());
        }
        Ok(())
    }

    /// Ctrl-Break and Ctrl-C, the console events a graceful stop sends; they are
    /// what `--ignore-sigterm` ignores here. While these listeners live, tokio's
    /// handler reports the events as handled, so the default handler never ends
    /// the process. Ctrl-C only arrives when the process does not inherit the
    /// "ignore Ctrl-C" flag (`CREATE_NEW_PROCESS_GROUP` sets it).
    pub struct Terminate {
        ctrl_c: CtrlC,
        ctrl_break: CtrlBreak,
    }

    impl Terminate {
        /// Subscribes to both events before anything else runs.
        pub fn new() -> std::io::Result<Self> {
            Ok(Self {
                ctrl_c: ctrl_c()?,
                ctrl_break: ctrl_break()?,
            })
        }

        /// Waits for the next Ctrl-C or Ctrl-Break.
        pub async fn recv(&mut self) {
            tokio::select! {
                _ = self.ctrl_c.recv() => {}
                _ = self.ctrl_break.recv() => {}
            }
        }
    }

    /// Never fires: [`Terminate`] already owns Ctrl-C, so `--ignore-sigterm`
    /// covers it too.
    pub async fn interrupt() -> std::io::Result<()> {
        std::future::pending().await
    }

    /// Columns of the attached console's window, read through `CONOUT$` so a piped
    /// stdout still reports the console (a ConPTY or a hidden one); `None` without
    /// a console, as under `DETACHED_PROCESS`.
    pub fn console_width() -> Option<u16> {
        crossterm::terminal::size().ok().map(|(columns, _)| columns)
    }

    /// Applies `--detached-grandchild` and `--breakaway-grandchild` to the spawn of
    /// the grandchild.
    pub fn configure_grandchild(command: &mut tokio::process::Command, args: &Args) {
        let mut flags = 0;
        if args.detached_grandchild {
            flags |= DETACHED_PROCESS;
        }
        if args.breakaway_grandchild {
            flags |= CREATE_BREAKAWAY_FROM_JOB;
        }
        if flags != 0 {
            command.creation_flags(flags);
        }
    }

    /// Unreachable: [`check`] rejects `--setsid-grandchild` first.
    pub fn setsid() -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    /// `pid=<pid>`; Windows has no session id to report.
    pub fn identity() -> std::io::Result<String> {
        Ok(format!("pid={}", std::process::id()))
    }
}

/// Spawns another copy without inheriting server bindings or environment output flags.
fn spawn_descendant(args: &Args, role: &str) -> std::io::Result<tokio::process::Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["--descendant", role]);
    for (set, flag) in [
        (args.setsid_grandchild, "--setsid-grandchild"),
        (args.detached_grandchild, "--detached-grandchild"),
        (args.breakaway_grandchild, "--breakaway-grandchild"),
    ] {
        if set {
            command.arg(flag);
        }
    }
    if role == "grandchild" {
        platform::configure_grandchild(&mut command, args);
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
    // A live listener still refuses the bind.
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
