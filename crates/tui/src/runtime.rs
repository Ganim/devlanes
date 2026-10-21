//! The event loop and the executor that turns effects into daemon calls.
//!
//! Every failure becomes a [`Msg`] for the app, which reports it through one
//! path. A failed channel send only happens after the loop quit, when nobody
//! is left to show the result.

use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as TermEvent, KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use devlanes_proto::AttachParams;
use devlanes_term::{LaunchRequest, TerminalLauncher};
use futures_util::{Stream, StreamExt};
use ratatui::{Terminal, backend::Backend};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, sleep, sleep_until},
};

use crate::{
    Error, Options,
    api::{AttachEvent, AttachInput, DaemonApi},
    app::{App, Effect, Msg},
    input, ui,
};

/// One frame at ~60 Hz; bursts of output within a frame are drawn once.
const FRAME: Duration = Duration::from_millis(16);
const MESSAGE_QUEUE: usize = 1024;
const ATTACH_INPUT_QUEUE: usize = 256;

/// Set while the terminal is in raw mode, so the panic hook restores only then.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Puts the terminal into TUI mode and restores it on drop and on panic.
pub struct TerminalGuard;

impl TerminalGuard {
    /// Enables raw mode, the alternate screen, mouse capture and bracketed paste.
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        // Restores on error too: the guard exists before the remaining setup can fail.
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    /// Leaves TUI mode however the loop ended.
    fn drop(&mut self) {
        restore();
    }
}

/// Undoes [`TerminalGuard::enter`]; safe to call more than once.
fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    // This can run while unwinding, so failures are printed rather than raised;
    // by now the alternate screen is gone and stderr is readable.
    let screen = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let raw = disable_raw_mode();
    if let Err(error) = screen.and(raw) {
        eprintln!("devlanes: could not restore the terminal ({error}); run `reset`");
    }
}

/// Chains a hook that restores the terminal before the panic message prints.
fn install_panic_hook() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Runs the TUI until the user quits. Closing never stops units: the loop only
/// drops its connections, which detaches.
pub async fn drive<B, E>(
    terminal: &mut Terminal<B>,
    api: Arc<dyn DaemonApi>,
    options: &Options,
    mut events: E,
    clipboard: Box<dyn Write + Send>,
) -> Result<(), Error>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    E: Stream<Item = io::Result<TermEvent>> + Unpin,
{
    let size = terminal.size().map_err(backend_error)?;
    let mut app = App::new(size.width, size.height);
    let (sender, mut receiver) = mpsc::channel(MESSAGE_QUEUE);
    let mut executor = Executor::new(api, sender, options, clipboard);
    for effect in app.start() {
        executor.execute(effect);
    }
    if !executor.flush(&mut app) {
        return Ok(());
    }
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut last_draw = Instant::now() - FRAME;
    loop {
        if dirty && last_draw.elapsed() >= FRAME {
            terminal
                .draw(|frame| ui::render(&app, frame))
                .map_err(backend_error)?;
            dirty = false;
            last_draw = Instant::now();
        }
        let next_frame = last_draw + FRAME;
        let msg = tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => terminal_msg(event),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
            msg = receiver.recv() => msg,
            _ = tick.tick() => Some(Msg::Tick),
            _ = sleep_until(next_frame), if dirty => None,
        };
        let Some(msg) = msg else {
            continue;
        };
        dirty = true;
        if !executor.apply(&mut app, msg) {
            return Ok(());
        }
        // Drain what is already queued so a burst of output costs one frame.
        while let Ok(msg) = receiver.try_recv() {
            if !executor.apply(&mut app, msg) {
                return Ok(());
            }
        }
    }
}

/// Converts backend errors into the crate error.
fn backend_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> Error {
    Error::Io(io::Error::other(error))
}

/// Maps crossterm events; key releases are dropped (kitty protocol reports them).
fn terminal_msg(event: TermEvent) -> Option<Msg> {
    match event {
        TermEvent::Key(key) if key.kind != KeyEventKind::Release => Some(Msg::Key(key)),
        TermEvent::Mouse(mouse) => Some(Msg::Mouse(mouse)),
        TermEvent::Paste(text) => Some(Msg::Paste(text)),
        TermEvent::Resize(width, height) => Some(Msg::Resize(width, height)),
        _ => None,
    }
}

