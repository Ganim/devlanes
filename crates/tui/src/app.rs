//! TUI state and the pure update function: messages in, effects out.
//!
//! Nothing here performs I/O, so key handling, event application and resync
//! behavior are tested without a terminal or a daemon.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use devlanes_proto::{
    ActorKind, AttachResult, Event, LaneSummary, LogLine, LogQuery, Method, ORPHANED, Page,
    RpcError, RunParams, Selection, UnitSummary, events,
};
use devlanes_term::{Progress, StartTracker};

pub use crate::start::{StartExit, StartFrame, StartRun};
use ratatui::layout::Rect;
use serde_json::Value;

use crate::api::{ApiError, UnitMetrics};
use crate::input::{self, InputModes};
use crate::screen::TerminalScreen;
use crate::text;
use crate::words::{self, Worded};

/// Smallest terminal that still fits a list, a pane and the help bar.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 8;
/// Notices stay this many one-second ticks; errors stay longer.
const NOTICE_TICKS: u8 = 6;
const ERROR_TICKS: u8 = 12;
/// Errors kept for the error list (`e`).
const MAX_ERRORS: usize = 50;
/// Lines requested per deep-scrollback page.
const LOG_PAGE: usize = 500;
const WHEEL_LINES: isize = 3;
const MAX_BACKOFF_MS: u64 = 5000;

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Terminal,
}

/// Top-level views, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Lanes,
    Monitor,
    Daemon,
}

/// Lines of the daemon log kept client-side.
const MAX_DAEMON_LOG: usize = 2000;
/// Daemon facts refresh every this many one-second ticks while the tab is visible.
const DAEMON_REFRESH_TICKS: u8 = 3;

/// The daemon tab: facts, doctor data, the log tail and in-flight actions.
#[derive(Debug, Default)]
pub struct DaemonView {
    /// `daemon.status` (version, pid, uptime, socket, managed processes).
    pub status: Option<Value>,
    /// `doctor` facts; the UI words them as checks.
    pub doctor: Option<Value>,
    pub log: VecDeque<String>,
    pub log_position: crate::logfile::LogPosition,
    /// Why the log cannot be read, shown in place of the tail.
    pub log_error: Option<String>,
    /// Log lines scrolled up from the newest; 0 follows the file.
    pub scroll: usize,
    /// The restart, stop, start or gc in flight.
    pub busy: Option<&'static str>,
    /// Set after this TUI stopped the daemon, until one is reachable again.
    pub stopped: bool,
    /// Facts and log failures repeat every refresh; only a change is reported.
    last_error: Option<String>,
    ticks: u8,
}

impl DaemonView {
    /// Managed processes from the last status, when known.
    pub fn managed_processes(&self) -> Option<u64> {
        self.status
            .as_ref()?
            .get("managed_processes")
            .and_then(Value::as_u64)
    }
}

/// Daemon connection as seen by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Connecting,
    Online,
    Reconnecting { attempt: u32 },
}

/// The selected row, kept by identity so live updates do not move the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selected {
    Lane(String),
    Unit { lane: String, unit: String },
}

/// One visible row of the lane tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Project(usize),
    Lane(usize),
    Unit(usize, usize),
}

/// The header message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// One cleaned line; the header clips it to its width.
    pub text: String,
    pub error: bool,
}

/// The lane or unit an error belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Lane(String),
    Unit { lane: String, unit: String },
}

/// One reported failure, kept in the error list and on its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEntry {
    /// The operation, such as `start acme@1:portal`.
    pub what: String,
    pub target: Option<Target>,
    pub worded: Worded,
    /// The target's daemon state when the error arrived; the row badge stays
    /// until the target becomes healthy from a different state.
    pub state: Option<String>,
}

impl ErrorEntry {
    /// The operation's verb for row badges (`start failed`).
    pub fn verb(&self) -> &str {
        self.what.split(' ').next().unwrap_or("action")
    }
}

/// One line of the error list, styled by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelLine {
    Title(String),
    Summary(String),
    Detail(String),
    Hint(String),
    Blank,
}

/// Modal prompts; while one is open it receives every key.
#[derive(Debug)]
pub enum Dialog {
    /// Recent errors with full text; `scroll` is the first visible line.
    Errors {
        scroll: usize,
    },
    /// `y` confirms; `Enter` too unless `strict` (anything that stops or removes).
    Confirm {
        prompt: String,
        /// What will happen, one sentence per entry.
        detail: Vec<String>,
        /// The verb for the key hint (`y restart`).
        action: &'static str,
        strict: bool,
        effect: Box<Effect>,
    },
    /// Destructive actions on data: the user types `expected` and presses Enter.
    TypeConfirm {
        prompt: String,
        detail: Vec<String>,
        expected: String,
        input: String,
        /// Enter was pressed with the wrong text.
        mismatch: bool,
        effect: Box<Effect>,
    },
    Tasks {
        lane: String,
        tasks: Vec<String>,
        index: usize,
    },
    /// Profiles to start the lane with; space toggles, enter starts.
    Profiles {
        lane: String,
        state: ProfilesState,
    },
    Help,
}

/// The profile picker while `profile.list` loads and after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfilesState {
    Loading,
    /// The daemon does not know `profile.list`.
    Unsupported,
    Ready {
        items: Vec<ProfileChoice>,
        index: usize,
    },
}

/// One row of the profile picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChoice {
    pub profile: crate::api::ProfileInfo,
    pub checked: bool,
}

/// What the terminal pane shows for an attached unit.
#[derive(Debug)]
pub enum PaneMode {
    Live,
    Logs(LogView),
    Copy(CopyState),
}

/// Deep scrollback served from the daemon's log store.
#[derive(Debug, Default)]
pub struct LogView {
    pub lines: Vec<LogLine>,
    /// Lines scrolled up from the newest line.
    pub offset: usize,
    pub older: Option<String>,
    pub loading: bool,
}

/// Keyboard selection over the visible grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyState {
    pub cursor: (u16, u16),
    pub anchor: Option<(u16, u16)>,
}

/// Attach stream lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachState {
    Opening,
    Live,
    Closed,
    Failed(Worded),
}

/// The one unit parsed client-side.
pub struct Attached {
    pub generation: u64,
    pub lane: String,
    pub unit: String,
    pub state: AttachState,
    pub screen: TerminalScreen,
    pub mode: PaneMode,
    /// Interactive streams own the unit's PTY size. Browsing attaches as an
    /// observer so moving through the list never resizes anyone's terminal.
    pub interactive: bool,
}

/// One row of the monitor table.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorRow {
    /// Sum of the lane's units that report numbers.
    Lane {
        lane: String,
        cpu_percent: Option<f64>,
        memory_bytes: Option<u64>,
    },
    Unit(UnitMetrics),
}

/// The PTY is never made smaller than this, whatever the pane size.
pub const MIN_PTY: (u16, u16) = (20, 5);
/// A resize settles this long before the stream is reopened for a fresh screen.
const RESIZE_SETTLE_MS: u64 = 200;

/// Inputs to [`App::update`].
#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    /// A frame of the start this TUI was opened for (`devlanes start`).
    Start(StartFrame),
    /// Outer terminal size.
    Resize(u16, u16),
    /// One second passed.
    Tick,
    Status(Result<Vec<LaneSummary>, ApiError>),
    Subscribed(Result<(), ApiError>),
    Event(Event),
    /// The subscription ended, with the reason when there is one.
    EventsEnded(Option<ApiError>),
    Reconnect,
    AttachOpened {
        generation: u64,
        result: Result<AttachResult, ApiError>,
    },
    AttachOutput {
        generation: u64,
        bytes: Vec<u8>,
    },
    AttachClosed {
        generation: u64,
    },
    /// The pause after the last resize is over.
    ResizeSettled {
        token: u64,
    },
    /// Input, resize or the stream itself failed on an open attach.
    StreamError {
        generation: u64,
        error: ApiError,
    },
    ActionDone {
        label: String,
        target: Target,
        result: Result<Value, ApiError>,
    },
    RunDone {
        lane: String,
        task: String,
        result: Result<Value, ApiError>,
    },
    Logs {
        generation: u64,
        older: bool,
        result: Result<Page<LogLine>, ApiError>,
    },
    Metrics(Result<Vec<UnitMetrics>, ApiError>),
    /// A local helper (URL opener, terminal launcher, clipboard, input queue) failed.
    Failed {
        what: String,
        message: String,
    },
    CleanDone {
        lane: String,
        result: Result<Value, ApiError>,
    },
    GcDone(Result<Value, ApiError>),
    DaemonInfo {
        status: Result<Value, ApiError>,
        doctor: Result<Value, ApiError>,
    },
    /// New lines of the daemon's log file, or why it cannot be read.
    DaemonLog(Result<crate::logfile::LogChunk, String>),
    DaemonStopped(Result<Value, ApiError>),
    /// The new daemon's status after a restart.
    DaemonRestarted(Result<Value, ApiError>),
    DaemonStarted(Result<Value, ApiError>),
    /// How devlanes was installed, and the release notices dismissed earlier.
    Notices {
        method: devlanes_client::update::InstallMethod,
        dismissed: Vec<String>,
    },
    Profiles {
        lane: String,
        result: Result<crate::api::ProfileList, ApiError>,
    },
}

/// Side effects the runtime performs; results come back as [`Msg`].
#[derive(Debug)]
pub enum Effect {
    Refresh,
    Subscribe,
    ScheduleReconnect {
        after_ms: u64,
    },
    Attach {
        generation: u64,
        lane: String,
        unit: String,
        columns: u16,
        rows: u16,
        interactive: bool,
    },
    /// Sends [`Msg::ResizeSettled`] with `token` after a short pause.
    SettleResize {
        token: u64,
        after_ms: u64,
    },
    Detach,
    Input(Vec<u8>),
    ResizePty {
        columns: u16,
        rows: u16,
    },
    Control {
        method: Method,
        lane: String,
        selection: Selection,
        label: String,
        target: Target,
    },
    Run {
        lane: String,
        params: RunParams,
    },
    FetchLogs {
        generation: u64,
        lane: String,
        query: LogQuery,
        older: bool,
    },
    FetchMetrics,
    OpenUrl(String),
    /// `attach` is a unit ref to run `devlanes attach` on; `None` opens a shell.
    OpenTerminal {
        cwd: PathBuf,
        attach: Option<String>,
        title: String,
    },
    Copy(String),
    /// Sends `yes`; only produced by a confirmed dialog.
    CleanLane {
        lane: String,
    },
    /// Sends `yes`; only produced by a confirmed dialog.
    LaneGc,
    FetchDaemon,
    /// Finds how devlanes was installed and which release notices were dismissed.
    LoadNotices,
    /// Remembers that the release notice `key` was dismissed.
    DismissNotice(String),
    ReadDaemonLog {
        from: crate::logfile::LogPosition,
    },
    /// Sends `yes`; only produced by a confirmed dialog.
    StopDaemon,
    /// Sends `yes`; only produced by a confirmed dialog.
    RestartDaemon,
    StartDaemon,
    FetchProfiles {
        lane: String,
    },
    Quit,
}

