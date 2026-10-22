//! The event loop and the executor that turns effects into daemon calls.
//!
//! Every failure becomes a [`Msg`] for the app, which reports it through one
//! path. A failed channel send only happens after the loop quit, when nobody
//! is left to show the result.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
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
use devlanes_term::{LaunchRequest, Progress, ProgressBar, Wrap};
use futures_util::{Stream, StreamExt};
use ratatui::{Terminal, backend::Backend};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, sleep, sleep_until},
};

use crate::{
    Error, Options,
    api::{AttachEvent, AttachInput, DaemonApi},
    app::{App, Effect, Msg},
    input,
    start::{StartExit, StartFrame, StartHandoff},
    ui,
};

/// One frame at ~60 Hz; bursts of output within a frame are drawn once.
const FRAME: Duration = Duration::from_millis(16);
const MESSAGE_QUEUE: usize = 1024;
const ATTACH_INPUT_QUEUE: usize = 256;
/// Queued messages applied per frame before drawing again.
const MAX_DRAIN: usize = 512;
/// Larger copies are refused: terminals drop OSC 52 writes past roughly this size.
const MAX_COPY_BYTES: usize = 100 * 1024;

/// Set while the terminal is in raw mode, so the panic hook restores only then.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Wakes the loop after a panic on any thread, so it ends instead of drawing
/// over the terminal the hook already restored.
static PANICKED: OnceLock<Notify> = OnceLock::new();
/// The progress bar's wrap (0 off, 1 bare, 2 tmux), so [`restore`] can clear
/// it after a panic.
static PROGRESS: AtomicU8 = AtomicU8::new(0);

/// [`PROGRESS`]'s code for `wrap`.
fn wrap_code(wrap: Wrap) -> u8 {
    match wrap {
        Wrap::Bare => 1,
        Wrap::Tmux => 2,
    }
}

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
    // A panic skips the executor's drop, which would clear the tab's progress bar.
    let wrap = match PROGRESS.load(Ordering::SeqCst) {
        1 => Some(Wrap::Bare),
        2 => Some(Wrap::Tmux),
        _ => None,
    };
    if let Some(wrap) = wrap {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(wrap.apply(&Progress::Clear.sequence()).as_bytes());
        let _ = stdout.flush();
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
        if let Some(panicked) = PANICKED.get() {
            panicked.notify_one();
        }
    }));
}

/// Runs the TUI until the user quits. Closing never stops units: the loop only
/// drops its connections, which detaches.
pub async fn drive<B, E>(
    terminal: &mut Terminal<B>,
    api: Arc<dyn DaemonApi>,
    options: &Options,
    events: E,
    clipboard: Box<dyn Write + Send>,
) -> Result<(), Error>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    E: Stream<Item = io::Result<TermEvent>> + Unpin,
{
    run_loop(terminal, api, options, events, clipboard, None)
        .await
        .map(drop)
}

/// [`drive`] for a start sent before the TUI opened (`devlanes start` on a
/// terminal): the TUI follows it and says how it stood when the user quit.
pub async fn drive_start<B, E>(
    terminal: &mut Terminal<B>,
    api: Arc<dyn DaemonApi>,
    options: &Options,
    events: E,
    clipboard: Box<dyn Write + Send>,
    start: StartHandoff,
) -> Result<StartExit, Error>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    E: Stream<Item = io::Result<TermEvent>> + Unpin,
{
    let app = run_loop(terminal, api, options, events, clipboard, Some(start)).await?;
    Ok(app.start_exit().unwrap_or(StartExit::Pending))
}

/// The next frame of the start, if there is one; a finished stream is dropped
/// and never resolves again, so the loop's other branches carry on.
async fn next_start(start: &mut Option<mpsc::Receiver<StartFrame>>) -> StartFrame {
    if let Some(stream) = start.as_mut()
        && let Some(frame) = stream.recv().await
    {
        return frame;
    }
    *start = None;
    std::future::pending().await
}

