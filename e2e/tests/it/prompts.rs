//! ssh's login prompts that `remote connect` relays: the real CLI shows them in
//! the terminal it runs in and sends the answers back. A fake daemon stands in
//! for the daemon and ssh, so no box is needed: it answers a `connect` request
//! with `remote.prompt` events and records the answer frames the CLI sends.

use std::{
    io::{Read, Write},
    num::NonZeroU16,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use devlanes_e2e::{Error, Lab, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::BUDGET;

/// The daemon numbers prompt streams from here, far above attach streams.
const FIRST_STREAM: u64 = 1 << 40;
/// How long a prompt may take to show, and the CLI to exit after the last answer.
const WAIT: Duration = Duration::from_secs(30);

/// A secret, a confirmation and a notice, the kinds ssh asks through askpass.
const PROMPTS: [(&str, &str); 3] = [
    ("secret", "Enter passphrase for key 'acme_ed25519':"),
    (
        "confirm",
        "Are you sure you want to continue connecting (yes/no/[fingerprint])?",
    ),
    ("notify", "Confirm user presence for key ED25519-SK acme"),
];

/// Passphrase prompts hide what is typed, confirmations show it, notices only
/// print; every answer reaches the daemon with its newline.
#[test]
fn connect_prompts_show_in_the_terminal_and_hide_secrets() -> Result<()> {
    let lab = Lab::without_config(BUDGET)?;
    let daemon = PromptDaemon::serve(&lab, &PROMPTS)?;
    let mut terminal = Terminal::run(&lab, &["remote", "connect", "acme", "--background"])?;
    terminal.wait_for("[acme] Enter passphrase for key 'acme_ed25519':")?;
    terminal.keys(b"hunter2-acme\r")?;
    terminal.wait_for("(yes/no/[fingerprint])?")?;
    terminal.keys(b"yes\r")?;
    terminal.wait_for("(yes/no/[fingerprint])? yes")?;
    terminal.wait_for("[acme] Confirm user presence for key ED25519-SK acme")?;
    let code = terminal.exit_code()?;
    let screen = terminal.text();
    assert!(
        !screen.contains("hunter2"),
        "the passphrase echoed:\n{screen}"
    );
    assert_eq!(code, 0, "{screen}");
    assert_eq!(
        daemon.answers(),
        ["hunter2-acme\n", "yes\n", "\n"],
        "{screen}"
    );
    assert!(!lab.daemon_running(), "a real daemon started");
    Ok(())
}

/// Ctrl-C at a passphrase prompt detaches like Ctrl-C anywhere in `connect`,
/// instead of leaving the CLI waiting for a line.
#[test]
fn ctrl_c_at_a_prompt_detaches() -> Result<()> {
    let lab = Lab::without_config(BUDGET)?;
    let _daemon = PromptDaemon::serve(&lab, &PROMPTS[..1])?;
    // A Git Bash or nextest parent may have set the inherited "ignore Ctrl-C"
    // flag, which would keep Ctrl-C from the CLI, as it would from any program
    // started under it.
    #[cfg(windows)]
    devlanes_sys::windows::process::accept_ctrl_c()?;
    let mut terminal = Terminal::run(&lab, &["remote", "connect", "acme"])?;
    terminal.wait_for("Enter passphrase")?;
    terminal.keys(b"\x03")?;
    let code = terminal.exit_code()?;
    assert_eq!(code, 130, "{}", terminal.text());
    Ok(())
}

/// A console without a window, as agents and editors give the commands they
/// run, has no keyboard: the prompt is cancelled at once instead of waiting.
#[cfg(windows)]
#[test]
fn prompts_without_a_console_window_are_cancelled() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let lab = Lab::without_config(BUDGET)?;
    let daemon = PromptDaemon::serve(&lab, &PROMPTS[..1])?;
    let mut command = lab.command(lab.repo(), &["remote", "connect", "acme", "--background"])?;
    command.creation_flags(devlanes_sys::windows::process::CREATE_NO_WINDOW);
    let output = lab.run(command, WAIT)?;
    output.expect_code(0)?;
    assert_eq!(daemon.answers(), [""], "{output}");
    assert!(!output.stderr.contains("mintty"), "{output}");
    Ok(())
}

/// Under mintty the standard streams are MSYS2 pseudo terminals and the
/// console is hidden: the prompt shows on stderr with the reason it is
/// cancelled, and a notice is printed there.
#[cfg(windows)]
#[test]
fn prompts_under_mintty_are_cancelled_with_a_reason() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let lab = Lab::without_config(BUDGET)?;
    let notice_first = [PROMPTS[2], PROMPTS[0]];
    let daemon = PromptDaemon::serve(&lab, &notice_first)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let _context = runtime.enter();
    let name = format!(
        r"\\.\pipe\msys-{:016x}-pty0-from-master",
        std::process::id()
    );
    // Held until the CLI exits; it never reads from it.
    let _pty = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)?;
    let stdin = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)?;
    let mut command = lab.command(lab.repo(), &["remote", "connect", "acme", "--background"])?;
    command
        .stdin(stdin)
        .creation_flags(devlanes_sys::windows::process::CREATE_NO_WINDOW);
    let output = lab.run(command, WAIT)?;
    output.expect_code(0)?;
    assert_eq!(daemon.answers(), ["\n", ""], "{output}");
    for line in [
        "[acme] Confirm user presence for key ED25519-SK acme",
        "[acme] Enter passphrase for key 'acme_ed25519':",
        "mintty does not pass typed keys to Windows programs",
    ] {
        assert!(output.stderr.contains(line), "{line}: {output}");
    }
    Ok(())
}