/// Screen regions shared by rendering, mouse hit-testing and PTY sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    pub header: Rect,
    pub list: Option<Rect>,
    pub pane: Rect,
    pub pane_inner: Rect,
    pub body: Rect,
    pub footer: Rect,
}

/// Splits the screen, or `None` when it is below the minimum size.
pub fn areas(width: u16, height: u16, zoom: bool) -> Option<Areas> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }
    let header = Rect::new(0, 0, width, 1);
    let footer = Rect::new(0, height - 1, width, 1);
    let body = Rect::new(0, 1, width, height - 2);
    let (list, pane) = if zoom {
        (None, body)
    } else {
        // Wide enough for a lane row with its badges; the pane keeps the rest.
        let list_width = (width * 2 / 5).clamp(28, 48);
        (
            Some(Rect::new(0, body.y, list_width, body.height)),
            Rect::new(list_width, body.y, width - list_width, body.height),
        )
    };
    let pane_inner = Rect::new(
        pane.x + 1,
        pane.y + 1,
        pane.width.saturating_sub(2),
        pane.height.saturating_sub(2),
    );
    Some(Areas {
        header,
        list,
        pane,
        pane_inner,
        body,
        footer,
    })
}

/// All TUI state.
pub struct App {
    pub lanes: Vec<LaneSummary>,
    pub loaded: bool,
    pub selected: Option<Selected>,
    pub focus: Focus,
    pub tab: Tab,
    pub zoom: bool,
    pub connection: Connection,
    pub dialog: Option<Dialog>,
    pub attached: Option<Attached>,
    pub metrics: Vec<UnitMetrics>,
    pub monitor_index: usize,
    pub list_offset: usize,
    pub notice: Option<Notice>,
    /// Recent errors, newest first.
    pub errors: VecDeque<ErrorEntry>,
    pub daemon: DaemonView,
    pub size: (u16, u16),
    /// How devlanes was installed (`None` until known) and dismissed release notices.
    pub release: (Option<devlanes_client::update::InstallMethod>, Vec<String>),
    expanded: HashMap<String, bool>,
    /// Lanes with a clean in flight; their rows say so.
    cleaning: HashSet<String>,
    /// The last failed action per lane or unit.
    target_errors: HashMap<Target, ErrorEntry>,
    /// Actions in flight per lane; their phase events are not reported twice.
    pending: HashMap<String, usize>,
    /// Focus the terminal once the stream that is opening is live.
    want_terminal: bool,
    /// The latest resize; older settle timers are ignored.
    resize_token: u64,
    /// Metrics fail every tick when unsupported; only a change is reported.
    last_metrics_error: Option<String>,
    notice_ticks: u8,
    generation: u64,
    reconnect_pending: bool,
    /// Start progress of every lane starting now, for the tab's progress bar.
    starts: StartTracker,
    /// Ticks a failed start has been shown red.
    start_error_ticks: u8,
    /// The start this TUI was opened for (`devlanes start` on a terminal).
    pub start_run: Option<StartRun>,
}

/// Ticks (seconds) a failed start stays red in the tab's progress bar.
const START_ERROR_TICKS: u8 = 3;