/// The event loop behind [`drive`] and [`drive_start`]; returns the app as it
/// was when the user quit.
async fn run_loop<B, E>(
    terminal: &mut Terminal<B>,
    api: Arc<dyn DaemonApi>,
    options: &Options,
    mut events: E,
    clipboard: Box<dyn Write + Send>,
    start: Option<StartHandoff>,
) -> Result<App, Error>
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
    executor.execute(Effect::LoadNotices);
    if !executor.flush(&mut app) {
        return Ok(app);
    }
    let mut start_stream = None;
    if let Some(start) = start {
        app.open_start(start.label);
        for frame in start.frames {
            if !executor.apply(&mut app, Msg::Start(frame)) {
                return Ok(app);
            }
        }
        start_stream = Some(start.stream);
    }
    let panicked = PANICKED.get_or_init(Notify::new);
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
                None => return Ok(app),
            },
            msg = receiver.recv() => msg,
            frame = next_start(&mut start_stream) => Some(Msg::Start(frame)),
            _ = tick.tick() => Some(Msg::Tick),
            _ = sleep_until(next_frame), if dirty => None,
            // The panic hook already restored the terminal; a UI drawing over
            // the shell afterwards would only make things worse.
            () = panicked.notified() => return Err(Error::Panicked),
        };
        let Some(msg) = msg else {
            continue;
        };
        dirty = true;
        let ticked = matches!(msg, Msg::Tick);
        if !executor.apply(&mut app, msg) {
            return Ok(app);
        }
        // Drain what is already queued so a burst of output costs one frame,
        // but stop after a while so a flood cannot hold the screen still.
        for _ in 0..MAX_DRAIN {
            let Ok(msg) = receiver.try_recv() else {
                break;
            };
            if !executor.apply(&mut app, msg) {
                return Ok(app);
            }
        }
        executor.show_progress(app.start_progress(), ticked);
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
    /// Launchers and the configured terminal; picked per launch, off the loop.
    options: Options,
    /// The terminal output for OSC sequences: clipboard copies and the tab's
    /// progress bar, which clears when the executor drops.
    out: ProgressBar<Box<dyn Write + Send>>,
    /// Set inside tmux, whose OSC 52 needs passthrough wrapping.
    in_tmux: bool,
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
    /// Keeps the options for launching terminals later.
    fn new(
        api: Arc<dyn DaemonApi>,
        sender: mpsc::Sender<Msg>,
        options: &Options,
        clipboard: Box<dyn Write + Send>,
    ) -> Self {
        if let Some(wrap) = options.progress {
            PROGRESS.store(wrap_code(wrap), Ordering::SeqCst);
        }
        Self {
            api,
            sender,
            options: options.clone(),
            out: ProgressBar::new(clipboard, options.progress),
            in_tmux: std::env::var_os("TMUX").is_some(),
            subscription: None,
            attach: None,
            metrics: None,
            daemon: None,
            log: None,
            local: Vec::new(),
        }
    }

    /// Writes the tab's progress bar when it changed, and every second while it
    /// shows: Ghostty drops a bar that is not repeated.
    fn show_progress(&mut self, progress: Progress, tick: bool) {
        let shown = self.out.shown().unwrap_or(Progress::Clear);
        if shown != progress || tick && shown != Progress::Clear {
            // The bar is decoration; a failed write is not worth a notice.
            let _ = self.out.set(progress);
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
                interactive,
            } => {
                self.detach();
                let (input_tx, input_rx) = mpsc::channel(ATTACH_INPUT_QUEUE);
                let params = AttachParams {
                    unit,
                    interactive,
                    columns: Some(columns),
                    rows: Some(rows),
                };
                let task =
                    tokio::spawn(pump_attach(api, sender, generation, lane, params, input_rx));
                self.attach = Some((task, input_tx));
            }
            Effect::SettleResize { token, after_ms } => {
                tokio::spawn(async move {
                    sleep(Duration::from_millis(after_ms)).await;
                    deliver(&sender, Msg::ResizeSettled { token }).await;
                });
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
                if text.len() > MAX_COPY_BYTES {
                    self.fail(
                        "copy",
                        "the selection is over 100 KiB; terminals drop clipboard writes that large"
                            .into(),
                    );
                    return true;
                }
                let sequence = input::osc52(&text, self.in_tmux);
                let out = self.out.get_mut();
                let written = out
                    .write_all(sequence.as_bytes())
                    .and_then(|()| out.flush());
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
            Effect::LoadNotices => {
                let home = api.home();
                tokio::task::spawn_blocking(move || {
                    let method = devlanes_client::update::InstallMethod::current();
                    let dismissed = home
                        .map(|home| devlanes_client::update::NoticeLog::load(&home).dismissed)
                        .unwrap_or_default();
                    deliver_blocking(&sender, Msg::Notices { method, dismissed });
                });
            }
            Effect::DismissNotice(key) => {
                if let Some(home) = api.home() {
                    tokio::task::spawn_blocking(move || {
                        let mut log = devlanes_client::update::NoticeLog::load(&home);
                        log.dismiss(&key);
                        // Losing a dismissal only shows the notice again.
                        let _ = log.save(&home);
                    });
                }
            }
            Effect::ReadDaemonLog { from } => {
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
                    let result = crate::logfile::read(&path, from)
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

    /// Opens a new terminal window through the configured or default launcher.
    fn open_terminal(&mut self, cwd: PathBuf, attach: Option<String>, title: String) {
        let sender = self.sender.clone();
        let options = self.options.clone();
        let home = self.api.home();
        tokio::task::spawn_blocking(move || {
            let failed = |message: String| {
                let msg = Msg::Failed {
                    what: "open terminal".into(),
                    message,
                };
                deliver_blocking(&sender, msg);
            };
            let launcher = match options.pick_launcher() {
                Ok(launcher) => launcher,
                Err(message) => {
                    failed(message);
                    return;
                }
            };
            // The same binary attaches, so client and daemon versions match.
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("devlanes"));
            let shell = launch_env::default_shell();
            let request = LaunchRequest {
                cwd,
                command: terminal_command(&exe, &shell, home.as_deref(), attach.as_deref()),
                title: Some(title),
                env: terminal_env(home.as_deref()),
            };
            if let Err(error) = launcher.launch(&request) {
                failed(error.to_string());
            }
        });
    }
}

/// The command a new terminal runs: `devlanes attach <unit>`, or the user's
/// shell for a lane. `DEVLANES_HOME` travels explicitly so the command reaches
/// the same daemon: as an `env` prefix on macOS and Linux, through
/// [`terminal_env`] on Windows.
pub fn terminal_command(
    exe: &Path,
    shell: &str,
    home: Option<&Path>,
    attach: Option<&str>,
) -> Vec<String> {
    let mut command = home.map(launch_env::home_prefix).unwrap_or_default();
    match attach {
        Some(unit) => command.extend([exe.display().to_string(), "attach".into(), unit.into()]),
        None => command.push(shell.to_owned()),
    }
    command
}

/// The variables the launcher sets for [`terminal_command`]: `DEVLANES_HOME`
/// on Windows, nothing on macOS and Linux, where the command carries it.
pub fn terminal_env(home: Option<&Path>) -> Vec<(String, String)> {
    home.map(launch_env::home_env).unwrap_or_default()
}

/// How `DEVLANES_HOME` and the shell reach a new terminal on macOS and Linux.
#[cfg(unix)]
mod launch_env {
    use std::path::Path;

    /// `env DEVLANES_HOME=<home>` before the command. Terminals started through
    /// `open` or `osascript` inherit launchd's environment, not this one.
    pub(super) fn home_prefix(home: &Path) -> Vec<String> {
        vec![
            "env".to_owned(),
            format!("DEVLANES_HOME={}", home.display()),
        ]
    }

    /// Nothing: [`home_prefix`] carries the home.
    pub(super) fn home_env(_home: &Path) -> Vec<(String, String)> {
        Vec::new()
    }

    /// `$SHELL`, else `/bin/sh`.
    pub(super) fn default_shell() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

/// How `DEVLANES_HOME` and the shell reach a new terminal on Windows.
#[cfg(windows)]
mod launch_env {
    use std::path::Path;

    /// Nothing: Windows has no `env` program, so [`home_env`] carries the home.
    pub(super) fn home_prefix(_home: &Path) -> Vec<String> {
        Vec::new()
    }

    /// `DEVLANES_HOME` for the launcher to set on its helper. Windows Terminal
    /// and WezTerm hand their caller's environment to the command they run.
    pub(super) fn home_env(home: &Path) -> Vec<(String, String)> {
        vec![(
            "DEVLANES_HOME".to_owned(),
            home.to_string_lossy().into_owned(),
        )]
    }

    /// `%ComSpec%` (usually `cmd.exe`); the lookup ignores case like every
    /// Windows variable. `SHELL` is skipped: Git Bash sets it to an MSYS path
    /// such as `/usr/bin/bash` that a new Windows Terminal tab cannot run.
    pub(super) fn default_shell() -> String {
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into())
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
#[cfg(unix)]
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

/// Hands a URL to the default browser through the shell, never `cmd /c start`,
/// which would parse `&` in the URL as a command separator.
#[cfg(windows)]
fn open_url(url: &str) -> io::Result<()> {
    devlanes_sys::windows::shell::open_url(url)
}