/// Sends a result to the loop; false once the loop has quit.
async fn deliver(sender: &mpsc::Sender<Msg>, msg: Msg) -> bool {
    sender.send(msg).await.is_ok()
}

/// [`deliver`] from a blocking thread (URL opener, terminal launcher).
fn deliver_blocking(sender: &mpsc::Sender<Msg>, msg: Msg) -> bool {
    sender.blocking_send(msg).is_ok()
}

/// Owns background tasks and the attached stream's input queue.
struct Executor {
    api: Arc<dyn DaemonApi>,
    sender: mpsc::Sender<Msg>,
    launcher: Result<Arc<dyn TerminalLauncher>, String>,
    clipboard: Box<dyn Write + Send>,
    subscription: Option<JoinHandle<()>>,
    attach: Option<(JoinHandle<()>, mpsc::Sender<AttachInput>)>,
    metrics: Option<JoinHandle<()>>,
    /// The daemon tab's facts request and log read, one of each at a time.
    daemon: Option<JoinHandle<()>>,
    log: Option<JoinHandle<()>>,
    /// Failures found while running an effect, applied before the next message
    /// so they cannot be lost to a full channel.
    local: Vec<Msg>,
}

impl Executor {
    /// Picks the configured terminal launcher once.
    fn new(
        api: Arc<dyn DaemonApi>,
        sender: mpsc::Sender<Msg>,
        options: &Options,
        clipboard: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            api,
            sender,
            launcher: options.launcher(),
            clipboard,
            subscription: None,
            attach: None,
            metrics: None,
            daemon: None,
            log: None,
            local: Vec::new(),
        }
    }

    /// Updates the app, runs its effects and any local failures they caused;
    /// false when the user quit.
    fn apply(&mut self, app: &mut App, msg: Msg) -> bool {
        let running = app
            .update(msg)
            .into_iter()
            .all(|effect| self.execute(effect));
        running && self.flush(app)
    }

    /// Applies queued local failures; they may queue more, so this loops.
    fn flush(&mut self, app: &mut App) -> bool {
        while !self.local.is_empty() {
            for msg in std::mem::take(&mut self.local) {
                let running = app
                    .update(msg)
                    .into_iter()
                    .all(|effect| self.execute(effect));
                if !running {
                    return false;
                }
            }
        }
        true
    }

    /// Queues a local failure for the app.
    fn fail(&mut self, what: &str, message: String) {
        self.local.push(Msg::Failed {
            what: what.into(),
            message,
        });
    }

    /// Runs one effect, returning false for [`Effect::Quit`].
    fn execute(&mut self, effect: Effect) -> bool {
        let api = Arc::clone(&self.api);
        let sender = self.sender.clone();
        match effect {
            Effect::Quit => {
                self.detach();
                return false;
            }
            Effect::Refresh => {
                tokio::spawn(async move {
                    deliver(&sender, Msg::Status(api.status().await)).await;
                });
            }
            Effect::Subscribe => {
                // A reconnect replaces the old subscription so events are not doubled.
                if let Some(task) = self.subscription.take() {
                    task.abort();
                }
                self.subscription = Some(tokio::spawn(async move {
                    let mut stream = match api.subscribe().await {
                        Ok(stream) => stream,
                        Err(error) => {
                            deliver(&sender, Msg::Subscribed(Err(error))).await;
                            return;
                        }
                    };
                    if !deliver(&sender, Msg::Subscribed(Ok(()))).await {
                        return;
                    }
                    let reason = loop {
                        match stream.next().await {
                            Some(Ok(event)) => {
                                if !deliver(&sender, Msg::Event(event)).await {
                                    return;
                                }
                            }
                            Some(Err(error)) => break Some(error),
                            None => break None,
                        }
                    };
                    deliver(&sender, Msg::EventsEnded(reason)).await;
                }));
            }
            Effect::ScheduleReconnect { after_ms } => {
                tokio::spawn(async move {
                    sleep(Duration::from_millis(after_ms)).await;
                    deliver(&sender, Msg::Reconnect).await;
                });
            }
            Effect::Attach {
                generation,
                lane,
                unit,
                columns,
                rows,
            } => {
                self.detach();
                let (input_tx, input_rx) = mpsc::channel(ATTACH_INPUT_QUEUE);
                let params = AttachParams {
                    unit,
                    interactive: true,
                    columns: Some(columns),
                    rows: Some(rows),
                };
                let task =
                    tokio::spawn(pump_attach(api, sender, generation, lane, params, input_rx));
                self.attach = Some((task, input_tx));
            }
            Effect::Detach => self.detach(),
            Effect::Input(bytes) => self.send_attach(AttachInput::Bytes(Bytes::from(bytes))),
            Effect::ResizePty { columns, rows } => {
                self.send_attach(AttachInput::Resize { columns, rows });
            }
            Effect::Control {
                method,
                lane,
                selection,
                label,
                target,
            } => {
                tokio::spawn(async move {
                    let result = api.control(method, lane, selection).await;
                    let msg = Msg::ActionDone {
                        label,
                        target,
                        result,
                    };
                    deliver(&sender, msg).await;
                });
            }
            Effect::Run { lane, params } => {
                tokio::spawn(async move {
                    let task = params.task.clone();
                    let result = api.run_task(lane.clone(), params).await;
                    deliver(&sender, Msg::RunDone { lane, task, result }).await;
                });
            }
            Effect::FetchLogs {
                generation,
                lane,
                query,
                older,
            } => {
                tokio::spawn(async move {
                    let result = api.logs(lane, query).await;
                    let msg = Msg::Logs {
                        generation,
                        older,
                        result,
                    };
                    deliver(&sender, msg).await;
                });
            }
            Effect::FetchMetrics => {
                // A slow daemon must not pile up one request per tick.
                if self
                    .metrics
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    return true;
                }
                self.metrics = Some(tokio::spawn(async move {
                    deliver(&sender, Msg::Metrics(api.metrics().await)).await;
                }));
            }
            Effect::OpenUrl(url) => {
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = open_url(&url) {
                        let msg = Msg::Failed {
                            what: format!("open {url}"),
                            message: error.to_string(),
                        };
                        deliver_blocking(&sender, msg);
                    }
                });
            }
            Effect::OpenTerminal { cwd, attach, title } => self.open_terminal(cwd, attach, title),
            Effect::Copy(text) => {
                let written = self
                    .clipboard
                    .write_all(input::osc52(&text).as_bytes())
                    .and_then(|()| self.clipboard.flush());
                if let Err(error) = written {
                    self.fail("copy", error.to_string());
                }
            }
            Effect::CleanLane { lane } => {
                tokio::spawn(async move {
                    let result = api.clean_lane(lane.clone()).await;
                    deliver(&sender, Msg::CleanDone { lane, result }).await;
                });
            }
            Effect::LaneGc => {
                tokio::spawn(async move {
                    deliver(&sender, Msg::GcDone(api.lane_gc().await)).await;
                });
            }
            Effect::FetchDaemon => {
                // A slow daemon must not pile up one request per refresh.
                if self.daemon.as_ref().is_some_and(|task| !task.is_finished()) {
                    return true;
                }
                self.daemon = Some(tokio::spawn(async move {
                    let (status, doctor) = tokio::join!(api.daemon_status(), api.doctor());
                    deliver(&sender, Msg::DaemonInfo { status, doctor }).await;
                }));
            }
            Effect::ReadDaemonLog { offset } => {
                if self.log.as_ref().is_some_and(|task| !task.is_finished()) {
                    return true;
                }
                let Some(path) = api.daemon_log() else {
                    self.local.push(Msg::DaemonLog(Err(
                        "this daemon has no local log file".into()
                    )));
                    return true;
                };
                self.log = Some(tokio::task::spawn_blocking(move || {
                    let result = crate::logfile::read(&path, offset)
                        .map_err(|error| format!("{}: {error}", path.display()));
                    deliver_blocking(&sender, Msg::DaemonLog(result));
                }));
            }
            Effect::StopDaemon => {
                tokio::spawn(async move {
                    deliver(&sender, Msg::DaemonStopped(api.stop_daemon().await)).await;
                });
            }
            Effect::RestartDaemon => {
                tokio::spawn(async move {
                    deliver(&sender, Msg::DaemonRestarted(api.restart_daemon().await)).await;
                });
            }
            Effect::StartDaemon => {
                tokio::spawn(async move {
                    deliver(&sender, Msg::DaemonStarted(api.start_daemon().await)).await;
                });
            }
            Effect::FetchProfiles { lane } => {
                tokio::spawn(async move {
                    let result = api.profile_list(lane.clone()).await;
                    deliver(&sender, Msg::Profiles { lane, result }).await;
                });
            }
        }
        true
    }

    /// Stops the attach pump; dropping its connection detaches without stopping the unit.
    fn detach(&mut self) {
        if let Some((task, _)) = self.attach.take() {
            task.abort();
        }
    }

    /// Queues input for the attached stream; a full queue drops the input and says so.
    fn send_attach(&mut self, input: AttachInput) {
        let Some((_, sender)) = &self.attach else {
            self.fail("input", "no terminal is attached".into());
            return;
        };
        match sender.try_send(input) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.fail("input", "dropped: the unit is not reading its input".into());
            }
            // The stream ended; its own message explains why.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Opens a new terminal window through the configured launcher.
    fn open_terminal(&mut self, cwd: PathBuf, attach: Option<String>, title: String) {
        let sender = self.sender.clone();
        let launcher = match &self.launcher {
            Ok(launcher) => Arc::clone(launcher),
            Err(message) => {
                let message = message.clone();
                self.fail("open terminal", message);
                return;
            }
        };
        tokio::task::spawn_blocking(move || {
            let command = match attach {
                Some(unit) => {
                    // The same binary attaches, so client and daemon versions match.
                    let exe = std::env::current_exe()
                        .map_or_else(|_| "devlanes".to_owned(), |p| p.display().to_string());
                    vec![exe, "attach".into(), unit]
                }
                None => vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())],
            };
            let request = LaunchRequest {
                cwd,
                command,
                title: Some(title),
            };
            if let Err(error) = launcher.launch(&request) {
                let msg = Msg::Failed {
                    what: "open terminal".into(),
                    message: error.to_string(),
                };
                deliver_blocking(&sender, msg);
            }
        });
    }
}

