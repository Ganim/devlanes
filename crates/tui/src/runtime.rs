//! The event loop and the executor that turns effects into daemon calls.
//!
//! Every failure becomes a [`Msg`] for the app, which reports it through one
//! path. A failed channel send only happens after the loop quit, when nobody
//! is left to show the result.

use std::{
    io::{self, Write},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event as TermEvent, KeyEventKind,
    },
    execute,
    terminal::{
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use devlanes_proto::{AttachParams, Method, Selection};
use devlanes_term::{Notifications, Notifier, Progress, ProgressBar, TitleGuard, Wrap};
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
/// The frame while the terminal is not focused: nobody watches it closely.
const UNFOCUSED_FRAME: Duration = Duration::from_millis(250);
/// Synchronized output (DEC mode 2026): the terminal holds what follows
/// until the end, so a frame never shows half drawn.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const MESSAGE_QUEUE: usize = 1024;
/// One spinner frame.
const SPIN: Duration = Duration::from_millis(150);
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
    /// Enables raw mode, the alternate screen, mouse capture, bracketed paste
    /// and focus reports (`CSI ? 1004 h`).
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
            EnableBracketedPaste,
            EnableFocusChange
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
    // A panic can strike between a frame's synchronized-output start and its
    // end; ending it again is harmless and keeps the terminal from holding
    // everything that follows.
    let screen = execute!(
        io::stdout(),
        EndSynchronizedUpdate,
        DisableFocusChange,
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
    app.theme = options.theme();
    app.on_quit = crate::app::OnQuit::parse(options.on_quit.as_deref());
    app.start_default = options.start_default;
    app.time_zone = options
        .time_zone
        .clone()
        .unwrap_or_else(jiff::tz::TimeZone::system);
    app.service_report = options.service.clone();
    #[cfg(unix)]
    let user_home = options.var("HOME").map(std::path::PathBuf::from);
    #[cfg(windows)]
    let user_home = crate::home::user_home(options);
    app.user_home = user_home;
    if let Some(cwd) = options.cwd.clone().or_else(|| std::env::current_dir().ok()) {
        app.prefer_path(cwd);
    }
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
    let mut spin = interval(SPIN);
    spin.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut last_draw = Instant::now() - FRAME;
    executor.show_title(&app.tab_title());
    loop {
        // Unfocused, the screen is drawn a few times a second, not sixty.
        let frame_time = if app.focused { FRAME } else { UNFOCUSED_FRAME };
        if dirty && last_draw.elapsed() >= frame_time {
            executor.sync(SYNC_BEGIN);
            let drawn = terminal
                .draw(|frame| ui::render(&app, frame))
                .map(|frame| executor.relink(frame.buffer, &app));
            executor.sync(SYNC_END);
            drawn.map_err(backend_error)?;
            dirty = false;
            last_draw = Instant::now();
        }
        let next_frame = last_draw + frame_time;
        let msg = tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => terminal_msg(event),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(app),
            },
            msg = receiver.recv() => msg,
            frame = next_start(&mut start_stream) => Some(Msg::Start(frame)),
            _ = tick.tick() => Some(Msg::Tick),
            // Spinners advance between ticks only while one is on screen.
            _ = spin.tick(), if app.spinning() && app.focused => Some(Msg::Spin),
            _ = sleep_until(next_frame), if dirty => None,
            // The panic hook already restored the terminal; a UI drawing over
            // the shell afterwards would only make things worse.
            () = panicked.notified() => return Err(Error::Panicked),
        };
        let Some(msg) = msg else {
            continue;
        };
        dirty = true;
        // The bar is written again every second and after anything the user
        // does (switching lanes, pages, resizing), so a terminal that dropped
        // it gets it back at once.
        let resend = matches!(
            msg,
            Msg::Tick | Msg::Key(_) | Msg::Mouse(_) | Msg::Resize(..)
        );
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
        executor.show_progress(app.start_progress(), resend);
        executor.show_title(&app.tab_title());
    }
}

/// Converts backend errors into the crate error.
fn backend_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> Error {
    Error::Io(io::Error::other(error))
}