impl App {
    /// Creates the state for a terminal of `width` x `height`.
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            lanes: Vec::new(),
            loaded: false,
            selected: None,
            focus: Focus::List,
            tab: Tab::Lanes,
            zoom: false,
            connection: Connection::Connecting,
            dialog: None,
            attached: None,
            metrics: Vec::new(),
            monitor_index: 0,
            list_offset: 0,
            notice: None,
            errors: VecDeque::new(),
            daemon: DaemonView::default(),
            size: (width, height),
            release: (None, Vec::new()),
            expanded: HashMap::new(),
            cleaning: HashSet::new(),
            target_errors: HashMap::new(),
            pending: HashMap::new(),
            want_terminal: false,
            resize_token: 0,
            last_metrics_error: None,
            notice_ticks: 0,
            generation: 0,
            reconnect_pending: false,
            starts: StartTracker::viewer(),
            start_error_ticks: 0,
            start_run: None,
        }
    }

    /// Follows a start sent before the TUI opened; its frames arrive as
    /// [`Msg::Start`].
    pub fn open_start(&mut self, label: String) {
        self.start_run = Some(StartRun::new(label));
    }

    /// How the start stands, for the exit status; `None` without one.
    pub fn start_exit(&self) -> Option<StartExit> {
        let run = self.start_run.as_ref()?;
        Some(match &run.outcome {
            None => StartExit::Pending,
            Some(outcome) if outcome.ok => StartExit::Ready,
            Some(_) => StartExit::Failed,
        })
    }

    /// The header line for the start: its phase while it runs, then its result.
    /// `Some((line, Some(ok)))` once it ended.
    pub fn start_line(&self) -> Option<(String, Option<bool>)> {
        let run = self.start_run.as_ref()?;
        Some(match &run.outcome {
            Some(outcome) => (outcome.line.clone(), Some(outcome.ok)),
            None => (run.running_line(&self.lanes), None),
        })
    }

    /// Applies one frame of the start: events move the phase line, the result
    /// reports failures on their units and keeps the lane in view.
    fn start_frame(&mut self, frame: StartFrame, effects: &mut Vec<Effect>) {
        let Some(run) = self.start_run.as_mut() else {
            return;
        };
        match frame {
            StartFrame::Event(event) => {
                if let Some(lane) = run.event(&event) {
                    // Its phase failure is reported once, by the result.
                    self.pending_begin(&lane);
                }
                self.focus_start(effects);
            }
            StartFrame::Done(result) => {
                let (failures, error) = run.finish(&result);
                let (label, lanes) = (run.label.clone(), run.lanes.clone());
                for lane in &lanes {
                    self.pending_end(lane);
                }
                if let Some(error) = error {
                    let what = match lanes.first() {
                        Some(lane) => format!("{label} {lane}"),
                        None => label.clone(),
                    };
                    let target = lanes.first().cloned().map(Target::Lane);
                    self.report(&what, target, words::api(&error));
                }
                let verb = if label == "restart" {
                    "restart"
                } else {
                    "start"
                };
                for failure in &failures {
                    let target = Target::Unit {
                        lane: failure.lane.clone(),
                        unit: failure.unit.clone(),
                    };
                    let what = format!("{verb} {}", unit_ref(&failure.lane, &failure.unit));
                    self.report(&what, Some(target), failure_words(failure));
                }
                // Untouched since the TUI focused the lane: show the first failure.
                if let (Some(failure), Some(lane)) = (failures.first(), lanes.first())
                    && self.selected == Some(Selected::Lane(lane.clone()))
                    && failure.lane == *lane
                {
                    self.selected = Some(Selected::Unit {
                        lane: failure.lane.clone(),
                        unit: failure.unit.clone(),
                    });
                    self.after_change(effects);
                }
                self.focus_start(effects);
                effects.push(Effect::Refresh);
            }
        }
    }

    /// Selects and expands the start's first lane once it is listed, once.
    fn focus_start(&mut self, effects: &mut Vec<Effect>) {
        let Some(run) = self.start_run.as_mut() else {
            return;
        };
        let Some(lane) = run.lanes.first().cloned() else {
            return;
        };
        if run.focused || !self.lanes.iter().any(|l| l.reference == lane) {
            return;
        }
        run.focused = true;
        self.expanded.insert(lane.clone(), true);
        self.selected = Some(Selected::Lane(lane));
        self.after_change(effects);
    }

    /// The tab's progress bar: every lane starting now, together. The selected
    /// lane alone would jump around while the user browses.
    pub fn start_progress(&self) -> Progress {
        self.starts.progress()
    }

    /// Feeds the start tracker; a failed start is forgotten after
    /// [`START_ERROR_TICKS`] so its red bar ends.
    fn track_starts(&mut self, msg: &Msg) {
        match msg {
            Msg::Status(Ok(lanes)) => self.starts.seed(lanes),
            Msg::Event(event) | Msg::Start(StartFrame::Event(event)) => self.starts.event(event),
            Msg::Tick if matches!(self.starts.progress(), Progress::Error(_)) => {
                self.start_error_ticks += 1;
                if self.start_error_ticks >= START_ERROR_TICKS {
                    self.starts.forget_failures();
                    self.start_error_ticks = 0;
                }
            }
            _ => {}
        }
    }

    /// Subscribes first; status is queried once the subscription is open, so no
    /// change between the two is missed.
    pub fn start(&self) -> Vec<Effect> {
        vec![Effect::Subscribe]
    }

    /// Applies one message.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.track_starts(&msg);
        match msg {
            Msg::Key(key) => self.key(key, &mut effects),
            Msg::Mouse(mouse) => self.mouse(mouse, &mut effects),
            Msg::Paste(text) => self.paste(&text, &mut effects),
            Msg::Resize(width, height) => {
                self.size = (width, height);
                self.resize_pane(&mut effects);
            }
            Msg::Tick => self.tick(&mut effects),
            Msg::Status(Ok(lanes)) => {
                self.connection = Connection::Online;
                self.loaded = true;
                self.lanes = lanes;
                sort_lanes(&mut self.lanes);
                self.after_change(&mut effects);
                self.focus_start(&mut effects);
            }
            Msg::Status(Err(error)) => self.api_error("status", None, error, &mut effects),
            Msg::Start(frame) => self.start_frame(frame, &mut effects),
            Msg::Subscribed(Ok(())) => {
                let reconnected = matches!(self.connection, Connection::Reconnecting { .. });
                self.connection = Connection::Online;
                // Someone may have started the daemon this TUI stopped.
                self.daemon.stopped = false;
                effects.push(Effect::Refresh);
                if reconnected {
                    self.reattach_if_closed(&mut effects);
                }
            }
            Msg::Subscribed(Err(error)) => {
                if !error.is_disconnect() {
                    self.report("subscribe", None, words::api(&error));
                    // A daemon that refuses subscriptions can still answer status.
                    effects.push(Effect::Refresh);
                }
                self.lost_connection(Some(&error), &mut effects);
            }
            Msg::EventsEnded(reason) => self.lost_connection(reason.as_ref(), &mut effects),
            Msg::Reconnect => {
                self.reconnect_pending = false;
                effects.push(Effect::Subscribe);
            }
            Msg::Event(event) => self.event(event, &mut effects),
            Msg::AttachOpened { generation, result } => {
                let size = self.attach_size();
                let Some(attached) = self.attached_mut(generation) else {
                    return effects;
                };
                match result {
                    Ok(_) => {
                        attached.state = AttachState::Live;
                        // Another viewer may have resized the PTY since the attach was sent.
                        if attached.interactive {
                            effects.push(Effect::ResizePty {
                                columns: size.0,
                                rows: size.1,
                            });
                        }
                        if std::mem::take(&mut self.want_terminal) {
                            self.focus = Focus::Terminal;
                        }
                    }
                    Err(error) => {
                        let worded = words::api(&error);
                        let what = format!("attach {}", unit_ref(&attached.lane, &attached.unit));
                        attached.state = AttachState::Failed(worded.clone());
                        if self.focus == Focus::Terminal || std::mem::take(&mut self.want_terminal)
                        {
                            self.focus = Focus::List;
                            self.set_notice(format!("no terminal: {}", worded.summary));
                        }
                        // A unit that is not running simply has no terminal; the pane says so.
                        if worded.code.as_deref() != Some("not_running") {
                            self.api_error(&what, None, error, &mut effects);
                        }
                    }
                }
            }
            Msg::StreamError { generation, error } => {
                // A lost connection is reported once by the subscription, and an
                // intentional daemon stop or restart is not an error at all.
                let expected =
                    error.is_disconnect() || self.daemon.stopped || self.daemon.busy.is_some();
                let Some(attached) = self.attached_mut(generation) else {
                    return effects;
                };
                let worded = words::api(&error);
                // Failed, not Closed: the close that follows must not reopen at once,
                // or a stream that keeps failing would reopen in a loop.
                attached.state = AttachState::Failed(worded.clone());
                if !expected {
                    let what = format!("terminal {}", unit_ref(&attached.lane, &attached.unit));
                    self.report(&what, None, worded);
                }
            }
            Msg::AttachOutput { generation, bytes } => {
                if let Some(attached) = self.attached_mut(generation) {
                    attached.screen.process(&bytes);
                }
            }
            Msg::AttachClosed { generation } => {
                let Some(attached) = self.attached_mut(generation) else {
                    return effects;
                };
                let clean = !matches!(attached.state, AttachState::Failed(_));
                if clean {
                    attached.state = AttachState::Closed;
                }
                // Focus stays on the dead terminal so typed keys are dropped there
                // instead of landing on list shortcuts such as `x` or `r`.
                if self.focus == Focus::Terminal {
                    self.set_notice("the terminal ended; shift+tab returns to the list".into());
                }
                // A clean end of a unit still listed as running means it restarted:
                // its new PTY is already there. Otherwise the next unit.changed, or
                // the reconnect, reopens it.
                let online = self.connection == Connection::Online;
                if clean
                    && online
                    && self
                        .attached_unit()
                        .is_some_and(|unit| is_active(&unit.state))
                {
                    self.reattach_if_closed(&mut effects);
                }
            }
            Msg::ResizeSettled { token } => {
                if token == self.resize_token {
                    self.reopen_attach(&mut effects);
                }
            }
            Msg::ActionDone {
                label,
                target,
                result,
            } => {
                self.pending_end(target_lane(&target));
                match result {
                    Ok(_) => {
                        self.target_errors.remove(&target);
                        self.set_notice(format!("{label}: ok"));
                    }
                    Err(error) => self.api_error(&label, Some(target), error, &mut effects),
                }
            }
            Msg::RunDone { lane, task, result } => self.run_done(lane, task, result, &mut effects),
            Msg::Logs {
                generation,
                older,
                result,
            } => self.logs(generation, older, result, &mut effects),
            Msg::Metrics(Ok(metrics)) => {
                self.last_metrics_error = None;
                self.metrics = metrics;
                self.clamp_monitor();
            }
            Msg::Metrics(Err(error)) => {
                // The table falls back to per-unit numbers from status.
                self.metrics.clear();
                if error.is_disconnect() {
                    self.lost_connection(Some(&error), &mut effects);
                } else {
                    let worded = words::api(&error);
                    let line = worded.one_line();
                    if self.last_metrics_error.as_ref() != Some(&line) {
                        self.last_metrics_error = Some(line);
                        self.report("metrics", None, worded);
                    }
                }
            }
            Msg::Failed { what, message } => self.report(&what, None, Worded::local(&message)),
            Msg::CleanDone { lane, result } => {
                self.cleaning.remove(&lane);
                self.pending_end(&lane);
                let target = Target::Lane(lane.clone());
                match result {
                    Ok(value) => {
                        self.target_errors.remove(&target);
                        let stopped = value
                            .get("stopped")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        self.set_notice(match stopped {
                            0 => format!("clean {lane}: done"),
                            1 => format!("clean {lane}: done, stopped 1 unit"),
                            n => format!("clean {lane}: done, stopped {n} units"),
                        });
                        // The daemon removes and re-lists the lane; fetch the result.
                        effects.push(Effect::Refresh);
                    }
                    Err(error) => {
                        self.api_error(&format!("clean {lane}"), Some(target), error, &mut effects);
                    }
                }
            }
            Msg::GcDone(result) => {
                self.daemon.busy = None;
                match result {
                    Ok(value) => {
                        let cleaned: Vec<String> = value
                            .get("cleaned")
                            .and_then(Value::as_array)
                            .map(|list| {
                                list.iter()
                                    .filter_map(Value::as_str)
                                    .map(text::clean)
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.set_notice(if cleaned.is_empty() {
                            "lane gc: nothing to clean".into()
                        } else {
                            format!("lane gc: cleaned {}", cleaned.join(", "))
                        });
                        effects.push(Effect::Refresh);
                    }
                    Err(error) => self.api_error("lane gc", None, error, &mut effects),
                }
            }
            Msg::Notices { method, dismissed } => self.release = (Some(method), dismissed),
            Msg::DaemonInfo { status, doctor } => {
                let mut failure = None;
                match status {
                    Ok(status) => self.daemon.status = Some(status),
                    Err(error) => failure = Some(("daemon status", error)),
                }
                match doctor {
                    Ok(doctor) => self.daemon.doctor = Some(doctor),
                    Err(error) => failure = failure.or(Some(("doctor", error))),
                }
                match failure {
                    Some((_, error)) if error.is_disconnect() => {
                        self.lost_connection(Some(&error), &mut effects);
                    }
                    Some((what, error)) => self.report_daemon_once(what, words::api(&error)),
                    None => self.daemon.last_error = None,
                }
            }
            Msg::DaemonLog(Ok(chunk)) => {
                self.daemon.log_error = None;
                let added = chunk.lines.len();
                if chunk.reset {
                    self.daemon.log.clear();
                    self.daemon.scroll = 0;
                } else if self.daemon.scroll > 0 {
                    // A paused view keeps showing the same lines while new ones arrive.
                    self.daemon.scroll += added;
                }
                self.daemon
                    .log
                    .extend(chunk.lines.iter().map(|line| text::clean(line)));
                while self.daemon.log.len() > MAX_DAEMON_LOG {
                    self.daemon.log.pop_front();
                }
                self.daemon.scroll = self.daemon.scroll.min(self.daemon.log.len());
                self.daemon.log_position = chunk.position;
            }
            Msg::DaemonLog(Err(message)) => {
                self.daemon.log_error = Some(text::clean(&message));
                self.report_daemon_once("daemon log", Worded::local(&message));
            }
            Msg::DaemonStopped(result) => {
                self.daemon.busy = None;
                match result {
                    Ok(_) => {
                        self.daemon.stopped = true;
                        self.daemon.status = None;
                        self.set_notice("daemon stopped".into());
                    }
                    Err(error) => self.api_error("stop daemon", None, error, &mut effects),
                }
            }
            Msg::Profiles { lane, result } => self.profiles_loaded(lane, result, &mut effects),
            Msg::DaemonRestarted(result) | Msg::DaemonStarted(result) => {
                let verb = if self.daemon.busy == Some("restarting") {
                    "restart"
                } else {
                    "start"
                };
                self.daemon.busy = None;
                match result {
                    Ok(status) => {
                        self.daemon.stopped = false;
                        self.daemon.status = Some(status);
                        self.set_notice(format!("daemon {verb}ed"));
                        // Reconnect now instead of waiting for the next backoff step.
                        effects.extend([Effect::Subscribe, Effect::Refresh, Effect::FetchDaemon]);
                    }
                    Err(error) => {
                        self.api_error(&format!("{verb} daemon"), None, error, &mut effects);
                    }
                }
            }
        }
        effects
    }

    /// Reports a daemon-tab failure unless it repeats the last one.
    fn report_daemon_once(&mut self, what: &str, worded: Worded) {
        let line = format!("{what}: {}", worded.one_line());
        if self.daemon.last_error.as_ref() != Some(&line) {
            self.daemon.last_error = Some(line);
            self.report(what, None, worded);
        }
    }

    /// True while a clean of `lane` is in flight.
    pub fn is_cleaning(&self, lane: &str) -> bool {
        self.cleaning.contains(lane)
    }

    // ---- errors -----------------------------------------------------------

    /// The single path for every failure: the error list, the header notice
    /// and, when it has one, the target's badge and details.
    pub fn report(&mut self, what: &str, target: Option<Target>, worded: Worded) {
        let what = text::clean(what);
        let state = target.as_ref().and_then(|t| self.target_state(t));
        let entry = ErrorEntry {
            what: what.clone(),
            target: target.clone(),
            worded,
            state,
        };
        self.notice = Some(Notice {
            text: format!("{what}: {}", entry.worded.one_line()),
            error: true,
        });
        self.notice_ticks = ERROR_TICKS;
        if let Some(target) = target {
            self.target_errors.insert(target, entry.clone());
        }
        self.errors.push_front(entry);
        self.errors.truncate(MAX_ERRORS);
    }

    /// The last failed action on a lane or unit, while it still applies.
    pub fn target_error(&self, target: &Target) -> Option<&ErrorEntry> {
        self.target_errors.get(target)
    }

    /// The daemon state of a target, if it is still listed.
    fn target_state(&self, target: &Target) -> Option<String> {
        let lane = self
            .lanes
            .iter()
            .find(|lane| lane.reference == target_lane(target))?;
        match target {
            Target::Lane(_) => Some(lane.state.clone()),
            Target::Unit { unit, .. } => lane
                .units
                .iter()
                .find(|u| u.name == *unit)
                .map(|u| u.state.clone()),
        }
    }

    /// Drops badges for targets that are gone or became healthy since the failure.
    fn prune_target_errors(&mut self) {
        let states: HashMap<Target, Option<String>> = self
            .target_errors
            .keys()
            .map(|target| (target.clone(), self.target_state(target)))
            .collect();
        self.target_errors.retain(
            |target, entry| match states.get(target).cloned().flatten() {
                None => false,
                Some(state) => {
                    let changed = entry.state.as_deref() != Some(state.as_str());
                    !(changed && matches!(state.as_str(), "ready" | "running"))
                }
            },
        );
    }

    /// Lines of the error list wrapped to `width`, newest error first.
    pub fn error_panel_lines(&self, width: usize) -> Vec<PanelLine> {
        let mut lines = Vec::new();
        for entry in &self.errors {
            for line in text::wrap(&entry.what, width) {
                lines.push(PanelLine::Title(line));
            }
            for line in text::wrap(&entry.worded.summary, width) {
                lines.push(PanelLine::Summary(line));
            }
            for detail in &entry.worded.detail {
                for line in text::wrap(detail, width) {
                    lines.push(PanelLine::Detail(line));
                }
            }
            if let Some(command) = &entry.worded.command {
                for line in text::wrap(&format!("command: {command}"), width) {
                    lines.push(PanelLine::Hint(line));
                }
            }
            if let Some(hint) = &entry.worded.hint {
                for line in text::wrap(&format!("hint: {hint}"), width) {
                    lines.push(PanelLine::Hint(line));
                }
            }
            lines.push(PanelLine::Blank);
        }
        lines.pop();
        lines
    }

    /// Scrolls the error list, clamped so the last line stays at the bottom.
    fn scroll_errors(&mut self, scroll: usize, delta: isize) -> usize {
        let (width, height) = error_panel_inner(self.size);
        let total = self.error_panel_lines(width).len();
        scroll
            .saturating_add_signed(delta)
            .min(total.saturating_sub(height))
    }

    // ---- connection -------------------------------------------------------

    /// Marks the daemon unreachable and schedules one retry with backoff. The
    /// first loss goes to the error list, unless this TUI is stopping or
    /// restarting the daemon itself; retries only update the header.
    fn lost_connection(&mut self, reason: Option<&ApiError>, effects: &mut Vec<Effect>) {
        let expected = self.daemon.stopped || self.daemon.busy.is_some();
        let attempt = match self.connection {
            Connection::Reconnecting { attempt } => attempt.saturating_add(1),
            _ => {
                if !expected {
                    let worded = match reason {
                        Some(error) => words::api(error),
                        None => Worded::local("the event stream ended"),
                    };
                    self.report("daemon connection", None, worded);
                }
                0
            }
        };
        self.connection = Connection::Reconnecting { attempt };
        if !self.reconnect_pending {
            self.reconnect_pending = true;
            let after_ms = (250u64 << attempt.min(5)).min(MAX_BACKOFF_MS);
            effects.push(Effect::ScheduleReconnect { after_ms });
        }
    }

    /// Reports a request failure; a lost connection also starts reconnecting.
    /// A failed action keeps its target either way, so the row shows what failed.
    fn api_error(
        &mut self,
        what: &str,
        target: Option<Target>,
        error: ApiError,
        effects: &mut Vec<Effect>,
    ) {
        if error.is_disconnect() && target.is_none() {
            self.lost_connection(Some(&error), effects);
            return;
        }
        self.report(what, target, words::api(&error));
        if error.is_disconnect() {
            self.lost_connection(Some(&error), effects);
        }
    }

    /// Re-opens the attach stream after the daemon or the unit came back,
    /// keeping its role and the focus.
    fn reattach_if_closed(&mut self, effects: &mut Vec<Effect>) {
        let closed = self
            .attached
            .as_ref()
            .is_some_and(|a| matches!(a.state, AttachState::Closed | AttachState::Failed(_)));
        if closed {
            self.reopen_attach(effects);
        }
    }

    // ---- events -----------------------------------------------------------

    /// Applies a subscription event to the cached lanes.
    fn event(&mut self, event: Event, effects: &mut Vec<Effect>) {
        match event.event.as_str() {
            events::RESYNC => effects.push(Effect::Refresh),
            events::LANE_CHANGED => {
                let lane = event
                    .data
                    .get("lane")
                    .cloned()
                    .map(serde_json::from_value::<LaneSummary>);
                let Some(Ok(lane)) = lane else {
                    // A payload this client cannot read means the cache may be stale.
                    effects.push(Effect::Refresh);
                    return;
                };
                match self
                    .lanes
                    .iter_mut()
                    .find(|l| l.reference == lane.reference)
                {
                    Some(existing) => *existing = lane,
                    None => self.lanes.push(lane),
                }
                sort_lanes(&mut self.lanes);
                self.after_change(effects);
            }
            events::LANE_REMOVED => {
                if let Some(reference) = event.data.get("lane").and_then(Value::as_str) {
                    self.lanes.retain(|lane| lane.reference != reference);
                    self.after_change(effects);
                }
            }
            events::UNIT_CHANGED => {
                let unit = event
                    .data
                    .get("unit")
                    .cloned()
                    .map(serde_json::from_value::<UnitSummary>);
                let Some(Ok(unit)) = unit else {
                    effects.push(Effect::Refresh);
                    return;
                };
                let Some(lane) = self.lanes.iter_mut().find(|l| l.reference == unit.lane) else {
                    effects.push(Effect::Refresh);
                    return;
                };
                let is_attached = self
                    .attached
                    .as_ref()
                    .is_some_and(|a| a.lane == unit.lane && a.unit == unit.name);
                match lane.units.iter_mut().find(|u| u.name == unit.name) {
                    Some(existing) => *existing = unit,
                    None => {
                        lane.units.push(unit);
                        sort_units(&mut lane.units);
                    }
                }
                if is_attached {
                    // A restart starts a new PTY; reopen a stream that ended with the old one.
                    self.reattach_if_closed(effects);
                }
                self.after_change(effects);
            }
            events::PHASE if event.data.get("state").and_then(Value::as_str) == Some("failed") => {
                let lane = text::clean(
                    event
                        .data
                        .get("lane")
                        .and_then(Value::as_str)
                        .unwrap_or("lane"),
                );
                // This TUI's own action reports the same failure when it returns.
                if self.pending.contains_key(&lane) {
                    return;
                }
                let phase = text::clean(
                    event
                        .data
                        .get("phase")
                        .and_then(Value::as_str)
                        .unwrap_or("start"),
                );
                let worded = event
                    .data
                    .get("reason")
                    .cloned()
                    .and_then(|reason| serde_json::from_value::<RpcError>(reason).ok())
                    .map_or_else(
                        || Worded::local(&format!("{phase} failed")),
                        |reason| words::rpc(&reason),
                    );
                self.report(&format!("{phase} {lane}"), Some(Target::Lane(lane)), worded);
            }
            _ => {}
        }
    }

    /// Keeps selection, list scroll and the attach stream consistent with the lanes.
    fn after_change(&mut self, effects: &mut Vec<Effect>) {
        let rows = self.rows();
        let valid = self
            .selected
            .as_ref()
            .is_some_and(|selected| rows.iter().any(|row| self.row_matches(*row, selected)));
        if !valid {
            self.selected = self.fallback_selection(&rows);
        }
        self.prune_target_errors();
        self.clamp_monitor();
        self.ensure_visible();
        self.sync_attach(effects);
    }

    /// Picks the lane of a vanished unit, else the first selectable row.
    fn fallback_selection(&self, rows: &[Row]) -> Option<Selected> {
        if let Some(Selected::Unit { lane, .. }) = &self.selected
            && rows
                .iter()
                .any(|row| matches!(row, Row::Lane(i) if self.lanes[*i].reference == *lane))
        {
            return Some(Selected::Lane(lane.clone()));
        }
        rows.iter().find_map(|row| self.selection_for(*row))
    }

    // ---- tree -------------------------------------------------------------

    /// Visible rows: project headers, lanes, and units of expanded lanes.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut project: Option<&str> = None;
        for (lane_index, lane) in self.lanes.iter().enumerate() {
            if project != Some(lane.project.as_str()) {
                project = Some(lane.project.as_str());
                rows.push(Row::Project(lane_index));
            }
            rows.push(Row::Lane(lane_index));
            if self.is_expanded(lane) {
                rows.extend((0..lane.units.len()).map(|unit| Row::Unit(lane_index, unit)));
            }
        }
        rows
    }

    /// Lanes doing nothing start collapsed; the user's toggle wins afterwards.
    pub fn is_expanded(&self, lane: &LaneSummary) -> bool {
        self.expanded
            .get(&lane.reference)
            .copied()
            .unwrap_or_else(|| {
                !matches!(lane.state.as_str(), "idle" | "unassigned" | "no-config")
                    || lane.units.iter().any(|u| is_active(&u.state))
            })
    }

    /// Selection identity for a row; project headers are not selectable.
    fn selection_for(&self, row: Row) -> Option<Selected> {
        match row {
            Row::Project(_) => None,
            Row::Lane(lane) => Some(Selected::Lane(self.lanes[lane].reference.clone())),
            Row::Unit(lane, unit) => Some(Selected::Unit {
                lane: self.lanes[lane].reference.clone(),
                unit: self.lanes[lane].units[unit].name.clone(),
            }),
        }
    }

    /// True when `row` is the row `selected` points to.
    fn row_matches(&self, row: Row, selected: &Selected) -> bool {
        self.selection_for(row).as_ref() == Some(selected)
    }

    /// Index of the selected row in [`App::rows`].
    pub fn selected_row(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows()
            .iter()
            .position(|row| self.row_matches(*row, selected))
    }

    /// The selected lane, or the lane of the selected unit.
    pub fn selected_lane(&self) -> Option<&LaneSummary> {
        let reference = match self.selected.as_ref()? {
            Selected::Lane(lane) | Selected::Unit { lane, .. } => lane,
        };
        self.lanes.iter().find(|lane| lane.reference == *reference)
    }

    /// The selected unit, when a unit row is selected.
    pub fn selected_unit(&self) -> Option<&UnitSummary> {
        let Some(Selected::Unit { unit, .. }) = &self.selected else {
            return None;
        };
        self.selected_lane()?.units.iter().find(|u| u.name == *unit)
    }

    /// Moves the selection by `delta` selectable rows.
    fn move_selection(&mut self, delta: isize, effects: &mut Vec<Effect>) {
        let selectable: Vec<Row> = self
            .rows()
            .into_iter()
            .filter(|row| !matches!(row, Row::Project(_)))
            .collect();
        if selectable.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|s| selectable.iter().position(|row| self.row_matches(*row, s)))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(selectable.len() - 1);
        self.selected = self.selection_for(selectable[next]);
        self.ensure_visible();
        self.sync_attach(effects);
    }

    /// Scrolls the list so the selected row is visible.
    fn ensure_visible(&mut self) {
        let height = self.list_height();
        let Some(row) = self.selected_row() else {
            self.list_offset = 0;
            return;
        };
        if height == 0 {
            return;
        }
        if row <= self.list_offset {
            // Keep the row above (often a project header) visible when moving up.
            self.list_offset = row.saturating_sub(1);
        } else if row >= self.list_offset + height {
            self.list_offset = row + 1 - height;
        }
    }

    /// Rows that fit inside the list border.
    fn list_height(&self) -> usize {
        areas(self.size.0, self.size.1, self.zoom)
            .and_then(|a| a.list)
            .map_or(0, |list| usize::from(list.height.saturating_sub(2)))
    }

    /// Expands or collapses the selected lane.
    fn toggle_lane(&mut self, effects: &mut Vec<Effect>) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let expanded = self.is_expanded(lane);
        let reference = lane.reference.clone();
        self.expanded.insert(reference.clone(), !expanded);
        if expanded {
            self.selected = Some(Selected::Lane(reference));
        }
        self.after_change(effects);
    }

    // ---- attach -----------------------------------------------------------

    /// Attaches to the selected unit and detaches from anything else. An
    /// orphaned unit has no stream; the pane explains it instead.
    fn sync_attach(&mut self, effects: &mut Vec<Effect>) {
        let orphaned = self.selected_unit().is_some_and(is_orphaned);
        let wanted = match &self.selected {
            Some(Selected::Unit { lane, unit }) if !orphaned => Some((lane.clone(), unit.clone())),
            _ => None,
        };
        let current = self
            .attached
            .as_ref()
            .map(|a| (a.lane.clone(), a.unit.clone()));
        if wanted == current {
            return;
        }
        if self.attached.take().is_some() {
            effects.push(Effect::Detach);
        }
        self.focus = Focus::List;
        self.want_terminal = false;
        let Some((lane, unit)) = wanted else {
            return;
        };
        // Browsing only watches; focusing the pane or zooming takes the size.
        self.open_attach(lane, unit, false, effects);
    }

    /// Opens a new stream on `lane:unit`, replacing the current one.
    fn open_attach(
        &mut self,
        lane: String,
        unit: String,
        interactive: bool,
        effects: &mut Vec<Effect>,
    ) {
        if self.attached.take().is_some() {
            effects.push(Effect::Detach);
        }
        let (columns, rows) = self.attach_size();
        self.generation += 1;
        self.attached = Some(Attached {
            generation: self.generation,
            lane: lane.clone(),
            unit: unit.clone(),
            state: AttachState::Opening,
            screen: TerminalScreen::new(rows, columns),
            mode: PaneMode::Live,
            interactive,
        });
        effects.push(Effect::Attach {
            generation: self.generation,
            unit: unit_ref(&lane, &unit),
            lane,
            columns,
            rows,
            interactive,
        });
    }

    /// Reopens the current stream for a fresh snapshot, keeping its role and
    /// the focus; keys typed meanwhile are dropped until it is live.
    fn reopen_attach(&mut self, effects: &mut Vec<Effect>) {
        let Some(attached) = self.attached.as_ref() else {
            return;
        };
        let (lane, unit, interactive) = (
            attached.lane.clone(),
            attached.unit.clone(),
            attached.interactive,
        );
        self.open_attach(lane, unit, interactive, effects);
    }

    /// The pane's inner `(columns, rows)`, using the lanes layout on every tab so
    /// switching tabs never resizes the unit's PTY.
    pub fn pane_size(&self) -> (u16, u16) {
        areas(self.size.0, self.size.1, self.zoom).map_or((MIN_WIDTH, MIN_HEIGHT), |a| {
            (a.pane_inner.width.max(1), a.pane_inner.height.max(1))
        })
    }

    /// The size requested for the PTY: the pane, but never below [`MIN_PTY`].
    pub fn attach_size(&self) -> (u16, u16) {
        let (columns, rows) = self.pane_size();
        (columns.max(MIN_PTY.0), rows.max(MIN_PTY.1))
    }

    /// Resizes the local grid and, for an interactive stream, the PTY; once
    /// resizing pauses the stream is reopened, because a grid that shrank
    /// cannot redraw what it cut off.
    fn resize_pane(&mut self, effects: &mut Vec<Effect>) {
        let (columns, rows) = self.attach_size();
        self.ensure_visible();
        if let Some(attached) = self.attached.as_mut()
            && attached.screen.size() != (rows, columns)
        {
            attached.screen.resize(rows, columns);
            if let PaneMode::Copy(state) = &mut attached.mode {
                state.cursor.0 = state.cursor.0.min(rows - 1);
                state.cursor.1 = state.cursor.1.min(columns - 1);
            }
            if attached.state == AttachState::Live && attached.interactive {
                effects.push(Effect::ResizePty { columns, rows });
            }
            self.resize_token += 1;
            effects.push(Effect::SettleResize {
                token: self.resize_token,
                after_ms: RESIZE_SETTLE_MS,
            });
        }
    }

    /// The attached unit when `generation` is current; stale streams are ignored.
    fn attached_mut(&mut self, generation: u64) -> Option<&mut Attached> {
        self.attached
            .as_mut()
            .filter(|attached| attached.generation == generation)
    }

    /// Input modes of the visible screen.
    fn modes(&self) -> InputModes {
        self.attached
            .as_ref()
            .map(|a| InputModes::from_screen(a.screen.screen()))
            .unwrap_or_default()
    }

    // ---- keys -------------------------------------------------------------

    /// Routes a key to the dialog, the terminal, copy mode, logs or the list.
    fn key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        // Below the minimum size nothing is drawn, so nothing may act blind.
        if areas(self.size.0, self.size.1, self.zoom).is_none() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('c') if ctrl => effects.push(Effect::Quit),
                KeyCode::Char('q') if self.dialog.is_none() && self.focus == Focus::List => {
                    effects.push(Effect::Quit);
                }
                KeyCode::Esc => self.dialog = None,
                _ => {}
            }
            return;
        }
        if self.dialog.is_some() {
            self.dialog_key(key, effects);
            return;
        }
        // Shift+Tab is reserved for focus, so apps in the pane never receive it;
        // Tab and C-a reach them (shell completion, tmux prefix).
        if key.code == KeyCode::BackTab {
            self.toggle_focus(effects);
            return;
        }
        if self.focus == Focus::Terminal && self.tab == Tab::Lanes {
            self.terminal_key(key, effects);
            return;
        }
        if let Some(attached) = self.attached.as_mut() {
            match &attached.mode {
                PaneMode::Copy(_) if self.tab == Tab::Lanes => {
                    self.copy_key(key, effects);
                    return;
                }
                PaneMode::Logs(_) if key.code == KeyCode::Esc && self.tab == Tab::Lanes => {
                    attached.mode = PaneMode::Live;
                    attached.screen.scroll_to_bottom();
                    return;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Char('u')
            && key.modifiers.is_empty()
            && self.dismiss_release(effects)
        {
            return;
        }
        match self.tab {
            Tab::Monitor => self.monitor_key(key, effects),
            Tab::Daemon => self.daemon_key(key, effects),
            Tab::Lanes => self.list_key(key, effects),
        }
    }

    /// Switches tabs; the daemon tab loads its facts and log at once.
    fn set_tab(&mut self, tab: Tab, effects: &mut Vec<Effect>) {
        if tab == Tab::Daemon && self.tab != Tab::Daemon {
            self.daemon.ticks = 0;
            effects.push(Effect::ReadDaemonLog {
                from: self.daemon.log_position,
            });
            if !self.daemon.stopped {
                effects.push(Effect::FetchDaemon);
            }
        }
        self.tab = tab;
    }

    /// Keys on the daemon tab. Every action that stops something asks first.
    fn daemon_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = usize::from(self.size.1 / 2).max(1);
        let log_len = self.daemon.log.len();
        match key.code {
            KeyCode::Char('q') => effects.push(Effect::Quit),
            KeyCode::Char('c') if ctrl => effects.push(Effect::Quit),
            KeyCode::Tab | KeyCode::Char('1') | KeyCode::Esc => self.set_tab(Tab::Lanes, effects),
            KeyCode::Char('2') => self.set_tab(Tab::Monitor, effects),
            KeyCode::Char('e') => self.dialog = Some(Dialog::Errors { scroll: 0 }),
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::Char('k') | KeyCode::Up => {
                self.daemon.scroll = (self.daemon.scroll + 1).min(log_len);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.daemon.scroll = self.daemon.scroll.saturating_sub(1);
            }
            KeyCode::PageUp => self.daemon.scroll = (self.daemon.scroll + page).min(log_len),
            KeyCode::PageDown => self.daemon.scroll = self.daemon.scroll.saturating_sub(page),
            // g/G mean top/bottom on every tab: the oldest line, then following again.
            KeyCode::Home | KeyCode::Char('g') => self.daemon.scroll = log_len,
            KeyCode::End | KeyCode::Char('G') => self.daemon.scroll = 0,
            KeyCode::Char('R') => self.daemon_prompt(
                "restart the daemon?",
                "restart",
                "the TUI reconnects when the new daemon is up.",
                Effect::RestartDaemon,
            ),
            KeyCode::Char('S') => self.daemon_prompt(
                "stop the daemon?",
                "stop",
                "press s on this tab to start it again.",
                Effect::StopDaemon,
            ),
            KeyCode::Char('X') => {
                if self.daemon.busy.is_none() {
                    self.dialog = Some(Dialog::Confirm {
                        prompt: "run lane gc?".into(),
                        detail: vec![
                            "it cleans every lane whose worktree is gone: stops its units, runs its lane.clean tasks and infra teardown, removes its per-lane containers and volumes, and frees its ports.".into(),
                            "it also drops port offsets that no config uses.".into(),
                        ],
                        action: "run gc",
                        strict: true,
                        effect: Box::new(Effect::LaneGc),
                    });
                }
            }
            KeyCode::Char('s') => {
                if !self.daemon.stopped && self.connection == Connection::Online {
                    self.set_notice("the daemon is running".into());
                } else if self.daemon.busy.is_none() {
                    self.daemon.busy = Some("starting");
                    effects.push(Effect::StartDaemon);
                }
            }
            _ => {}
        }
    }

    /// The confirm dialog for restarting or stopping the daemon, with the
    /// number of processes it would stop.
    fn daemon_prompt(&mut self, prompt: &str, action: &'static str, after: &str, effect: Effect) {
        if self.daemon.busy.is_some() {
            self.set_notice(format!(
                "the daemon is busy {}",
                self.daemon.busy.unwrap_or("")
            ));
            return;
        }
        if self.daemon.stopped {
            self.set_notice("the daemon is stopped; s starts it".into());
            return;
        }
        let processes = match self.daemon.managed_processes() {
            Some(0) => "no managed processes are running.".to_owned(),
            Some(1) => {
                "it stops 1 managed process; start it again afterwards with devlanes lane resume."
                    .to_owned()
            }
            Some(n) => format!(
                "it stops {n} managed processes; start them again afterwards with devlanes lane resume."
            ),
            None => "it stops every process the daemon manages.".to_owned(),
        };
        self.dialog = Some(Dialog::Confirm {
            prompt: prompt.into(),
            detail: vec![processes, after.into()],
            action,
            strict: true,
            effect: Box::new(effect),
        });
    }

    /// Opens the clean dialog for the selected lane, or the lane of the selected unit.
    fn clean_prompt(&mut self) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let reference = lane.reference.clone();
        if self.cleaning.contains(&reference) {
            self.set_notice(format!("{reference} is already being cleaned"));
            return;
        }
        self.dialog = Some(Dialog::TypeConfirm {
            prompt: format!("clean {reference}?"),
            detail: vec![
                "it stops the lane's units, runs its lane.clean tasks and infra teardown, removes its per-lane containers and volumes, and frees its ports and setup records.".into(),
                "worktree files are not touched.".into(),
            ],
            expected: reference.clone(),
            input: String::new(),
            mismatch: false,
            effect: Box::new(Effect::CleanLane { lane: reference }),
        });
    }

    /// Records an action the user just confirmed, so its progress shows.
    fn begin(&mut self, effect: &Effect) {
        match effect {
            Effect::Run { lane, .. } => self.pending_begin(lane),
            Effect::CleanLane { lane } => {
                self.pending_begin(lane);
                self.cleaning.insert(lane.clone());
                self.set_notice(format!("cleaning {lane}"));
            }
            Effect::LaneGc => self.daemon.busy = Some("collecting lanes"),
            Effect::RestartDaemon => self.daemon.busy = Some("restarting"),
            Effect::StopDaemon => self.daemon.busy = Some("stopping"),
            _ => {}
        }
    }

    /// Toggles list and terminal focus.
    fn toggle_focus(&mut self, effects: &mut Vec<Effect>) {
        match self.focus {
            Focus::Terminal => {
                self.focus = Focus::List;
                self.want_terminal = false;
            }
            Focus::List => self.focus_terminal(effects),
        }
    }

    /// Moves focus to the terminal pane, only onto a live stream. A watching
    /// stream is reopened as interactive first, so typing comes with the size.
    fn focus_terminal(&mut self, effects: &mut Vec<Effect>) {
        if self.tab != Tab::Lanes {
            self.set_notice("the terminal is on the lanes tab".into());
            return;
        }
        let Some(attached) = self.attached.as_ref() else {
            let notice = match self.selected_unit() {
                Some(unit) if is_orphaned(unit) => {
                    "a previous daemon left this process running; it has no terminal".into()
                }
                _ => "select a unit to type into it".into(),
            };
            self.set_notice(notice);
            return;
        };
        match (&attached.state, attached.interactive) {
            (AttachState::Live, true) => self.focus = Focus::Terminal,
            (AttachState::Live | AttachState::Opening, false) => {
                self.want_terminal = true;
                let (lane, unit) = (attached.lane.clone(), attached.unit.clone());
                self.open_attach(lane, unit, true, effects);
            }
            (AttachState::Opening, true) => self.want_terminal = true,
            (AttachState::Closed, _) => {
                self.set_notice("the terminal ended; it reopens when the unit runs again".into());
            }
            (AttachState::Failed(reason), _) => {
                let notice = format!("no terminal: {}", reason.summary);
                self.set_notice(notice);
            }
        }
    }

    /// Records the start of an action on `lane`.
    fn pending_begin(&mut self, lane: &str) {
        *self.pending.entry(lane.to_owned()).or_insert(0) += 1;
    }

    /// Records the end of an action on `lane`.
    fn pending_end(&mut self, lane: &str) {
        if let Some(count) = self.pending.get_mut(lane) {
            *count -= 1;
            if *count == 0 {
                self.pending.remove(lane);
            }
        }
    }

    /// Keys in list focus.
    fn list_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => effects.push(Effect::Quit),
            KeyCode::Char('c') if ctrl => effects.push(Effect::Quit),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1, effects),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1, effects),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2, effects),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2, effects),
            KeyCode::Enter | KeyCode::Char(' ') => match self.selected {
                Some(Selected::Lane(_)) => self.toggle_lane(effects),
                Some(Selected::Unit { .. }) => self.focus_terminal(effects),
                None => {}
            },
            KeyCode::Left | KeyCode::Char('h') => {
                if self
                    .selected_lane()
                    .is_some_and(|lane| self.is_expanded(lane))
                {
                    self.toggle_lane(effects);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self
                    .selected_lane()
                    .is_some_and(|lane| !self.is_expanded(lane))
                {
                    self.toggle_lane(effects);
                }
            }
            KeyCode::Tab | KeyCode::Char('2') => self.set_tab(Tab::Monitor, effects),
            KeyCode::Char('3') => self.set_tab(Tab::Daemon, effects),
            KeyCode::Char('e') => self.dialog = Some(Dialog::Errors { scroll: 0 }),
            KeyCode::Char('1') => self.set_tab(Tab::Lanes, effects),
            // Shift+C: Option chords become characters in many macOS terminals.
            KeyCode::Char('C') => self.clean_prompt(),
            KeyCode::Char('s') => self.control(Method::Start, effects),
            KeyCode::Char('x') => self.control(Method::Stop, effects),
            KeyCode::Char('r') => self.control(Method::Restart, effects),
            KeyCode::Char('t') => self.run_prompt(),
            KeyCode::Char('p') => self.profile_prompt(effects),
            KeyCode::Char('z') => {
                self.zoom = !self.zoom;
                // Zooming means looking at this terminal: take its size.
                let watching = self
                    .attached
                    .as_ref()
                    .filter(|a| !a.interactive)
                    .map(|a| (a.lane.clone(), a.unit.clone()));
                match watching {
                    Some((lane, unit)) => {
                        self.ensure_visible();
                        self.open_attach(lane, unit, true, effects);
                    }
                    None => self.resize_pane(effects),
                }
            }
            KeyCode::Char('o') => self.open_url(effects),
            KeyCode::Char('O') => self.open_terminal(effects),
            KeyCode::Char('c') => self.enter_copy(),
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::PageUp | KeyCode::Char('[') => {
                let page = self.page();
                self.scroll(page, effects);
            }
            KeyCode::PageDown | KeyCode::Char(']') => {
                let page = self.page();
                self.scroll(-page, effects);
            }
            KeyCode::Esc => {
                if let Some(attached) = self.attached.as_mut() {
                    attached.screen.scroll_to_bottom();
                }
            }
            _ => {}
        }
    }

    /// Keys in terminal focus go to the unit, except the focus toggle. With no
    /// live stream they are dropped, never handed to list shortcuts.
    fn terminal_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let modes = self.modes();
        let Some(attached) = self.attached.as_mut() else {
            self.focus = Focus::List;
            return;
        };
        if attached.state != AttachState::Live {
            self.set_notice("the terminal is not live; shift+tab returns to the list".into());
            return;
        }
        // Typing returns to the live screen, like a real terminal.
        attached.mode = PaneMode::Live;
        attached.screen.scroll_to_bottom();
        if let Some(bytes) = input::encode_key(&key, modes) {
            effects.push(Effect::Input(bytes));
        }
    }

    /// Paste goes to a text prompt or, in terminal focus, to the unit.
    fn paste(&mut self, text: &str, effects: &mut Vec<Effect>) {
        if let Some(Dialog::TypeConfirm { input, .. }) = self.dialog.as_mut() {
            input.push_str(text.trim());
            return;
        }
        if self.focus != Focus::Terminal || self.dialog.is_some() {
            return;
        }
        let modes = self.modes();
        if self
            .attached
            .as_ref()
            .is_some_and(|a| a.state == AttachState::Live)
        {
            effects.push(Effect::Input(input::encode_paste(text, modes)));
        }
    }

    /// Keys while a dialog is open.
    fn dialog_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::Help => {}
            Dialog::Errors { scroll } => {
                let page = isize::try_from(error_panel_inner(self.size).1)
                    .unwrap_or(1)
                    .max(1);
                let delta = match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('e') => return,
                    KeyCode::Char('j') | KeyCode::Down => 1,
                    KeyCode::Char('k') | KeyCode::Up => -1,
                    KeyCode::PageDown | KeyCode::Char(' ') => page,
                    KeyCode::PageUp => -page,
                    KeyCode::Char('g') | KeyCode::Home => isize::MIN / 2,
                    KeyCode::Char('G') | KeyCode::End => isize::MAX / 2,
                    _ => 0,
                };
                let scroll = self.scroll_errors(scroll, delta);
                self.dialog = Some(Dialog::Errors { scroll });
            }
            Dialog::Confirm {
                prompt,
                detail,
                action,
                strict,
                effect,
            } => match key.code {
                // A stray Enter must not stop or remove anything.
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.begin(&effect);
                    effects.push(*effect);
                }
                KeyCode::Enter if !strict => {
                    self.begin(&effect);
                    effects.push(*effect);
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => {
                    self.dialog = Some(Dialog::Confirm {
                        prompt,
                        detail,
                        action,
                        strict,
                        effect,
                    });
                }
            },
            Dialog::TypeConfirm {
                prompt,
                detail,
                expected,
                mut input,
                mut mismatch,
                effect,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter if input.trim() == expected => {
                    self.begin(&effect);
                    effects.push(*effect);
                }
                code => {
                    // The warning is for the text that was submitted; editing clears it.
                    match code {
                        KeyCode::Enter => mismatch = true,
                        KeyCode::Backspace => {
                            input.pop();
                            mismatch = false;
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            input.push(c);
                            mismatch = false;
                        }
                        _ => {}
                    }
                    self.dialog = Some(Dialog::TypeConfirm {
                        prompt,
                        detail,
                        expected,
                        input,
                        mismatch,
                        effect,
                    });
                }
            },
            Dialog::Tasks {
                lane,
                tasks,
                mut index,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Enter => {
                    if let Some(task) = tasks.get(index) {
                        self.confirm_run(lane, task.clone());
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    index = (index + 1).min(tasks.len().saturating_sub(1));
                    self.dialog = Some(Dialog::Tasks { lane, tasks, index });
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    index = index.saturating_sub(1);
                    self.dialog = Some(Dialog::Tasks { lane, tasks, index });
                }
                _ => self.dialog = Some(Dialog::Tasks { lane, tasks, index }),
            },
            Dialog::Profiles { lane, state } => self.profiles_key(key, lane, state, effects),
        }
    }

    /// Keys in the profile picker.
    fn profiles_key(
        &mut self,
        key: KeyEvent,
        lane: String,
        state: ProfilesState,
        effects: &mut Vec<Effect>,
    ) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            return;
        }
        let ProfilesState::Ready {
            mut items,
            mut index,
        } = state
        else {
            // Nothing to pick while loading or on an older daemon.
            self.dialog = Some(Dialog::Profiles { lane, state });
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                index = (index + 1).min(items.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => index = index.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(item) = items.get_mut(index) {
                    item.checked = !item.checked;
                }
            }
            // A config without profiles has nothing to pick; Enter starts nothing.
            KeyCode::Enter if items.is_empty() => {}
            KeyCode::Enter => {
                // List order, so later profiles' env wins the way the config reads.
                let mut profiles: Vec<String> = items
                    .iter()
                    .filter(|item| item.checked)
                    .map(|item| item.profile.name.clone())
                    .collect();
                let label = if profiles.is_empty() {
                    // An empty list would make the daemon reuse the lane's last
                    // profiles; `default` is accepted even when the config has no
                    // such profile, and then means none.
                    let default = items.iter().find(|item| item.profile.is_default);
                    profiles.push(
                        default.map_or_else(|| "default".into(), |item| item.profile.name.clone()),
                    );
                    match default {
                        Some(_) => format!("start {lane} with the default profile"),
                        None => format!("start {lane} without a profile"),
                    }
                } else {
                    format!("start {lane} with {}", profiles.join(", "))
                };
                self.pending_begin(&lane);
                effects.push(Effect::Control {
                    method: Method::Start,
                    target: Target::Lane(lane.clone()),
                    lane,
                    selection: Selection {
                        profiles,
                        ..Selection::default()
                    },
                    label,
                });
                return;
            }
            _ => {}
        }
        self.dialog = Some(Dialog::Profiles {
            lane,
            state: ProfilesState::Ready { items, index },
        });
    }

    /// Fills the picker with the lane's profiles, checking the ones it runs now.
    fn profiles_loaded(
        &mut self,
        lane: String,
        result: Result<crate::api::ProfileList, ApiError>,
        effects: &mut Vec<Effect>,
    ) {
        let open =
            matches!(&self.dialog, Some(Dialog::Profiles { lane: open, .. }) if *open == lane);
        let current = self
            .lanes
            .iter()
            .find(|l| l.reference == lane)
            .map(lane_profiles)
            .unwrap_or_default();
        match result {
            Ok(list) if open => {
                let items: Vec<ProfileChoice> = list
                    .items
                    .into_iter()
                    .map(|profile| ProfileChoice {
                        checked: current.contains(&profile.name),
                        profile,
                    })
                    .collect();
                self.dialog = Some(Dialog::Profiles {
                    lane,
                    state: ProfilesState::Ready { items, index: 0 },
                });
            }
            // The picker was closed while loading; nothing to show.
            Ok(_) => {}
            Err(error) if crate::api::is_unsupported(&error) => {
                if open {
                    self.dialog = Some(Dialog::Profiles {
                        lane,
                        state: ProfilesState::Unsupported,
                    });
                }
            }
            Err(error) => {
                if open {
                    self.dialog = None;
                }
                self.api_error(&format!("profiles of {lane}"), None, error, effects);
            }
        }
    }

    /// Keys on the monitor tab.
    fn monitor_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => effects.push(Effect::Quit),
            KeyCode::Char('c') if ctrl => effects.push(Effect::Quit),
            KeyCode::Tab | KeyCode::Char('3') => self.set_tab(Tab::Daemon, effects),
            KeyCode::Char('1') | KeyCode::Esc => self.set_tab(Tab::Lanes, effects),
            KeyCode::Char('e') => self.dialog = Some(Dialog::Errors { scroll: 0 }),
            KeyCode::Char('j') | KeyCode::Down => {
                self.monitor_index = self.monitor_index.saturating_add(1);
                self.clamp_monitor();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.monitor_index = self.monitor_index.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.monitor_index = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.monitor_index = usize::MAX;
                self.clamp_monitor();
            }
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            _ => {}
        }
    }

    // ---- actions ----------------------------------------------------------

    /// Start/stop/restart the selected unit, or the whole selected lane.
    fn control(&mut self, method: Method, effects: &mut Vec<Effect>) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let reference = lane.reference.clone();
        let verb = method_verb(method);
        let error_target = match self.selected_unit() {
            Some(unit) => Target::Unit {
                lane: reference.clone(),
                unit: unit.name.clone(),
            },
            None => Target::Lane(reference.clone()),
        };
        let (units, target) = match self.selected_unit() {
            Some(unit) => {
                let unit = unit_ref(&reference, &unit.name);
                (vec![unit.clone()], unit)
            }
            // Restart names units explicitly (the CLI requires them); start and stop act on the lane.
            None if method == Method::Restart => {
                let units: Vec<String> = lane
                    .units
                    .iter()
                    .filter(|u| u.kind != "task" && is_active(&u.state))
                    .map(|u| unit_ref(&reference, &u.name))
                    .collect();
                if units.is_empty() {
                    self.set_notice(format!("nothing running in {reference}"));
                    return;
                }
                (units, reference.clone())
            }
            None => (Vec::new(), reference.clone()),
        };
        self.pending_begin(&reference);
        effects.push(Effect::Control {
            method,
            lane: reference,
            selection: Selection {
                units,
                ..Selection::default()
            },
            label: format!("{verb} {target}"),
            target: error_target,
        });
    }

    /// Asks before running a task: tasks can be migrations or seeds.
    fn run_prompt(&mut self) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let reference = lane.reference.clone();
        if let Some(unit) = self.selected_unit()
            && unit.kind == "task"
        {
            let task = unit.name.clone();
            self.confirm_run(reference, task);
            return;
        }
        let tasks: Vec<String> = lane
            .units
            .iter()
            .filter(|u| u.kind == "task")
            .map(|u| u.name.clone())
            .collect();
        if tasks.is_empty() {
            self.set_notice(format!("no tasks in {reference}"));
            return;
        }
        self.dialog = Some(Dialog::Tasks {
            lane: reference,
            tasks,
            index: 0,
        });
    }

    /// Opens the confirm dialog for one task.
    fn confirm_run(&mut self, lane: String, task: String) {
        self.dialog = Some(Dialog::Confirm {
            prompt: format!("run {task} in {lane}?"),
            detail: Vec::new(),
            action: "run",
            strict: false,
            effect: Box::new(Effect::Run {
                lane,
                params: RunParams {
                    task,
                    env: BTreeMap::new(),
                    // The TUI shows the task in its pane instead of streaming the request.
                    detach: true,
                    terminal: true,
                },
            }),
        });
    }

    /// Selects the task that just started; interactive tasks also get focus.
    fn run_done(
        &mut self,
        lane: String,
        task: String,
        result: Result<Value, ApiError>,
        effects: &mut Vec<Effect>,
    ) {
        self.pending_end(&lane);
        let target = Target::Unit {
            lane: lane.clone(),
            unit: task.clone(),
        };
        match result {
            Ok(value) => {
                self.target_errors.remove(&target);
                self.set_notice(format!("run {task}: started"));
                self.expanded.insert(lane.clone(), true);
                self.selected = Some(Selected::Unit { lane, unit: task });
                self.after_change(effects);
                let interactive = value.get("interactive").and_then(Value::as_bool) == Some(true);
                if interactive {
                    // Reopens the task's stream as interactive; focus follows once it is live.
                    self.focus_terminal(effects);
                }
            }
            Err(error) => {
                // A task that is not listed as a unit attaches its error to the lane.
                let target = if self.target_state(&target).is_some() {
                    target
                } else {
                    Target::Lane(lane.clone())
                };
                self.api_error(
                    &format!("run {task} in {lane}"),
                    Some(target),
                    error,
                    effects,
                );
            }
        }
    }

    /// Opens the profile picker and asks the daemon for the lane's profiles.
    fn profile_prompt(&mut self, effects: &mut Vec<Effect>) {
        if let Some(lane) = self.selected_lane() {
            let lane = lane.reference.clone();
            self.dialog = Some(Dialog::Profiles {
                lane: lane.clone(),
                state: ProfilesState::Loading,
            });
            effects.push(Effect::FetchProfiles { lane });
        }
    }

    /// Opens the unit's first URL, or the lane's first unit URL.
    fn open_url(&mut self, effects: &mut Vec<Effect>) {
        let url = match self.selected_unit() {
            Some(unit) => unit.urls.first().cloned(),
            None => self
                .selected_lane()
                .and_then(|lane| lane.units.iter().find_map(|u| u.urls.first().cloned())),
        };
        match url {
            Some(url) => effects.push(Effect::OpenUrl(url)),
            None => self.set_notice("no url".into()),
        }
    }

    /// Opens a terminal attached to the unit, or a shell in the lane's worktree.
    fn open_terminal(&mut self, effects: &mut Vec<Effect>) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let attach = self
            .selected_unit()
            .map(|u| unit_ref(&lane.reference, &u.name));
        let title = attach.clone().unwrap_or_else(|| lane.reference.clone());
        effects.push(Effect::OpenTerminal {
            cwd: lane.path.clone(),
            attach,
            title,
        });
    }

    // ---- scrollback and copy ---------------------------------------------

    /// Half a pane, the scroll step for page keys.
    fn page(&self) -> isize {
        isize::try_from(self.pane_size().1 / 2).unwrap_or(1).max(1)
    }

    /// Scrolls the pane; going past the client-side scrollback loads log pages.
    fn scroll(&mut self, delta: isize, effects: &mut Vec<Effect>) {
        let height = usize::from(self.pane_size().1);
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        match &mut attached.mode {
            PaneMode::Copy(_) => {}
            PaneMode::Live => {
                if attached.screen.scroll_by(delta) && attached.state != AttachState::Opening {
                    attached.mode = PaneMode::Logs(LogView {
                        loading: true,
                        ..LogView::default()
                    });
                    effects.push(Effect::FetchLogs {
                        generation: attached.generation,
                        lane: attached.lane.clone(),
                        query: log_query(&attached.lane, &attached.unit, None),
                        older: false,
                    });
                }
            }
            PaneMode::Logs(view) => {
                if delta < 0 && view.offset == 0 {
                    // Scrolling down past the newest stored line returns to the live screen.
                    attached.mode = PaneMode::Live;
                    attached.screen.scroll_to_bottom();
                    return;
                }
                let max = view.lines.len().saturating_sub(height);
                view.offset = view.offset.saturating_add_signed(delta).min(max);
                if delta > 0
                    && view.offset == max
                    && !view.loading
                    && let Some(cursor) = view.older.clone()
                {
                    view.loading = true;
                    effects.push(Effect::FetchLogs {
                        generation: attached.generation,
                        lane: attached.lane.clone(),
                        query: log_query(&attached.lane, &attached.unit, Some(cursor)),
                        older: true,
                    });
                }
            }
        }
    }

    /// Stores a page of log lines; older pages go above the loaded ones.
    fn logs(
        &mut self,
        generation: u64,
        older: bool,
        result: Result<Page<LogLine>, ApiError>,
        effects: &mut Vec<Effect>,
    ) {
        // A page for a unit that is no longer shown has no place to go.
        let Some(attached) = self.attached_mut(generation) else {
            return;
        };
        let what = format!("logs {}", unit_ref(&attached.lane, &attached.unit));
        let PaneMode::Logs(view) = &mut attached.mode else {
            return;
        };
        view.loading = false;
        match result {
            Ok(page) => {
                view.older = if page.truncated {
                    page.next_cursor
                } else {
                    None
                };
                if older {
                    let mut lines = page.items;
                    lines.append(&mut view.lines);
                    view.lines = lines;
                } else {
                    view.lines = page.items;
                }
            }
            Err(error) => self.api_error(&what, None, error, effects),
        }
    }

    /// Enters copy mode with the cursor at the bottom-left of the pane.
    fn enter_copy(&mut self) {
        let Some(attached) = self.attached.as_mut() else {
            self.set_notice("select a unit to copy from".into());
            return;
        };
        let (rows, _) = attached.screen.size();
        attached.mode = PaneMode::Copy(CopyState {
            cursor: (rows.saturating_sub(1), 0),
            anchor: None,
        });
    }

    /// Keys in copy mode.
    fn copy_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        let (rows, columns) = attached.screen.size();
        let PaneMode::Copy(state) = &mut attached.mode else {
            return;
        };
        let last_column = columns.saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => attached.mode = PaneMode::Live,
            KeyCode::Char('h') | KeyCode::Left => state.cursor.1 = state.cursor.1.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => {
                state.cursor.1 = (state.cursor.1 + 1).min(last_column);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if state.cursor.0 == 0 {
                    // At the top edge the view scrolls instead of the cursor.
                    attached.screen.scroll_by(1);
                } else {
                    state.cursor.0 -= 1;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if state.cursor.0 + 1 >= rows {
                    attached.screen.scroll_by(-1);
                } else {
                    state.cursor.0 += 1;
                }
            }
            KeyCode::Char('0') | KeyCode::Home => state.cursor.1 = 0,
            KeyCode::Char('$') | KeyCode::End => state.cursor.1 = last_column,
            KeyCode::Char('v') | KeyCode::Char(' ') => {
                state.anchor = match state.anchor {
                    Some(_) => None,
                    None => Some(state.cursor),
                };
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                // Without a selection, the cursor's line is copied.
                let (start, end) = match state.anchor {
                    Some(anchor) => (anchor, state.cursor),
                    None => ((state.cursor.0, 0), (state.cursor.0, last_column)),
                };
                let text = attached.screen.text_between(start, end);
                attached.mode = PaneMode::Live;
                let text = text.trim_end().to_owned();
                if !text.is_empty() {
                    effects.push(Effect::Copy(text));
                    // OSC 52 has no reply: the terminal may ignore it silently.
                    self.set_notice(
                        "copied with OSC 52; it reaches the clipboard if the terminal allows that"
                            .into(),
                    );
                }
            }
            _ => {}
        }
    }

    // ---- mouse ------------------------------------------------------------

    /// Wheel scrolls (or is forwarded when the unit wants mouse input); clicks select and focus.
    fn mouse(&mut self, mouse: MouseEvent, effects: &mut Vec<Effect>) {
        if self.dialog.is_some() || self.tab != Tab::Lanes {
            return;
        }
        let Some(layout) = areas(self.size.0, self.size.1, self.zoom) else {
            return;
        };
        let position = (mouse.column, mouse.row).into();
        if layout.pane_inner.contains(position) {
            let modes = self.modes();
            let live = self.attached.as_ref().is_some_and(|a| {
                a.state == AttachState::Live
                    && matches!(a.mode, PaneMode::Live)
                    && a.screen.scroll_offset() == 0
            });
            if self.focus == Focus::Terminal && live {
                let column = mouse.column - layout.pane_inner.x;
                let row = mouse.row - layout.pane_inner.y;
                if let Some(bytes) = input::encode_mouse(&mouse, column, row, modes) {
                    effects.push(Effect::Input(bytes));
                    return;
                }
            }
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll(WHEEL_LINES, effects),
                MouseEventKind::ScrollDown => self.scroll(-WHEEL_LINES, effects),
                MouseEventKind::Down(_) if self.focus == Focus::List => {
                    self.focus_terminal(effects);
                }
                _ => {}
            }
            return;
        }
        let Some(list) = layout.list.filter(|list| list.contains(position)) else {
            return;
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-1, effects),
            MouseEventKind::ScrollDown => self.move_selection(1, effects),
            MouseEventKind::Down(_) => {
                self.focus = Focus::List;
                // Rows live between the borders; a click on either border selects nothing.
                if mouse.row + 1 >= list.bottom() {
                    return;
                }
                let Some(offset) = mouse.row.checked_sub(list.y + 1) else {
                    return;
                };
                let rows = self.rows();
                if let Some(row) = rows.get(self.list_offset + usize::from(offset))
                    && let Some(selected) = self.selection_for(*row)
                {
                    self.selected = Some(selected);
                    self.sync_attach(effects);
                }
            }
            _ => {}
        }
    }

    // ---- misc -------------------------------------------------------------

    /// Ages the notice, polls metrics on the monitor and follows the daemon
    /// tab's facts and log while it is visible.
    fn tick(&mut self, effects: &mut Vec<Effect>) {
        if self.notice_ticks > 0 {
            self.notice_ticks -= 1;
            if self.notice_ticks == 0 {
                self.notice = None;
            }
        }
        if self.tab == Tab::Monitor && self.connection == Connection::Online {
            effects.push(Effect::FetchMetrics);
        }
        if self.tab == Tab::Daemon {
            // The log is a local file, readable even while the daemon is down.
            effects.push(Effect::ReadDaemonLog {
                from: self.daemon.log_position,
            });
            self.daemon.ticks += 1;
            if self.daemon.ticks >= DAEMON_REFRESH_TICKS {
                self.daemon.ticks = 0;
                if self.connection == Connection::Online && self.daemon.busy.is_none() {
                    effects.push(Effect::FetchDaemon);
                }
            }
        }
    }

    /// The release notice for the header and its key: the daemon runs another
    /// version than this TUI, or a newer release is out. `None` once dismissed.
    pub fn release_badge(&self) -> Option<(String, String)> {
        use devlanes_client::update;
        let status = self.daemon.status.as_ref()?;
        let own = env!("CARGO_PKG_VERSION");
        let daemon = status.get("version").and_then(Value::as_str);
        let notice = match daemon {
            Some(daemon) if daemon != own => Some((
                update::skew_key(daemon, own),
                format!(
                    "daemon runs {daemon}, this TUI {own}: restart it when convenient (it stops services)"
                ),
            )),
            _ => None,
        };
        let notice = notice.or_else(|| {
            let latest = status.pointer("/update/latest").and_then(Value::as_str)?;
            if !devlanes_proto::release::is_newer(latest, own) {
                return None;
            }
            let command = self.release.0.as_ref()?.upgrade_command(latest);
            Some((
                update::update_key(latest),
                format!("devlanes {latest} is available: {command}"),
            ))
        })?;
        (!self.release.1.contains(&notice.0)).then_some(notice)
    }

    /// Hides the release notice for good; a newer version shows again.
    fn dismiss_release(&mut self, effects: &mut Vec<Effect>) -> bool {
        let Some((key, _)) = self.release_badge() else {
            return false;
        };
        self.release.1.push(key.clone());
        effects.push(Effect::DismissNotice(key));
        true
    }

    /// Shows a short status message in the header; errors go through [`App::report`].
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(Notice {
            text: text::clean(&notice),
            error: false,
        });
        self.notice_ticks = NOTICE_TICKS;
    }

    /// Rows of the monitor table: each lane's total, then its units. Unit
    /// numbers come from metrics when the daemon has them, else from status.
    pub fn monitor_rows(&self) -> Vec<MonitorRow> {
        let units: Vec<UnitMetrics> = if self.metrics.is_empty() {
            self.lanes
                .iter()
                .flat_map(|lane| lane.units.iter())
                .filter(|unit| is_active(&unit.state))
                .map(|unit| UnitMetrics {
                    lane: unit.lane.clone(),
                    unit: unit.name.clone(),
                    cpu_percent: unit.cpu_percent,
                    memory_bytes: unit.memory_bytes,
                    ..UnitMetrics::default()
                })
                .collect()
        } else {
            self.metrics.clone()
        };
        let mut lanes: Vec<String> = units.iter().map(|unit| unit.lane.clone()).collect();
        lanes.sort();
        lanes.dedup();
        let mut rows = Vec::new();
        for lane in lanes {
            let members: Vec<&UnitMetrics> = units.iter().filter(|u| u.lane == lane).collect();
            let cpu = members
                .iter()
                .filter_map(|u| u.cpu_percent)
                .reduce(|a, b| a + b);
            let memory = members
                .iter()
                .filter_map(|u| u.memory_bytes)
                .reduce(|a, b| a + b);
            rows.push(MonitorRow::Lane {
                lane: lane.clone(),
                cpu_percent: cpu,
                memory_bytes: memory,
            });
            let mut sorted: Vec<UnitMetrics> = members.into_iter().cloned().collect();
            sorted.sort_by_key(|unit| unit.unit.to_lowercase());
            rows.extend(sorted.into_iter().map(MonitorRow::Unit));
        }
        rows
    }

    /// Keeps the monitor selection inside the table.
    fn clamp_monitor(&mut self) {
        let len = self.monitor_rows().len();
        self.monitor_index = self.monitor_index.min(len.saturating_sub(1));
    }

    /// The listed state of the attached unit.
    pub fn attached_unit(&self) -> Option<&UnitSummary> {
        let attached = self.attached.as_ref()?;
        self.lanes
            .iter()
            .find(|lane| lane.reference == attached.lane)?
            .units
            .iter()
            .find(|unit| unit.name == attached.unit)
    }
}