impl Drop for Executor {
    /// Closes the subscription and attach connections when the loop ends, even
    /// if the caller's runtime keeps running. This detaches; it stops nothing.
    fn drop(&mut self) {
        self.detach();
        if let Some(task) = self.subscription.take() {
            task.abort();
        }
        if let Some(task) = self.daemon.take() {
            task.abort();
        }
        if let Some(task) = self.metrics.take() {
            task.abort();
        }
    }
}

/// Opens the attach stream, then forwards output and errors to the app and
/// input to the daemon.
async fn pump_attach(
    api: Arc<dyn DaemonApi>,
    sender: mpsc::Sender<Msg>,
    generation: u64,
    lane: String,
    params: AttachParams,
    mut input: mpsc::Receiver<AttachInput>,
) {
    let session = match api.attach(lane, params).await {
        Ok(session) => session,
        Err(error) => {
            let msg = Msg::AttachOpened {
                generation,
                result: Err(error),
            };
            deliver(&sender, msg).await;
            return;
        }
    };
    let (result, mut output, remote) = session.split();
    let opened = Msg::AttachOpened {
        generation,
        result: Ok(result),
    };
    if !deliver(&sender, opened).await {
        return;
    }
    loop {
        tokio::select! {
            event = output.recv() => {
                let msg = match event {
                    Some(AttachEvent::Output(bytes)) => Msg::AttachOutput { generation, bytes: bytes.to_vec() },
                    Some(AttachEvent::Error(error)) => Msg::StreamError { generation, error },
                    None => break,
                };
                if !deliver(&sender, msg).await {
                    return;
                }
            },
            request = input.recv() => match request {
                Some(request) => {
                    // A closed remote means the stream ended; keep reading output,
                    // which delivers the reason and then closes.
                    if remote.send(request).await.is_err() {
                        continue;
                    }
                }
                None => return,
            },
        }
    }
    deliver(&sender, Msg::AttachClosed { generation }).await;
}

/// Hands a URL to the desktop's opener.
fn open_url(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let status = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} exited with {status}")))
    }
}