/// Maps crossterm events; key releases are dropped (kitty protocol reports them).
fn terminal_msg(event: TermEvent) -> Option<Msg> {
    match event {
        #[cfg(unix)]
        TermEvent::Key(key) if key.kind != KeyEventKind::Release => Some(Msg::Key(key)),
        #[cfg(windows)]
        TermEvent::Key(key) if key.kind != KeyEventKind::Release => Some(Msg::Key(altgr(key))),
        TermEvent::Mouse(mouse) => Some(Msg::Mouse(mouse)),
        TermEvent::Paste(text) => Some(Msg::Paste(text)),
        TermEvent::Resize(width, height) => Some(Msg::Resize(width, height)),
        TermEvent::FocusGained => Some(Msg::Focus(true)),
        TermEvent::FocusLost => Some(Msg::Focus(false)),
        _ => None,
    }
}

/// AltGr reaches Windows console programs as Ctrl+Alt with the character it
/// typed (`@` is AltGr+Q on a German layout), which the app would take for a
/// Ctrl chord: text fields would drop it and units would get a control byte.
/// crossterm reports a real Ctrl+Alt chord with the key's unshifted
/// character, and the chords programs bind are on letters and digits, so any
/// other character is taken as typed text. Unix terminals resolve AltGr
/// themselves.
#[cfg(windows)]
fn altgr(mut key: crossterm::event::KeyEvent) -> crossterm::event::KeyEvent {
    use crossterm::event::{KeyCode, KeyModifiers};
    let chord = KeyModifiers::CONTROL | KeyModifiers::ALT;
    if let KeyCode::Char(c) = key.code
        && key.modifiers.contains(chord)
        && !c.is_ascii_alphanumeric()
        && !c.is_control()
        && c != ' '
    {
        key.modifiers.remove(chord);
    }
    key
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
    /// The terminal output for OSC sequences: clipboard copies, the tab's
    /// progress bar and its title. When the executor drops the bar clears
    /// first, then the title the TUI found is put back.
    out: ProgressBar<TitleGuard<Box<dyn Write + Send>>>,
    notifications: Option<Notifications>,
    notifier: Notifier,
    links: Option<devlanes_term::Links>,
    /// Frames are wrapped in synchronized output.
    sync_output: bool,
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
    /// Writes OSC sequences to `clipboard` with the configured progress bar.
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
            out: ProgressBar::new(TitleGuard::new(clipboard, options.title), options.progress),
            notifications: options.notifications,
            notifier: Notifier::default(),
            links: options.links,
            sync_output: options.sync_output,
            in_tmux: options.var("TMUX").is_some(),
            subscription: None,
            attach: None,
            metrics: None,
            daemon: None,
            log: None,
            local: Vec::new(),
        }
    }

    /// Writes the tab's progress bar when it changed, and on `resend` while it
    /// shows: Ghostty drops a bar that is not repeated.
    fn show_progress(&mut self, progress: Progress, resend: bool) {
        let shown = self.out.shown().unwrap_or(Progress::Clear);
        if shown != progress || resend && shown != Progress::Clear {
            // The bar is decoration; a failed write is not worth a notice.
            let _ = self.out.set(progress);
        }
    }

    /// Sets the tab title; written only when the text changed.
    fn show_title(&mut self, title: &str) {
        // The title is decoration; a failed write is not worth a notice.
        let _ = self.out.get_mut().set(title);
    }

    /// Makes the service URLs of the frame just drawn clickable, where the
    /// terminal shows links.
    fn relink(&mut self, buffer: &ratatui::buffer::Buffer, app: &App) {
        let Some(links) = self.links else {
            return;
        };
        let bytes = crate::hyperlink::relink(buffer, &app.link_targets(), &links);
        if !bytes.is_empty() {
            let out = self.out.get_mut();
            let _ = out.write_all(bytes.as_bytes()).and_then(|()| out.flush());
        }
    }

    /// Writes a synchronized-output mark around a frame, when enabled.
    fn sync(&mut self, mark: &[u8]) {
        if self.sync_output {
            let out = self.out.get_mut();
            let _ = out.write_all(mark).and_then(|()| out.flush());
        }
    }

    /// Shows a desktop notification where they are on and the last one is
    /// not too recent.
    fn notify(&mut self, title: &str, body: &str) {
        let Some(notifications) = self.notifications else {
            return;
        };
        if !self.notifier.allow(std::time::Instant::now()) {
            return;
        }
        let out = self.out.get_mut();
        let sequence = notifications.sequence(title, body);
        let _ = out
            .write_all(sequence.as_bytes())
            .and_then(|()| out.flush());
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
            Effect::Notify { title, body } => self.notify(&title, &body),
            Effect::QuitWith { stop, remember } => {
                let home = api.home();
                tokio::spawn(async move {
                    let mut errors = Vec::new();
                    if let Some(choice) = remember {
                        let written = match home {
                            Some(home) => tokio::task::spawn_blocking(move || {
                                devlanes_config::set_user_value(
                                    &home.join("config.yaml"),
                                    "tui.on_quit",
                                    &serde_json::Value::from(choice.as_str()),
                                )
                                .map_err(|error| error.message)
                            })
                            .await
                            .unwrap_or_else(|error| Err(error.to_string())),
                            None => Err("no devlanes home to write config.yaml in".into()),
                        };
                        if let Err(error) = written {
                            errors.push(format!("could not remember the choice: {error}"));
                        }
                    }
                    // The lanes stop together; each answers when it stopped.
                    let stops = stop.into_iter().map(|lane| {
                        let api = Arc::clone(&api);
                        async move {
                            api.control(Method::Stop, lane.clone(), Selection::default())
                                .await
                                .map_err(|error| format!("stop {lane} failed: {error}"))
                        }
                    });
                    for result in futures_util::future::join_all(stops).await {
                        if let Err(error) = result {
                            errors.push(error);
                        }
                    }
                    let error = (!errors.is_empty()).then(|| errors.join("; "));
                    deliver(&sender, Msg::QuitReady { error }).await;
                });
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
            Effect::FollowContainer {
                generation,
                lane,
                container,
                tail,
            } => {
                self.detach();
                // Held in the attach slot so any later attach or detach ends it;
                // nothing is ever sent on the input side.
                let (input_tx, _) = mpsc::channel(1);
                let task = tokio::spawn(pump_container(
                    api, sender, generation, lane, container, tail,
                ));
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
            Effect::Run { lane, params, yes } => {
                tokio::spawn(async move {
                    let task = params.task.clone();
                    let result = api.run_task(lane.clone(), params, yes).await;
                    deliver(&sender, Msg::RunDone { lane, task, result }).await;
                });
            }
            Effect::FetchTasks { lane } => {
                tokio::spawn(async move {
                    let result = api.task_list(lane.clone()).await;
                    deliver(&sender, Msg::Tasks { lane, result }).await;
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
            Effect::FetchMetrics(query) => {
                // A slow daemon must not pile up one request per tick.
                if self
                    .metrics
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    return true;
                }
                self.metrics = Some(tokio::spawn(async move {
                    deliver(&sender, Msg::Metrics(api.metrics(query).await)).await;
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
                tokio::task::spawn_blocking(move || {
                    let (method, dismissed) = api.notices();
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
            Effect::Settings(request) => crate::settings::perform(request, api.home(), sender),
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
                    Some(AttachEvent::ResizeRefused(error)) => Msg::ResizeRefused { generation, error },
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

/// Follows a container's Docker logs and writes each line to the pane's grid
/// as a unit's output would arrive, so scrollback and copy work the same.
async fn pump_container(
    api: Arc<dyn DaemonApi>,
    sender: mpsc::Sender<Msg>,
    generation: u64,
    lane: String,
    container: String,
    tail: usize,
) {
    let mut stream = match api.follow_container(lane, container, tail).await {
        Ok(stream) => stream,
        Err(error) => {
            let msg = Msg::ContainerLogsOpened {
                generation,
                result: Err(error),
            };
            deliver(&sender, msg).await;
            return;
        }
    };
    let opened = Msg::ContainerLogsOpened {
        generation,
        result: Ok(()),
    };
    if !deliver(&sender, opened).await {
        return;
    }
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(event) => match container_line(&event) {
                Some(bytes) => Msg::AttachOutput { generation, bytes },
                None => continue,
            },
            Err(error) => {
                deliver(&sender, Msg::StreamError { generation, error }).await;
                break;
            }
        };
        if !deliver(&sender, msg).await {
            return;
        }
    }
    deliver(&sender, Msg::AttachClosed { generation }).await;
}

/// The bytes a `log` event adds to the grid: the line with its colors when
/// the daemon kept them, then CR LF, since the grid is a terminal.
fn container_line(event: &devlanes_proto::Event) -> Option<Vec<u8>> {
    if event.event != devlanes_proto::events::LOG {
        return None;
    }
    let line = event.data.get("line")?;
    let text = line
        .get("styled")
        .and_then(serde_json::Value::as_str)
        .or_else(|| line.get("text").and_then(serde_json::Value::as_str))?;
    let mut bytes = text.as_bytes().to_vec();
    bytes.extend_from_slice(b"\r\n");
    Some(bytes)
}

/// Hands a URL to the desktop's opener, or inside WSL to the Windows browser.
#[cfg(unix)]
fn open_url(url: &str) -> io::Result<()> {
    let opener = devlanes_sys::url_opener(url);
    let status = std::process::Command::new(&opener.program)
        .args(&opener.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            opener.program.display()
        )))
    }
}

/// Hands a URL to the shell, which opens only plain `http` and `https` URLs
/// with the default browser.
#[cfg(windows)]
fn open_url(url: &str) -> io::Result<()> {
    devlanes_sys_windows::shell_execute::open_url(url)
}

#[cfg(all(test, windows))]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn key(c: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), modifiers)
    }

    #[test]
    fn chords_on_letters_and_digits_stay_chords() {
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        for event in [
            key('c', KeyModifiers::CONTROL),
            key('q', ctrl_alt),
            key('Q', ctrl_alt | KeyModifiers::SHIFT),
            key('1', ctrl_alt),
            key(' ', ctrl_alt),
            key('!', KeyModifiers::SHIFT),
            key('@', KeyModifiers::CONTROL),
            key('@', KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Left, ctrl_alt),
        ] {
            assert_eq!(altgr(event), event);
        }
    }

    #[test]
    fn altgr_characters_are_typed_text() {
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(altgr(key('@', ctrl_alt)), key('@', KeyModifiers::NONE));
        assert_eq!(altgr(key('{', ctrl_alt)), key('{', KeyModifiers::NONE));
        assert_eq!(
            altgr(key('€', ctrl_alt | KeyModifiers::SHIFT)),
            key('€', KeyModifiers::SHIFT)
        );
        // As text, the unit gets the character's UTF-8 bytes, not a control byte.
        let typed = terminal_msg(TermEvent::Key(key('@', ctrl_alt)));
        assert!(matches!(typed, Some(Msg::Key(k)) if k == key('@', KeyModifiers::NONE)));
    }

    #[test]
    /// A container's `log` event becomes its colored line, else its plain
    /// one, ending in CR LF; other events add nothing.
    fn container_lines_end_in_crlf_and_keep_colors() {
        let event = |name: &str, line: serde_json::Value| devlanes_proto::Event {
            seq: 0,
            event: name.into(),
            data: serde_json::json!({"unit": "db", "line": line}),
        };
        let styled = event(
            devlanes_proto::events::LOG,
            serde_json::json!({"text": "ready", "styled": "\x1b[32mready\x1b[0m"}),
        );
        assert_eq!(
            container_line(&styled).as_deref(),
            Some(&b"\x1b[32mready\x1b[0m\r\n"[..])
        );
        let plain = event(
            devlanes_proto::events::LOG,
            serde_json::json!({"text": "ready"}),
        );
        assert_eq!(container_line(&plain).as_deref(), Some(&b"ready\r\n"[..]));
        let other = event(devlanes_proto::events::DIAGNOSTIC, serde_json::json!({}));
        assert_eq!(container_line(&other), None);
    }
}