/// `acme@2` + `api` gives `acme@2:api`, the unit reference the daemon resolves.
pub fn unit_ref(lane: &str, unit: &str) -> String {
    format!("{lane}:{unit}")
}

/// A service that failed during a start: its reason (or exit code) and the
/// last lines of its log, for the details pane.
fn failure_words(failure: &devlanes_proto::UnitFailureData) -> Worded {
    let mut worded = match (&failure.reason, failure.exit_code) {
        (Some(reason), _) => words::rpc(reason),
        (None, Some(code)) => Worded::local(&format!("exited with code {code}")),
        (None, None) => Worded::local("did not become ready"),
    };
    worded
        .detail
        .extend(failure.log_tail.iter().map(|line| text::clean(line)));
    worded
        .hint
        .get_or_insert_with(|| format!("devlanes logs {}", unit_ref(&failure.lane, &failure.unit)));
    worded
}

/// States with a live process behind them, or one about to start. Orphans
/// count: their process runs, so their lane is expanded and restart covers them.
pub fn is_active(state: &str) -> bool {
    matches!(
        state,
        "queued" | "starting" | "ready" | "running" | "stopping" | ORPHANED
    )
}

/// A process a previous daemon left running: no output stream to attach to.
pub fn is_orphaned(unit: &UnitSummary) -> bool {
    unit.state == ORPHANED
}