/// The daemon's endpoint served by a thread that answers `connect` with
/// prompts and every other request with an empty result. Dropping it stops
/// the thread.
struct PromptDaemon {
    answers: Arc<Mutex<Vec<String>>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PromptDaemon {
    /// Serves the lab's endpoint before returning, so the CLI never finds it
    /// missing and spawns a real daemon. `prompts` are `(kind, text)` pairs.
    fn serve(lab: &Lab, prompts: &[(&str, &str)]) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let listener = {
            let _context = runtime.enter();
            Listener::bind(lab)?
        };
        let prompts: Arc<Vec<(String, String)>> = Arc::new(
            prompts
                .iter()
                .map(|&(kind, text)| (kind.to_owned(), text.to_owned()))
                .collect(),
        );
        let answers = Arc::new(Mutex::new(Vec::new()));
        let recorded = answers.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = thread::spawn(move || {
            runtime.block_on(async move {
                tokio::select! {
                    () = listener.accept_each(prompts, recorded) => {}
                    _ = stopped => {}
                    () = tokio::time::sleep(BUDGET) => {}
                }
            });
        });
        Ok(Self {
            answers,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    /// The answers received so far, in prompt order.
    fn answers(&self) -> Vec<String> {
        self.answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for PromptDaemon {
    /// Stops accepting; open connections end with the runtime.
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `<home>/run/daemon.sock`, in a private `run` as the client expects.
#[cfg(unix)]
struct Listener(tokio::net::UnixListener);

#[cfg(unix)]
impl Listener {
    /// Binds the lab's daemon socket.
    fn bind(lab: &Lab) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let run = lab.home().join("run");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&run)?;
        Ok(Self(tokio::net::UnixListener::bind(
            run.join("daemon.sock"),
        )?))
    }

    /// Serves every client on its own task.
    async fn accept_each(
        self,
        prompts: Arc<Vec<(String, String)>>,
        answers: Arc<Mutex<Vec<String>>>,
    ) {
        while let Ok((stream, _)) = self.0.accept().await {
            tokio::spawn(connection(stream, prompts.clone(), answers.clone()));
        }
    }
}

/// The named pipe the client derives from the lab's home.
#[cfg(windows)]
struct Listener {
    name: String,
    first: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl Listener {
    /// Creates the first pipe instance; `daemon status` names the pipe without
    /// starting a daemon.
    fn bind(lab: &Lab) -> Result<Self> {
        let status = lab.devlanes(lab.repo(), &["daemon", "status", "--json"])?;
        let name = crate::need(status.json()?["socket"].as_str(), "pipe name")?.to_owned();
        let first = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)?;
        Ok(Self { name, first })
    }

    /// Serves every client on its own task. The next instance exists before a
    /// connected one is handed off, so the pipe never goes missing.
    async fn accept_each(
        self,
        prompts: Arc<Vec<(String, String)>>,
        answers: Arc<Mutex<Vec<String>>>,
    ) {
        let mut server = self.first;
        while server.connect().await.is_ok() {
            let Ok(next) = tokio::net::windows::named_pipe::ServerOptions::new().create(&self.name)
            else {
                return;
            };
            let connected = std::mem::replace(&mut server, next);
            tokio::spawn(connection(connected, prompts.clone(), answers.clone()));
        }
    }
}

/// One client: echoes its hello, so versions always match, then answers its
/// requests until it hangs up.
async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    prompts: Arc<Vec<(String, String)>>,
    answers: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut hello = control(&read_frame(&mut stream).await?)?;
    hello["body"]["managed_processes"] = json!(0);
    write_control(&mut stream, &hello).await?;
    loop {
        let Ok(request) = control(&read_frame(&mut stream).await?) else {
            continue;
        };
        if request["type"] != "request" {
            continue;
        }
        let body = &request["body"];
        let data = if body["method"] == "connect" {
            let host = body["params"]["host"].clone();
            for (stream_id, (kind, prompt)) in (FIRST_STREAM..).zip(prompts.iter()) {
                let event = json!({"type": "event", "body": {
                    "seq": 0,
                    "event": "remote.prompt",
                    "data": {"host": host, "prompt": prompt, "kind": kind, "stream_id": stream_id},
                }});
                write_control(&mut stream, &event).await?;
                let answer = loop {
                    let frame = read_frame(&mut stream).await?;
                    if frame.first() == Some(&1)
                        && frame.get(1..9) == Some(&stream_id.to_be_bytes()[..])
                    {
                        break String::from_utf8_lossy(&frame[9..]).into_owned();
                    }
                };
                answers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(answer);
            }
            json!({"host": host, "state": "connected"})
        } else {
            json!({})
        };
        let response = json!({"type": "response", "body": {
            "id": body["id"], "status": "ok", "data": data,
        }});
        write_control(&mut stream, &response).await?;
    }
}

/// The JSON of a control frame (kind byte 0).
fn control(frame: &[u8]) -> std::io::Result<Value> {
    match frame.split_first() {
        Some((0, json)) => serde_json::from_slice(json).map_err(std::io::Error::other),
        _ => Err(std::io::Error::other("not a control frame")),
    }
}

/// One frame's payload: a u32 big-endian length, then that many bytes.
async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let mut payload = vec![0u8; u32::from_be_bytes(length).min(1 << 20) as usize];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Sends `value` as a control frame.
async fn write_control(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &Value,
) -> std::io::Result<()> {
    let mut payload = vec![0u8];
    payload.extend_from_slice(&serde_json::to_vec(value)?);
    let length = u32::try_from(payload.len()).map_err(std::io::Error::other)?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await
}

/// The real CLI in a pseudo terminal (a pseudo console on Windows), with its
/// screen parsed as it prints.
struct Terminal {
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Terminal {
    /// Runs `devlanes <args>` in the lab's repository at 120x30.
    fn run(lab: &Lab, args: &[&str]) -> Result<Self> {
        let (columns, rows) = (120, 30);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(setup)?;
        let template = lab.command(lab.repo(), args)?;
        let mut command = CommandBuilder::new(template.get_program());
        command.args(template.get_args());
        command.cwd(lab.repo());
        command.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).map_err(setup)?;
        drop(pair.slave);
        if let Some(pid) = child.process_id() {
            lab.track(pid.cast_signed());
        }
        let cells = |value: u16| NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            cells(rows),
            cells(columns),
            0,
        )));
        let mut reader = pair.master.try_clone_reader().map_err(setup)?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(setup)?));
        let (parsed, answer) = (screen.clone(), writer.clone());
        // Ends when the pseudo terminal closes.
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let chunk = &buffer[..read];
                let Ok(mut parser) = parsed.lock() else {
                    break;
                };
                parser.process(chunk);
                // The pseudo console asks where the cursor is when it starts.
                if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                    let (row, column) = parser.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", row + 1, column + 1);
                    if let Ok(mut writer) = answer.lock() {
                        let _ = writer.write_all(reply.as_bytes());
                        let _ = writer.flush();
                    }
                }
            }
        });
        Ok(Self {
            screen,
            writer,
            _master: pair.master,
            child,
        })
    }

    /// The screen's text.
    fn text(&self) -> String {
        self.screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screen()
            .contents()
    }

    /// Waits for `text` on the screen.
    fn wait_for(&self, text: &str) -> Result<()> {
        let start = Instant::now();
        while !self.text().contains(text) {
            if start.elapsed() > WAIT {
                return Err(Error::Unexpected(format!(
                    "{text} never showed\n{}",
                    self.text()
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Types `bytes` as if from the keyboard.
    fn keys(&mut self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Waits for the CLI to exit and returns its status.
    fn exit_code(&mut self) -> Result<u32> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status.exit_code());
            }
            if start.elapsed() > WAIT {
                return Err(Error::Unexpected(format!(
                    "the CLI did not exit\n{}",
                    self.text()
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Terminal {
    /// Kills the CLI if a failed check left it running.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Maps a pseudo-terminal failure into the harness error.
fn setup(error: impl std::fmt::Display) -> Error {
    Error::Setup(format!("pseudo-terminal: {error}"))
}