/// Display order: services and tasks together by name, ad-hoc exec units
/// after them. Only the TUI's copy is sorted; the daemon starts units in
/// declaration order.
pub fn sort_units(units: &mut [UnitSummary]) {
    units.sort_by(|a, b| {
        let key = |unit: &UnitSummary| (unit.kind == "exec", unit.name.to_lowercase());
        key(a).cmp(&key(b)).then_with(|| a.name.cmp(&b.name))
    });
}

/// Plain verb for notices.
fn method_verb(method: Method) -> &'static str {
    match method {
        Method::Start => "start",
        Method::Stop => "stop",
        Method::Restart => "restart",
        _ => "request",
    }
}

/// The lane reference of a target.
pub fn target_lane(target: &Target) -> &str {
    match target {
        Target::Lane(lane) | Target::Unit { lane, .. } => lane,
    }
}

/// The error list's box: the whole body between header and help bar, so long
/// lines get every column and nothing peeks through at the edges.
pub fn error_panel_area(size: (u16, u16)) -> Rect {
    let (width, height) = size;
    Rect::new(0, 1.min(height), width, height.saturating_sub(2))
}

/// The error list's text `(columns, rows)` inside its border.
pub fn error_panel_inner(size: (u16, u16)) -> (usize, usize) {
    let area = error_panel_area(size);
    (
        usize::from(area.width.saturating_sub(2)).max(1),
        usize::from(area.height.saturating_sub(2)).max(1),
    )
}

/// Newest lines of one unit, older than `cursor` when given.
fn log_query(lane: &str, unit: &str, cursor: Option<String>) -> LogQuery {
    LogQuery {
        units: vec![unit_ref(lane, unit)],
        tail: Some(LOG_PAGE),
        cursor,
        ..LogQuery::default()
    }
}

/// Projects by name, lanes by index, unassigned lanes last; units sorted
/// inside each lane.
fn sort_lanes(lanes: &mut [LaneSummary]) {
    for lane in lanes.iter_mut() {
        sort_units(&mut lane.units);
    }
    lanes.sort_by(|a, b| {
        (
            a.project.as_str(),
            a.index.is_none(),
            a.index,
            a.reference.as_str(),
        )
            .cmp(&(
                b.project.as_str(),
                b.index.is_none(),
                b.index,
                b.reference.as_str(),
            ))
    });
}

/// True for units started by an agent; the list shows a badge for them.
pub fn started_by_agent(unit: &UnitSummary) -> bool {
    unit.actor.kind == ActorKind::Agent
}

/// The profiles of the lane's last start. Older daemons send only `profile`.
pub fn lane_profiles(lane: &LaneSummary) -> Vec<String> {
    if lane.profiles.is_empty() {
        lane.profile.iter().cloned().collect()
    } else {
        lane.profiles.clone()
    }
}
