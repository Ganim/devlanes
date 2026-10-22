//! TUI state and the pure update function: messages in, effects out.
//!
//! Nothing here performs I/O, so key handling, event application and resync
//! behavior are tested without a terminal or a daemon.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use devlanes_proto::{
    ActorKind, AttachResult, Event, LaneSummary, LogLine, LogQuery, Method, ORPHANED, Page,
    RpcError, RunParams, Selection, UnitSummary, events,
};
use devlanes_term::{Progress, StartTracker};

pub use crate::start::{StartExit, StartFrame, StartRun};
use ratatui::layout::{Position, Rect};
use serde_json::Value;

use crate::api::{ApiError, Metrics, MetricsQuery, TaskInfo};
use crate::input::{self, InputModes};
pub use crate::monitor::{MonitorId, MonitorRow, MonitorView, SortKey};
use crate::screen::TerminalScreen;
use crate::text;
use crate::theme::Theme;
use crate::tree::{self, Node};
pub use crate::tree::{Row, RowId, Section};
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

/// Where a list row's content starts: column 0 holds the selection marker,
/// then two columns per level. Headers draw their `▾`/`▸` there.
pub fn row_indent(depth: u8) -> u16 {
    1 + 2 * u16::from(depth)
}
const MAX_BACKOFF_MS: u64 = 5000;

/// Two presses on the same row this close together are a double click.
const DOUBLE_CLICK_MS: u128 = 400;
/// The dismissal key of the first-run hint strip, stored with release notices.
pub const FIRST_RUN_KEY: &str = "tui-first-run";

/// Where keys go: watching (keys drive devlanes) or typing (keys go to the
/// unit in the pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Terminal,
}

/// What the body shows: the selected project, or one of the two views that
/// open over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Project,
    Monitor,
    Daemon,
}

/// What a click on a top-bar, lane-bar or footer item does. Rendering records
/// these with the rectangles it drew, so hit tests match the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    Project(String),
    Lane(String),
    /// The start in the top bar: that lane's whole-lane row.
    StartLane(String),
    /// The problem counter in the top bar.
    FirstProblem,
    /// A key hint: the click presses the key.
    Key(KeyCode, KeyModifiers),
    /// A monitor table row.
    MonitorRow(MonitorId),
}

/// The text filter on the list (`/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub text: String,
    /// Keys go to the filter until Enter or Esc.
    pub editing: bool,
}

/// What `task.list` said for a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tasks {
    Loading,
    Loaded(Vec<TaskInfo>),
    /// An older daemon, or a lane without config: units of kind task only.
    Unavailable,
}

/// One phase of a lane's start, as `lane.phase` events report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseStep {
    pub phase: String,
    /// `started`, `progress`, `done`, `failed` or `skipped`.
    pub state: String,
    pub detail: Option<String>,
    /// Ticks (seconds) since the phase started, or how long it took.
    pub seconds: u64,
}

/// The phases of a lane's latest start, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanePhases {
    pub start_id: Option<u64>,
    pub steps: Vec<PhaseStep>,
    /// Ticks since the start began.
    pub seconds: u64,
}

/// Lines of the daemon log kept client-side.
const MAX_DAEMON_LOG: usize = 2000;
/// Daemon facts refresh every this many one-second ticks while the tab is visible.
const DAEMON_REFRESH_TICKS: u8 = 3;
/// Doctor refreshes this often while the daemon view is closed, for its badge.
const DOCTOR_BACKGROUND_TICKS: u8 = 60;

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
    /// Copy mode on the log, like the unit terminal's: whole lines.
    pub copy: Option<LogCopy>,
    /// The restart, stop, start or gc in flight.
    pub busy: Option<&'static str>,
    /// Set after this TUI stopped the daemon, until one is reachable again.
    pub stopped: bool,
    /// Facts and log failures repeat every refresh; only a change is reported.
    last_error: Option<String>,
    ticks: u8,
    /// Ticks until the next background doctor refresh.
    background: u8,
}

/// The cursor and selection of copy mode on the daemon log, as log line indexes
/// (0 is the oldest kept line). The view ends at the selection's newest line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCopy {
    pub cursor: usize,
    /// Where `v` started the selection.
    pub anchor: Option<usize>,
}

impl LogCopy {
    /// The selected lines, first to last inclusive; the cursor line alone
    /// before `v`.
    pub fn range(self) -> (usize, usize) {
        let anchor = self.anchor.unwrap_or(self.cursor);
        (anchor.min(self.cursor), anchor.max(self.cursor))
    }
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

/// The selected row and the lane it is in. The lane also decides the project
/// tab and the lane chip that show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub lane: String,
    pub row: RowId,
}

impl Selected {
    /// The whole-lane row of `lane`.
    pub fn lane(lane: &str) -> Self {
        Self {
            lane: lane.into(),
            row: RowId::WholeLane,
        }
    }

    /// The row of unit `unit` in `lane`.
    pub fn unit(lane: &str, unit: &str) -> Self {
        Self {
            lane: lane.into(),
            row: RowId::Unit(unit.into()),
        }
    }
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

/// Modal prompts; while one is open it receives every key.
#[derive(Debug)]
pub enum Dialog {
    /// `y` confirms; `Enter` too unless `strict` (the daemon's stop and restart).
    Confirm {
        /// The action, as the box title: `clean acme@2`.
        title: String,
        prompt: String,
        /// What will happen, one sentence per entry.
        detail: Vec<String>,
        /// The verb for the key hint (`y restart`).
        action: &'static str,
        strict: bool,
        effect: Box<Effect>,
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
    /// Input or the stream itself failed on an open attach; it closes next.
    StreamError {
        generation: u64,
        error: ApiError,
    },
    /// The daemon refused to resize an open attach; the stream goes on.
    ResizeRefused {
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
    Metrics(Result<Metrics, ApiError>),
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
    Tasks {
        lane: String,
        result: Result<Vec<TaskInfo>, ApiError>,
    },
    /// A spinner frame passed; sent only while something spins.
    Spin,
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
        /// The user confirmed a task whose config asks first.
        yes: bool,
    },
    /// `task.list` for one lane.
    FetchTasks {
        lane: String,
    },
    FetchLogs {
        generation: u64,
        lane: String,
        query: LogQuery,
        older: bool,
    },
    /// Samples over the monitor's range.
    FetchMetrics(MetricsQuery),
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

/// Screen regions shared by rendering, mouse hit-testing and PTY sizing. No
/// boxes: one header row per panel and a `│` column between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    /// Row 0: devlanes, project tabs, messages, the view buttons.
    pub header: Rect,
    /// Row 1 in the project view: the lane chips.
    pub lane_bar: Rect,
    /// The list's box, its title in the top border, and its rows inside.
    pub list_box: Option<Rect>,
    pub list: Option<Rect>,
    /// The pane's box; the unit's name and state are its title.
    pub pane_box: Rect,
    /// The pane's content: the unit's terminal or the details.
    pub pane_inner: Rect,
    /// Everything between the header and the footer, for the monitor and daemon views.
    pub body: Rect,
    /// The first-run hint strip above the footer.
    pub strip: Option<Rect>,
    pub footer: Rect,
}

/// Splits the screen, or `None` when it is below the minimum size.
pub fn areas(width: u16, height: u16, zoom: bool) -> Option<Areas> {
    areas_with(width, height, zoom, false)
}

/// [`areas`] with the first-run hint strip above the footer when `strip`.
pub fn areas_with(width: u16, height: u16, zoom: bool, strip: bool) -> Option<Areas> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }
    let strip = strip && height > MIN_HEIGHT;
    let header = Rect::new(0, 0, width, 1);
    let footer = Rect::new(0, height - 1, width, 1);
    let bottom = height - 1 - u16::from(strip);
    let body = Rect::new(0, 1, width, bottom - 1);
    let lane_bar = Rect::new(0, 1, width, 1);
    // Two boxes under the lane bar, as the navigation.md mockups draw them.
    let top = 2;
    let panel_height = bottom - top;
    let inner = |rect: Rect| Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
    let (list_box, pane_x) = if zoom {
        (None, 0)
    } else {
        // 48 columns from 120 wide up, as in the mockups; the pane gets the rest.
        let list_width = (width * 2 / 5).clamp(30, 48);
        (
            Some(Rect::new(0, top, list_width, panel_height)),
            list_width,
        )
    };
    let pane_box = Rect::new(pane_x, top, width - pane_x, panel_height);
    let pane = inner(pane_box);
    Some(Areas {
        header,
        lane_bar,
        list_box,
        list: list_box.map(inner),
        pane_box,
        // One column of margin inside the border, as a terminal window has.
        pane_inner: Rect::new(pane.x + 1, pane.y, pane.width - 1, pane.height),
        body,
        strip: strip.then(|| Rect::new(0, bottom, width, 1)),
        footer,
    })
}

/// All TUI state.
pub struct App {
    pub lanes: Vec<LaneSummary>,
    pub loaded: bool,
    /// The selected row; its lane is the lane shown and its project the tab.
    pub selected: Option<Selected>,
    pub focus: Focus,
    pub view: View,
    pub zoom: bool,
    pub filter: Option<Filter>,
    pub theme: Theme,
    /// Advances on every spinner frame.
    pub spin: u64,
    /// Rectangles of clickable chrome, written by the last render.
    pub hits: RefCell<Vec<(Rect, Click)>>,
    /// `task.list` per lane.
    pub tasks: HashMap<String, Tasks>,
    /// The phases of each lane's latest start.
    pub phases: HashMap<String, LanePhases>,
    /// The row selected last in each lane, restored when it shows again.
    remembered: HashMap<String, RowId>,
    /// The lane shown last in each project.
    project_lane: HashMap<String, String>,
    /// Folds the user made, per lane.
    folds: HashMap<String, HashMap<RowId, bool>>,
    /// Groups folded to make the lane fit the first time it showed.
    auto_folds: HashMap<String, HashSet<RowId>>,
    /// The last press in the list, for double clicks.
    last_click: Option<(std::time::Instant, RowId)>,
    /// Where a press in the pane began: a release there types, a drag selects.
    pane_press: Option<(u16, u16)>,
    /// The open dialog's box, written by the last render.
    pub dialog_area: RefCell<Option<Rect>>,
    /// The working directory, to open on its lane first.
    cwd: Option<PathBuf>,
    /// Tab and Enter were used, so the first-run strip has done its job.
    learned: (bool, bool),
    pub connection: Connection,
    pub dialog: Option<Dialog>,
    pub attached: Option<Attached>,
    /// The latest `metrics` reply; `None` until one arrives or after an error.
    pub metrics: Option<Metrics>,
    pub monitor: MonitorView,
    pub list_offset: usize,
    pub notice: Option<Notice>,
    /// Recent errors, newest first.
    pub errors: VecDeque<ErrorEntry>,
    pub daemon: DaemonView,
    pub size: (u16, u16),
    /// How devlanes was installed (`None` until known) and dismissed release notices.
    pub release: (Option<devlanes_client::update::InstallMethod>, Vec<String>),
    /// Lanes whose task list was fetched again after their config changed.
    tasks_refreshed: HashSet<String>,
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
            view: View::Project,
            zoom: false,
            filter: None,
            theme: Theme::default(),
            spin: 0,
            hits: RefCell::new(Vec::new()),
            tasks: HashMap::new(),
            phases: HashMap::new(),
            remembered: HashMap::new(),
            project_lane: HashMap::new(),
            folds: HashMap::new(),
            auto_folds: HashMap::new(),
            last_click: None,
            pane_press: None,
            dialog_area: RefCell::new(None),
            cwd: None,
            learned: (false, false),
            connection: Connection::Connecting,
            dialog: None,
            attached: None,
            metrics: None,
            monitor: MonitorView::default(),
            list_offset: 0,
            notice: None,
            errors: VecDeque::new(),
            daemon: DaemonView::default(),
            size: (width, height),
            release: (None, Vec::new()),
            tasks_refreshed: HashSet::new(),
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
                if event.event == events::PHASE {
                    let data = event.data.clone();
                    let lane = run.event(&event);
                    self.track_phase(&data);
                    if let Some(lane) = lane {
                        self.pending_begin(&lane);
                    }
                    self.focus_start(effects);
                    return;
                }
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
                    && self.selected == Some(Selected::lane(lane))
                    && failure.lane == *lane
                {
                    self.select(Selected::unit(&failure.lane, &failure.unit), effects);
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
        self.view = View::Project;
        // The whole-lane row: its phase list says what the start is doing.
        self.select(Selected::lane(&lane), effects);
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
            Msg::Status(Err(error)) => self.api_error("daemon status", None, error, &mut effects),
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
                    self.report("live updates", None, words::api(&error));
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
                let no_pty = self.attached_has_no_pty();
                let Some(attached) = self.attached_mut(generation) else {
                    return effects;
                };
                match result {
                    Ok(_) => {
                        attached.state = AttachState::Live;
                        // Another viewer may have resized the PTY since the attach was sent.
                        if attached.interactive && !no_pty {
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
                        let what = format!(
                            "open terminal of {}",
                            unit_ref(&attached.lane, &attached.unit)
                        );
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
                    let what = format!("terminal of {}", unit_ref(&attached.lane, &attached.unit));
                    self.report(&what, None, worded);
                }
            }
            Msg::ResizeRefused { generation, error } => {
                let Some(attached) = self.attached_mut(generation) else {
                    return effects;
                };
                // Pipe units come with `pty: false` and are never resized, so a
                // refusal is a real error. Marking the unit pipe-only here once
                // left a PTY unit at its old width after every later zoom.
                let unit = unit_ref(&attached.lane, &attached.unit);
                self.report(
                    &format!("resize terminal of {unit}"),
                    None,
                    words::api(&error),
                );
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
                    self.set_notice("output ended: esc goes back to the list".into());
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
                    Ok(value) => {
                        self.target_errors.remove(&target);
                        self.set_notice(done_notice(&label, &value));
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
                self.metrics = Some(metrics);
                self.clamp_monitor();
            }
            Msg::Metrics(Err(error)) => {
                // The table falls back to per-unit numbers from status.
                self.metrics = None;
                if error.is_disconnect() {
                    self.lost_connection(Some(&error), &mut effects);
                } else {
                    let worded = words::api(&error);
                    let line = worded.one_line();
                    if self.last_metrics_error.as_ref() != Some(&line) {
                        self.last_metrics_error = Some(line);
                        self.report("resource samples", None, worded);
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
                            0 => format!("cleaned {lane}"),
                            1 => format!("cleaned {lane}, stopped 1 unit"),
                            n => format!("cleaned {lane}, stopped {n} units"),
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
                            "no lanes with missing worktrees".into()
                        } else {
                            format!("cleaned {}", cleaned.join(", "))
                        });
                        effects.push(Effect::Refresh);
                    }
                    Err(error) => self.api_error("clean gone lanes", None, error, &mut effects),
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
                    Err(error) => failure = failure.or(Some(("doctor checks", error))),
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
                let mut dropped = 0;
                while self.daemon.log.len() > MAX_DAEMON_LOG {
                    self.daemon.log.pop_front();
                    dropped += 1;
                }
                self.daemon.scroll = self.daemon.scroll.min(self.daemon.log.len());
                self.daemon.log_position = chunk.position;
                // Copy mode stays on the same lines; a rotated log ends it.
                self.daemon.copy = match self.daemon.copy {
                    Some(copy) if !chunk.reset && copy.cursor >= dropped => {
                        let copy = LogCopy {
                            cursor: copy.cursor - dropped,
                            anchor: copy.anchor.map(|a| a.saturating_sub(dropped)),
                        };
                        let last = self.daemon.log.len().saturating_sub(1);
                        self.daemon.scroll = last.saturating_sub(copy.range().1);
                        Some(copy)
                    }
                    _ => None,
                };
            }
            Msg::DaemonLog(Err(message)) => {
                self.daemon.log_error = Some(text::clean(&message));
                self.report_daemon_once("read daemon.log", Worded::local(&message));
            }
            Msg::DaemonStopped(result) => {
                self.daemon.busy = None;
                match result {
                    Ok(_) => {
                        self.daemon.stopped = true;
                        self.daemon.status = None;
                        self.set_notice("daemon stopped".into());
                    }
                    Err(error) => self.api_error("stop the daemon", None, error, &mut effects),
                }
            }
            Msg::Profiles { lane, result } => self.profiles_loaded(lane, result, &mut effects),
            Msg::Tasks { lane, result } => {
                // Without `task.list` the list shows the tasks that ran.
                let tasks = match result {
                    Ok(tasks) => Tasks::Loaded(tasks),
                    Err(_) => Tasks::Unavailable,
                };
                self.tasks.insert(lane, tasks);
                self.after_change(&mut effects);
            }
            Msg::Spin => self.spin = self.spin.wrapping_add(1),
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
                        self.api_error(&format!("{verb} the daemon"), None, error, &mut effects);
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
            text: format!("{what} failed: {}", entry.worded.one_line()),
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
                        None => Worded::local("the daemon closed live updates"),
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

    /// Follows a `lane.phase` event in the lane's phase list. A new
    /// `start_id` begins a new list. Applying the same event twice changes
    /// nothing, since the start's own stream repeats the subscription's events.
    fn track_phase(&mut self, data: &Value) {
        let field = |key: &str| data.get(key).and_then(Value::as_str).map(text::clean);
        let (Some(lane), Some(phase), Some(state)) =
            (field("lane"), field("phase"), field("state"))
        else {
            return;
        };
        let start = data.get("start_id").and_then(Value::as_u64);
        let phases = self.phases.entry(lane).or_default();
        if start.is_some() && phases.start_id != start {
            *phases = LanePhases {
                start_id: start,
                ..LanePhases::default()
            };
        }
        let detail = devlanes_term::phase_detail(data);
        match phases.steps.iter_mut().find(|step| step.phase == phase) {
            Some(step) => {
                // Progress keeps the phase running; only an end changes the state.
                if state != "progress" {
                    step.state = state;
                }
                if detail.is_some() {
                    step.detail = detail;
                }
            }
            None => phases.steps.push(PhaseStep {
                phase,
                state: if state == "progress" {
                    "started".into()
                } else {
                    state
                },
                detail,
                seconds: 0,
            }),
        }
    }

    /// Applies a subscription event to the cached lanes.
    fn event(&mut self, event: Event, effects: &mut Vec<Effect>) {
        if event.event == events::PHASE {
            self.track_phase(&event.data);
        }
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

    /// Keeps selection, list scroll, task lists and the attach stream
    /// consistent with the lanes.
    fn after_change(&mut self, effects: &mut Vec<Effect>) {
        let lane_known = self
            .selected
            .as_ref()
            .is_some_and(|s| self.lanes.iter().any(|lane| lane.reference == s.lane));
        if !lane_known {
            self.selected = self.initial_lane().map(|lane| Selected {
                row: self.default_row(&lane),
                lane,
            });
        }
        if let Some(lane) = self.selected.as_ref().map(|s| s.lane.clone()) {
            self.auto_fold(&lane);
            let rows = self.rows();
            let row = self.selected.as_ref().map(|s| s.row.clone());
            if let Some(row) = row
                && !rows.iter().any(|r| r.id == row)
            {
                // A task that just ran becomes a unit, and the other way round.
                let twin = match &row {
                    RowId::Unit(name) => Some(RowId::Task(name.clone())),
                    RowId::Task(name) => Some(RowId::Unit(name.clone())),
                    _ => None,
                }
                .filter(|twin| rows.iter().any(|r| r.id == *twin));
                // Else the innermost header still shown, else the first row.
                let nodes = self.nodes();
                let fallback = twin
                    .into_iter()
                    .chain(tree::parents(&nodes, &row))
                    .into_iter()
                    .find(|parent| rows.iter().any(|r| r.id == *parent))
                    .or_else(|| {
                        // Under a filter: the first match.
                        self.filter.as_ref().and_then(|_| {
                            rows.iter()
                                .find(|r| r.folded.is_none() && r.id != RowId::WholeLane)
                                .map(|r| r.id.clone())
                        })
                    })
                    .unwrap_or(RowId::WholeLane);
                if let Some(selected) = self.selected.as_mut() {
                    selected.row = fallback;
                }
            }
            self.remember();
            self.want_tasks(&lane, effects);
        }
        self.prune_target_errors();
        self.clamp_monitor();
        self.ensure_visible();
        self.sync_attach(effects);
    }

    /// The lane to open on: the one whose worktree holds the working
    /// directory, the one shown last, the first project with something
    /// running, else the first lane.
    fn initial_lane(&self) -> Option<String> {
        if let Some(cwd) = &self.cwd
            && let Some(lane) = self
                .lanes
                .iter()
                .filter(|lane| {
                    !lane.path.as_os_str().is_empty()
                        && devlanes_core::paths::path_starts_with(cwd, &lane.path)
                })
                .max_by_key(|lane| lane.path.as_os_str().len())
        {
            return Some(lane.reference.clone());
        }
        let running = self
            .lanes
            .iter()
            .find(|lane| lane.units.iter().any(|u| is_active(&u.state)));
        running
            .or(self.lanes.first())
            .map(|lane| lane.reference.clone())
    }

    /// Whether a spinner is on screen somewhere, so the runtime redraws often.
    pub fn spinning(&self) -> bool {
        self.lanes.iter().any(|lane| {
            matches!(lane.state.as_str(), "starting" | "stopping")
                || lane.units.iter().any(|u| {
                    matches!(u.state.as_str(), "starting" | "stopping")
                        || (u.state == "running" && tree::role(u) != tree::Role::Service)
                })
        })
    }

    /// Opens on the lane of `cwd` the first time lanes load.
    pub fn prefer_path(&mut self, cwd: PathBuf) {
        self.cwd = Some(cwd);
    }

    /// Where the selection starts in a lane: the row selected there last,
    /// else the first problem, else the whole lane when it is stopped or
    /// starting, else its first service.
    fn default_row(&self, lane: &str) -> RowId {
        if let Some(row) = self.remembered.get(lane) {
            return row.clone();
        }
        let Some(summary) = self.lanes.iter().find(|l| l.reference == lane) else {
            return RowId::WholeLane;
        };
        if let Some(problem) = summary.units.iter().find(|u| tree::is_problem(u)) {
            return RowId::Unit(problem.name.clone());
        }
        if matches!(summary.state.as_str(), "running" | "failed") {
            let nodes = tree::tree(summary, self.task_list(lane), None);
            if let Some(first) = nodes
                .iter()
                .find(|node| node.id == RowId::Section(Section::Services))
                .and_then(|node| node.leaves().into_iter().next())
            {
                return first;
            }
        }
        RowId::WholeLane
    }

    /// Stores the selection as the lane's and the project's last.
    fn remember(&mut self) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        if let Some(project) = self
            .lanes
            .iter()
            .find(|lane| lane.reference == selected.lane)
            .map(|lane| lane.project.clone())
        {
            self.project_lane.insert(project, selected.lane.clone());
        }
        self.remembered.insert(selected.lane, selected.row);
    }

    /// Asks for the lane's task list the first time it shows, and again
    /// after its config changed.
    fn want_tasks(&mut self, lane: &str, effects: &mut Vec<Effect>) {
        let changed = self
            .lanes
            .iter()
            .find(|l| l.reference == lane)
            .is_some_and(|l| l.config_changed);
        let stale = match self.tasks.get(lane) {
            None => true,
            // A changed config may define other tasks; fetch once per change.
            Some(Tasks::Loaded(_)) => changed && !self.tasks_refreshed.contains(lane),
            Some(_) => false,
        };
        if stale {
            if changed {
                self.tasks_refreshed.insert(lane.to_owned());
            }
            self.tasks.insert(lane.to_owned(), Tasks::Loading);
            effects.push(Effect::FetchTasks { lane: lane.into() });
        }
        if !changed {
            self.tasks_refreshed.remove(lane);
        }
    }

    /// The task list of `lane` when it loaded.
    pub fn task_list(&self, lane: &str) -> Option<&[TaskInfo]> {
        match self.tasks.get(lane) {
            Some(Tasks::Loaded(tasks)) => Some(tasks),
            _ => None,
        }
    }

    /// What `task.list` says about task `name` in `lane`.
    pub fn task_info(&self, lane: &str, name: &str) -> Option<&TaskInfo> {
        self.task_list(lane)?.iter().find(|task| task.name == name)
    }

    // ---- tree -------------------------------------------------------------

    /// Project names in tab order.
    pub fn projects(&self) -> Vec<String> {
        let mut projects: Vec<String> = Vec::new();
        for lane in &self.lanes {
            if !projects.contains(&lane.project) {
                projects.push(lane.project.clone());
            }
        }
        projects
    }

    /// The project of the selected lane.
    pub fn project(&self) -> Option<&str> {
        self.selected_lane().map(|lane| lane.project.as_str())
    }

    /// The lanes of the project shown, in chip order.
    pub fn project_lanes(&self) -> Vec<&LaneSummary> {
        let Some(project) = self.project() else {
            return Vec::new();
        };
        // Lanes that exist; a worktree without one is not a lane to switch to.
        self.lanes
            .iter()
            .filter(|l| l.project == project && l.index.is_some())
            .collect()
    }

    /// The selected lane's rows as a tree, filtered.
    pub fn nodes(&self) -> Vec<Node> {
        let Some(lane) = self.selected_lane() else {
            return Vec::new();
        };
        let filter = self.filter.as_ref().map(|f| f.text.as_str());
        tree::tree(lane, self.task_list(&lane.reference), filter)
    }

    /// The selected lane's visible rows.
    pub fn rows(&self) -> Vec<Row> {
        let Some(lane) = self.selected.as_ref().map(|s| s.lane.clone()) else {
            return Vec::new();
        };
        let filtering = self.filter.as_ref().is_some_and(|f| !f.text.is_empty());
        let selected = self.selected.as_ref().map(|s| s.row.clone());
        with_gaps(tree::flatten(
            &self.nodes(),
            &|node| self.fold_decision(&lane, node, selected.as_ref()),
            filtering,
        ))
    }

    /// A fold the user made; else a group holding the selection stays open
    /// (a unit that stops moves into `stopped` and must stay in sight); else
    /// a fold the first layout made, unless the group now holds a problem.
    fn fold_decision(&self, lane: &str, node: &Node, selected: Option<&RowId>) -> Option<bool> {
        let id = &node.id;
        if let Some(fold) = self.folds.get(lane).and_then(|folds| folds.get(id)) {
            return Some(*fold);
        }
        if let Some(selected) = selected
            && *selected != *id
            && node.leaves().contains(selected)
        {
            return Some(false);
        }
        let auto = self
            .auto_folds
            .get(lane)
            .is_some_and(|folds| folds.contains(id));
        (auto && !self.holds_problem(lane, id)).then_some(true)
    }

    /// Whether a header holds a unit with a problem or a failed action.
    fn holds_problem(&self, lane: &str, id: &RowId) -> bool {
        let Some(summary) = self.lanes.iter().find(|l| l.reference == lane) else {
            return false;
        };
        let nodes = tree::tree(summary, self.task_list(lane), None);
        let Some(node) = tree::headers(&nodes).into_iter().find(|n| n.id == *id) else {
            return false;
        };
        node.leaves()
            .iter()
            .any(|leaf| self.is_problem_row(summary, leaf))
    }

    /// Whether a row is a problem: a failed or orphaned unit, or a failed action.
    fn is_problem_row(&self, lane: &LaneSummary, row: &RowId) -> bool {
        let name = match row {
            RowId::Unit(name) | RowId::Task(name) => name,
            _ => return false,
        };
        let target = Target::Unit {
            lane: lane.reference.clone(),
            unit: name.clone(),
        };
        self.target_errors.contains_key(&target)
            || lane
                .units
                .iter()
                .any(|u| u.name == *name && tree::is_problem(u))
    }

    /// The first time a lane shows and does not fit, its largest healthy
    /// groups fold, one after another, until it does.
    fn auto_fold(&mut self, lane: &str) {
        if self.auto_folds.contains_key(lane) {
            return;
        }
        let Some(summary) = self.lanes.iter().find(|l| l.reference == lane) else {
            return;
        };
        if summary.units.is_empty() {
            return;
        }
        let height = self.list_height();
        let nodes = tree::tree(summary, self.task_list(lane), None);
        let user = self.folds.get(lane).cloned().unwrap_or_default();
        let selected = self.selected.as_ref().map(|s| s.row.clone());
        let mut folded: HashSet<RowId> = HashSet::new();
        let count = |folded: &HashSet<RowId>| {
            with_gaps(tree::flatten(
                &nodes,
                &|node| {
                    user.get(&node.id)
                        .copied()
                        .or(folded.contains(&node.id).then_some(true))
                },
                false,
            ))
            .len()
        };
        // Groups, and the tasks and infra sections; services stay open, they
        // are what the list is for.
        let foldable = |id: &RowId| {
            matches!(
                id,
                RowId::Group(..) | RowId::Section(tree::Section::Tasks | tree::Section::Infra)
            )
        };
        let mut candidates: Vec<&Node> = tree::headers(&nodes)
            .into_iter()
            .filter(|node| foldable(&node.id) && !node.folded_by_default)
            .filter(|node| !user.contains_key(&node.id))
            .filter(|node| {
                let leaves = node.leaves();
                !leaves.iter().any(|leaf| self.is_problem_row(summary, leaf))
                    && selected.as_ref().is_none_or(|s| !leaves.contains(s))
            })
            .collect();
        candidates.sort_by_key(|node| std::cmp::Reverse(node.leaves().len()));
        for node in candidates {
            if height == 0 || count(&folded) <= height {
                break;
            }
            folded.insert(node.id.clone());
        }
        self.auto_folds.insert(lane.to_owned(), folded);
    }

    /// Index of the selected row in [`App::rows`].
    pub fn selected_row(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows().iter().position(|row| row.id == selected.row)
    }

    /// The selected row's identity.
    pub fn selected_id(&self) -> Option<&RowId> {
        self.selected.as_ref().map(|s| &s.row)
    }

    /// The lane shown.
    pub fn selected_lane(&self) -> Option<&LaneSummary> {
        let reference = &self.selected.as_ref()?.lane;
        self.lanes.iter().find(|lane| lane.reference == *reference)
    }

    /// The selected unit, when a unit row is selected.
    pub fn selected_unit(&self) -> Option<&UnitSummary> {
        let RowId::Unit(unit) = &self.selected.as_ref()?.row else {
            return None;
        };
        self.selected_lane()?.units.iter().find(|u| u.name == *unit)
    }

    /// Selects `row` in the shown lane.
    fn select_row(&mut self, row: RowId, effects: &mut Vec<Effect>) {
        if let Some(selected) = self.selected.as_mut() {
            selected.row = row;
        }
        self.remember();
        self.ensure_visible();
        self.sync_attach(effects);
    }

    /// Moves the selection by `delta` rows, over the blank rows between
    /// sections.
    fn move_selection(&mut self, delta: isize, effects: &mut Vec<Effect>) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let current = self.selected_row().unwrap_or(0);
        let mut next = current.saturating_add_signed(delta).min(rows.len() - 1);
        let step: isize = if delta < 0 { -1 } else { 1 };
        while matches!(rows[next].id, RowId::Gap(_)) {
            match next.checked_add_signed(step).filter(|n| *n < rows.len()) {
                Some(n) => next = n,
                // A gap at the end: step back the other way instead.
                None => {
                    next = next.saturating_add_signed(-step);
                    break;
                }
            }
        }
        self.select_row(rows[next].id.clone(), effects);
    }

    /// Shows `target`'s lane and selects its row, unfolding what hides it.
    pub fn select(&mut self, target: Selected, effects: &mut Vec<Effect>) {
        self.remember();
        let lane = target.lane.clone();
        let nodes = self
            .lanes
            .iter()
            .find(|l| l.reference == lane)
            .map(|summary| tree::tree(summary, self.task_list(&lane), None))
            .unwrap_or_default();
        for parent in tree::parents(&nodes, &target.row) {
            self.folds
                .entry(lane.clone())
                .or_default()
                .insert(parent, false);
        }
        if self.selected.as_ref().is_none_or(|s| s.lane != lane) {
            self.list_offset = 0;
        }
        self.selected = Some(target);
        self.after_change(effects);
    }

    /// Shows lane `lane` with the row selected there last.
    pub fn select_lane(&mut self, lane: &str, effects: &mut Vec<Effect>) {
        if self.selected.as_ref().is_some_and(|s| s.lane == lane) {
            return;
        }
        self.remember();
        let row = self.default_row(lane);
        self.list_offset = 0;
        self.selected = Some(Selected {
            lane: lane.into(),
            row,
        });
        self.after_change(effects);
    }

    /// Shows project `name` on the lane shown there last.
    pub fn select_project(&mut self, name: &str, effects: &mut Vec<Effect>) {
        self.view = View::Project;
        let lane = self.project_lane.get(name).cloned().or_else(|| {
            self.lanes
                .iter()
                .find(|lane| lane.project == name)
                .map(|lane| lane.reference.clone())
        });
        if let Some(lane) = lane {
            self.select_lane(&lane, effects);
        }
    }

    /// The previous or next project tab, wrapping.
    fn step_project(&mut self, delta: isize, effects: &mut Vec<Effect>) {
        let projects = self.projects();
        if projects.is_empty() {
            return;
        }
        let current = self
            .project()
            .and_then(|p| projects.iter().position(|name| name == p))
            .unwrap_or(0);
        let len = projects.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        let name = projects[next].clone();
        self.select_project(&name, effects);
    }

    /// The previous or next lane of the project, stopping at the ends.
    fn step_lane(&mut self, delta: isize, effects: &mut Vec<Effect>) {
        let lanes: Vec<String> = self
            .project_lanes()
            .iter()
            .map(|lane| lane.reference.clone())
            .collect();
        let Some(current) = self
            .selected
            .as_ref()
            .and_then(|s| lanes.iter().position(|lane| *lane == s.lane))
        else {
            return;
        };
        let next = current.saturating_add_signed(delta).min(lanes.len() - 1);
        self.select_lane(&lanes[next].clone(), effects);
    }

    /// Problems of the project, in list order: failed and orphaned units,
    /// failed actions on units and on lanes.
    pub fn problems(&self) -> Vec<Selected> {
        let mut out = Vec::new();
        for lane in self.project_lanes() {
            if self
                .target_errors
                .contains_key(&Target::Lane(lane.reference.clone()))
            {
                out.push(Selected::lane(&lane.reference));
            }
            let nodes = tree::tree(lane, self.task_list(&lane.reference), None);
            for row in tree::flatten(&nodes, &|_| Some(false), true) {
                if self.is_problem_row(lane, &row.id) {
                    out.push(Selected {
                        lane: lane.reference.clone(),
                        row: row.id,
                    });
                }
            }
        }
        out
    }

    /// Moves to the next (`forward`) or previous problem of the project.
    fn next_problem(&mut self, forward: bool, effects: &mut Vec<Effect>) {
        let problems = self.problems();
        if problems.is_empty() {
            self.set_notice("no problems in this project".into());
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|s| problems.iter().position(|p| p == s));
        let next = match (current, forward) {
            (Some(i), true) => (i + 1) % problems.len(),
            (Some(i), false) => (i + problems.len() - 1) % problems.len(),
            (None, true) => 0,
            (None, false) => problems.len() - 1,
        };
        self.select(problems[next].clone(), effects);
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
        let len = self.rows().len();
        if row < self.list_offset {
            self.list_offset = row;
        } else if row >= self.list_offset + height {
            self.list_offset = row + 1 - height;
        }
        self.list_offset = self.list_offset.min(len.saturating_sub(height));
    }

    /// Scrolls the list without moving the selection (the wheel).
    fn scroll_list(&mut self, delta: isize) {
        let height = self.list_height();
        let len = self.rows().len();
        self.list_offset = self
            .list_offset
            .saturating_add_signed(delta)
            .min(len.saturating_sub(height));
    }

    /// The screen's regions, with the first-run strip when it shows.
    pub fn areas(&self) -> Option<Areas> {
        areas_with(self.size.0, self.size.1, self.zoom, self.first_run_strip())
    }

    /// Whether the first-run strip shows: once the dismissals loaded, until
    /// the user typed into a unit and switched projects, or pressed `u`.
    pub fn first_run_strip(&self) -> bool {
        self.release.0.is_some()
            && !self.release.1.iter().any(|key| key == FIRST_RUN_KEY)
            && !(self.learned.0 && self.learned.1)
            && !self.lanes.is_empty()
    }

    /// Rows that fit in the list.
    pub fn list_height(&self) -> usize {
        self.areas()
            .and_then(|a| a.list)
            .map_or(0, |list| usize::from(list.height))
    }

    /// Folds or unfolds a header row; the choice is kept for the session.
    fn toggle_fold(&mut self, id: &RowId, effects: &mut Vec<Effect>) {
        let Some(row) = self.rows().into_iter().find(|row| row.id == *id) else {
            return;
        };
        let Some(folded) = row.folded else {
            return;
        };
        let Some(lane) = self.selected.as_ref().map(|s| s.lane.clone()) else {
            return;
        };
        self.folds
            .entry(lane)
            .or_default()
            .insert(id.clone(), !folded);
        self.after_change(effects);
    }

    /// `h`: folds the selected header, or moves to the row's header.
    fn fold_or_parent(&mut self, effects: &mut Vec<Effect>) {
        let Some(id) = self.selected_id().cloned() else {
            return;
        };
        let row = self.rows().into_iter().find(|row| row.id == id);
        if row.as_ref().and_then(|row| row.folded) == Some(false) {
            self.toggle_fold(&id, effects);
            return;
        }
        if let Some(parent) = tree::parents(&self.nodes(), &id).into_iter().next() {
            self.select_row(parent, effects);
        }
    }

    // ---- attach -----------------------------------------------------------

    /// Attaches to the selected unit and detaches from anything else. An
    /// orphaned unit has no stream; the pane explains it instead.
    fn sync_attach(&mut self, effects: &mut Vec<Effect>) {
        let lane = self.selected.as_ref().map(|s| s.lane.clone());
        let wanted = self
            .selected_unit()
            .filter(|unit| !is_orphaned(unit))
            .map(|unit| unit.name.clone())
            .zip(lane)
            .map(|(unit, lane)| (lane, unit));
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
        self.areas().map_or((MIN_WIDTH, MIN_HEIGHT), |a| {
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
        let no_pty = self.attached_has_no_pty();
        if let Some(attached) = self.attached.as_mut()
            && attached.screen.size() != (rows, columns)
        {
            attached.screen.resize(rows, columns);
            if let PaneMode::Copy(state) = &mut attached.mode {
                state.cursor.0 = state.cursor.0.min(rows - 1);
                state.cursor.1 = state.cursor.1.min(columns - 1);
            }
            // A pipe unit gets its size from the reopen below, which attaches with it.
            if attached.state == AttachState::Live && attached.interactive && !no_pty {
                effects.push(Effect::ResizePty { columns, rows });
            }
            self.resize_token += 1;
            effects.push(Effect::SettleResize {
                token: self.resize_token,
                after_ms: RESIZE_SETTLE_MS,
            });
        }
    }

    /// Whether the attached unit runs on pipes, as the daemon's `pty` says.
    fn attached_has_no_pty(&self) -> bool {
        self.attached_unit().is_some_and(|unit| !unit.pty)
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
        let project = self.view == View::Project;
        // Typing: every key goes to the unit but the ways back.
        if self.focus == Focus::Terminal && project {
            self.terminal_key(key, effects);
            return;
        }
        if project && self.filter.as_ref().is_some_and(|f| f.editing) {
            self.filter_key(key, effects);
            return;
        }
        if self.view == View::Daemon && self.daemon.copy.is_some() {
            let page = usize::from(self.size.1 / 2).max(1);
            self.daemon_copy_key(key, page, effects);
            return;
        }
        if self.view == View::Monitor && self.monitor.editing {
            self.monitor_filter_key(key, effects);
            return;
        }
        if self.view == View::Monitor && key.code == KeyCode::Esc && self.monitor_back() {
            return;
        }
        if let Some(attached) = self.attached.as_mut()
            && project
        {
            match &attached.mode {
                PaneMode::Copy(_) => {
                    self.copy_key(key, effects);
                    return;
                }
                PaneMode::Logs(_) if key.code == KeyCode::Esc => {
                    attached.mode = PaneMode::Live;
                    attached.screen.scroll_to_bottom();
                    return;
                }
                _ => {}
            }
        }
        if self.global_key(key, effects) {
            return;
        }
        match self.view {
            View::Monitor => self.monitor_key(key, effects),
            View::Daemon => self.daemon_key(key, effects),
            View::Project => self.list_key(key, effects),
        }
    }

    /// Keys that work on every screen while watching; true when `key` was one.
    fn global_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => effects.push(Effect::Quit),
            KeyCode::Char('q') if plain => effects.push(Effect::Quit),
            KeyCode::Char('u') if plain => {
                if !self.dismiss_release(effects) && self.first_run_strip() {
                    self.release.1.push(FIRST_RUN_KEY.into());
                    effects.push(Effect::DismissNotice(FIRST_RUN_KEY.into()));
                    self.resize_pane(effects);
                }
            }
            KeyCode::Tab => {
                self.learned.1 = true;
                self.step_project(1, effects);
            }
            KeyCode::BackTab => self.step_project(-1, effects),
            KeyCode::Char(digit @ '1'..='9') if plain => {
                let index = usize::from(digit as u8 - b'1');
                if let Some(name) = self.projects().get(index).cloned() {
                    self.learned.1 = true;
                    self.select_project(&name, effects);
                }
            }
            KeyCode::Char('M') => self.set_view(View::Monitor, effects),
            KeyCode::Char('D') => self.set_view(View::Daemon, effects),
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::Esc if self.view != View::Project => self.set_view(View::Project, effects),
            _ => return false,
        }
        true
    }

    /// Opens a view, or returns to the project when it is open already; the
    /// daemon view loads its facts and log at once.
    pub fn set_view(&mut self, view: View, effects: &mut Vec<Effect>) {
        let view = if view == self.view {
            View::Project
        } else {
            view
        };
        if view == View::Daemon && self.view != View::Daemon {
            self.daemon.ticks = 0;
            effects.push(Effect::ReadDaemonLog {
                from: self.daemon.log_position,
            });
            if !self.daemon.stopped {
                effects.push(Effect::FetchDaemon);
            }
        }
        if view == View::Monitor && self.view != View::Monitor {
            effects.push(Effect::FetchMetrics(self.metrics_query()));
        }
        self.view = view;
    }

    /// Keys while the list filter takes typing: text narrows the list, Enter
    /// keeps it, Esc clears it.
    fn filter_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let Some(filter) = self.filter.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.filter = None,
            KeyCode::Enter => {
                filter.editing = false;
                if filter.text.is_empty() {
                    self.filter = None;
                }
            }
            KeyCode::Backspace => {
                filter.text.pop();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                effects.push(Effect::Quit);
                return;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                filter.text.push(c);
            }
            KeyCode::Down | KeyCode::Up => {
                let delta = if key.code == KeyCode::Down { 1 } else { -1 };
                self.move_selection(delta, effects);
                return;
            }
            _ => return,
        }
        self.list_offset = 0;
        self.after_change(effects);
    }

    /// Keys on the daemon tab. Every action that stops something asks first.
    fn daemon_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = usize::from(self.size.1 / 2).max(1);
        let log_len = self.daemon.log.len();
        match key.code {
            // The bottom visible line; moving keeps the cursor on the last row.
            KeyCode::Char('c') if !ctrl && log_len > 0 => {
                self.daemon.copy = Some(LogCopy {
                    cursor: log_len.saturating_sub(1 + self.daemon.scroll),
                    anchor: None,
                });
            }
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
                "restart the daemon",
                "Restart the daemon?",
                "restart",
                "This TUI reconnects on its own.",
                Effect::RestartDaemon,
            ),
            // S stops the daemon, and starts it when this TUI stopped it.
            KeyCode::Char('S' | 's') if self.daemon.stopped => {
                if self.daemon.busy.is_none() {
                    self.daemon.busy = Some("starting");
                    effects.push(Effect::StartDaemon);
                }
            }
            KeyCode::Char('S') => self.daemon_prompt(
                "stop the daemon",
                "Stop the daemon?",
                "stop",
                "S on this page starts the daemon again.",
                Effect::StopDaemon,
            ),
            KeyCode::Char('X') => {
                if self.daemon.busy.is_none() {
                    self.dialog = Some(Dialog::Confirm {
                        title: "clean gone lanes".into(),
                        prompt: "Clean every lane whose worktree is gone?".into(),
                        detail: vec![
                            "For each one it stops the units, runs lane.clean tasks and infra teardown, removes the lane's containers and volumes and frees its ports. It also drops port offsets no config uses.".into(),
                        ],
                        action: "clean",
                        strict: true,
                        effect: Box::new(Effect::LaneGc),
                    });
                }
            }
            KeyCode::Char('s') => {
                if self.connection == Connection::Online {
                    self.set_notice("daemon already running".into());
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
    fn daemon_prompt(
        &mut self,
        title: &str,
        prompt: &str,
        action: &'static str,
        after: &str,
        effect: Effect,
    ) {
        if let Some(busy) = self.daemon.busy {
            self.set_notice(format!("wait: daemon {busy}…"));
            return;
        }
        if self.daemon.stopped {
            self.set_notice("daemon stopped: S starts it".into());
            return;
        }
        let processes = match self.daemon.managed_processes() {
            Some(0) => "Nothing is running.".to_owned(),
            Some(1) => "It stops 1 process. Start it again with devlanes lane resume.".to_owned(),
            Some(n) => {
                format!("It stops {n} processes. Start them again with devlanes lane resume.")
            }
            None => "It stops every process the daemon runs.".to_owned(),
        };
        self.dialog = Some(Dialog::Confirm {
            title: title.into(),
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
            self.set_notice(format!("already cleaning {reference}"));
            return;
        }
        let name = match &lane.branch {
            Some(branch) => format!("{} ({reference})", text::clean(branch)),
            None => reference.clone(),
        };
        self.dialog = Some(Dialog::Confirm {
            title: format!("clean {reference}"),
            prompt: format!("Clean {name}?"),
            detail: vec![
                "It stops every unit in the lane, then:".into(),
                "  runs its lane.clean tasks and infra teardown".into(),
                "  removes the lane's own containers and volumes".into(),
                "  frees its ports and setup records".into(),
                format!(
                    "Files in {} stay as they are.",
                    text::clean(&lane.path.display().to_string())
                ),
            ],
            action: "clean",
            strict: false,
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
                self.set_notice(format!("cleaning {lane}…"));
            }
            Effect::LaneGc => self.daemon.busy = Some("collecting lanes"),
            Effect::RestartDaemon => self.daemon.busy = Some("restarting"),
            Effect::StopDaemon => self.daemon.busy = Some("stopping"),
            _ => {}
        }
    }

    /// Ends typing: keys drive devlanes again.
    fn stop_typing(&mut self) {
        self.focus = Focus::List;
        self.want_terminal = false;
    }

    /// Starts typing into the pane, only onto a live stream. A watching
    /// stream is reopened as interactive first, so typing comes with the size.
    fn focus_terminal(&mut self, effects: &mut Vec<Effect>) {
        if self.view != View::Project {
            self.set_notice("the terminal is in the project view".into());
            return;
        }
        self.learned.0 = true;
        let Some(attached) = self.attached.as_ref() else {
            let notice = match self.selected_unit() {
                Some(unit) if is_orphaned(unit) => {
                    "no terminal: an earlier daemon started this process".into()
                }
                _ => "select a running unit first".into(),
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
                self.set_notice("output ended: it comes back when the unit runs again".into());
            }
            (AttachState::Failed(reason), _) => {
                let notice = format!("no terminal: {}", reason.summary);
                self.set_notice(notice);
            }
        }
    }

    /// Keys in copy mode on the daemon log: move, `v` selects, `y` copies the
    /// selected lines without their colors, Esc leaves.
    fn daemon_copy_key(&mut self, key: KeyEvent, page: usize, effects: &mut Vec<Effect>) {
        let last = self.daemon.log.len().saturating_sub(1);
        let Some(mut copy) = self.daemon.copy else {
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => copy.cursor = (copy.cursor + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => copy.cursor = copy.cursor.saturating_sub(1),
            KeyCode::PageDown => copy.cursor = (copy.cursor + page).min(last),
            KeyCode::PageUp => copy.cursor = copy.cursor.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => copy.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => copy.cursor = last,
            KeyCode::Char('v') | KeyCode::Char(' ') => {
                copy.anchor = match copy.anchor {
                    Some(_) => None,
                    None => Some(copy.cursor),
                };
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let (first, last) = copy.range();
                let text: Vec<String> = self
                    .daemon
                    .log
                    .range(first..=last)
                    .map(|line| text::clean(line))
                    .collect();
                effects.push(Effect::Copy(text.join("\n")));
                self.daemon.copy = None;
                self.set_notice("copied (OSC 52; some terminals block it)".into());
                return;
            }
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => {
                self.daemon.copy = None;
                return;
            }
            _ => {}
        }
        // The view ends at the selection's newest line, so both ends stay in sight
        // while it fits.
        self.daemon.scroll = last.saturating_sub(copy.range().1);
        self.daemon.copy = Some(copy);
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
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1, effects),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1, effects),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2, effects),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2, effects),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(effects),
            KeyCode::Left | KeyCode::Char('h') => self.fold_or_parent(effects),
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(id) = self.selected_id().cloned()
                    && self
                        .rows()
                        .iter()
                        .any(|row| row.id == id && row.folded == Some(true))
                {
                    self.toggle_fold(&id, effects);
                }
            }
            KeyCode::Char('[') => self.step_lane(-1, effects),
            KeyCode::Char(']') => self.step_lane(1, effects),
            KeyCode::Char('n') => self.next_problem(true, effects),
            KeyCode::Char('N') => self.next_problem(false, effects),
            KeyCode::Char('/') => {
                let text = self.filter.take().map(|f| f.text).unwrap_or_default();
                self.filter = Some(Filter {
                    text,
                    editing: true,
                });
            }
            // Shift+C: Option chords become characters in many macOS terminals.
            KeyCode::Char('C') => self.clean_prompt(),
            KeyCode::Char('s') => self.control(Method::Start, effects),
            KeyCode::Char('x') => self.control(Method::Stop, effects),
            KeyCode::Char('r') => self.control(Method::Restart, effects),
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
            KeyCode::PageUp => {
                let page = self.page();
                self.scroll(page, effects);
            }
            KeyCode::PageDown => {
                let page = self.page();
                self.scroll(-page, effects);
            }
            // One step back: the kept filter, then a scrolled pane.
            KeyCode::Esc => {
                if self.filter.take().is_some() {
                    self.after_change(effects);
                } else if let Some(attached) = self.attached.as_mut() {
                    attached.screen.scroll_to_bottom();
                }
            }
            _ => {}
        }
    }

    /// Enter and Space: fold a header, type into a running unit.
    fn activate(&mut self, effects: &mut Vec<Effect>) {
        let Some(id) = self.selected_id().cloned() else {
            return;
        };
        if matches!(id, RowId::Section(_) | RowId::Group(..)) {
            self.toggle_fold(&id, effects);
        } else if self.selected_unit().is_some() {
            // A unit that does not run says why in a notice.
            self.focus_terminal(effects);
        }
    }

    /// Whether `key` ends typing: Ctrl+] always (crossterm reports the 0x1d
    /// byte as Ctrl+5 without enhanced key codes), Esc unless the program
    /// uses the alternate screen and wants Esc itself.
    fn leaves_typing(&self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(']' | '5') if ctrl => true,
            KeyCode::Esc => !self.full_screen(),
            _ => false,
        }
    }

    /// The attached program draws on the alternate screen (vim, less).
    pub fn full_screen(&self) -> bool {
        self.attached
            .as_ref()
            .is_some_and(|a| a.screen.screen().alternate_screen())
    }

    /// Keys while typing go to the unit, except the ways back. With no live
    /// stream they are dropped, never handed to list shortcuts.
    fn terminal_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        if self.leaves_typing(&key) {
            self.stop_typing();
            return;
        }
        let modes = self.modes();
        let Some(attached) = self.attached.as_mut() else {
            self.focus = Focus::List;
            return;
        };
        if attached.state != AttachState::Live {
            self.set_notice("not live: esc goes back to the list".into());
            return;
        }
        // Typing returns to the live screen, like a real terminal.
        attached.mode = PaneMode::Live;
        attached.screen.scroll_to_bottom();
        if let Some(bytes) = input::encode_key(&key, modes) {
            effects.push(Effect::Input(bytes));
        }
    }

    /// Paste goes to the unit in terminal focus.
    fn paste(&mut self, text: &str, effects: &mut Vec<Effect>) {
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
            // A stray Enter must not stop or remove anything.
            Dialog::Confirm { strict, effect, .. }
                if matches!(key.code, KeyCode::Char('y' | 'Y'))
                    || (key.code == KeyCode::Enter && !strict) =>
            {
                self.begin(&effect);
                effects.push(*effect);
            }
            Dialog::Confirm { .. }
                if matches!(key.code, KeyCode::Char('n' | 'q') | KeyCode::Esc) => {}
            dialog @ Dialog::Confirm { .. } => self.dialog = Some(dialog),
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
                self.api_error(&format!("load profiles of {lane}"), None, error, effects);
            }
        }
    }

    /// What the monitor asks `metrics` for: its range, in about as many
    /// buckets as the widest graph has samples.
    pub fn metrics_query(&self) -> MetricsQuery {
        MetricsQuery {
            range_ms: self.monitor.range().0,
            points: usize::from(self.size.0).clamp(60, 400),
        }
    }

    /// Keys on the monitor.
    fn monitor_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let rows = self.monitor_rows();
        let page = usize::from(self.size.1 / 2).max(1);
        let index = self.monitor.index;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.monitor.select_index(&rows, index + 1),
            KeyCode::Char('k') | KeyCode::Up => {
                self.monitor.select_index(&rows, index.saturating_sub(1));
            }
            KeyCode::PageDown => self.monitor.select_index(&rows, index + page),
            KeyCode::PageUp => self.monitor.select_index(&rows, index.saturating_sub(page)),
            KeyCode::Char('g') | KeyCode::Home => self.monitor.select_index(&rows, 0),
            KeyCode::Char('G') | KeyCode::End => self.monitor.select_index(&rows, usize::MAX),
            KeyCode::Char('<') => self.monitor.sort = self.monitor.sort.step(-1),
            KeyCode::Char('>') => self.monitor.sort = self.monitor.sort.step(1),
            KeyCode::Char('i') => self.monitor.reverse = !self.monitor.reverse,
            KeyCode::Char('e') => self.monitor.flat = !self.monitor.flat,
            KeyCode::Char('/') => self.monitor.editing = true,
            KeyCode::Char('z') => self.monitor.zoom = !self.monitor.zoom,
            KeyCode::Char('+' | '=') | KeyCode::Char('-') => {
                let last = crate::monitor::RANGES.len() - 1;
                self.monitor.range = match key.code {
                    KeyCode::Char('-') => self.monitor.range.saturating_sub(1),
                    _ => (self.monitor.range + 1).min(last),
                };
                self.set_notice(format!("range {}", self.monitor.range().1));
                effects.push(Effect::FetchMetrics(self.metrics_query()));
            }
            KeyCode::Char('h') | KeyCode::Left => match rows.get(index) {
                Some(MonitorRow::Group(group)) => {
                    self.monitor.folded.insert(group.key.clone());
                }
                Some(MonitorRow::Entry(entry)) if !self.monitor.flat => {
                    self.monitor.selected = Some(MonitorId::Group(entry.lane.clone()));
                }
                _ => {}
            },
            KeyCode::Char('l') | KeyCode::Right => {
                if let Some(MonitorRow::Group(group)) = rows.get(index) {
                    self.monitor.folded.remove(&group.key);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => match rows.get(index) {
                Some(MonitorRow::Group(group)) => {
                    if !self.monitor.folded.remove(&group.key) {
                        self.monitor.folded.insert(group.key.clone());
                    }
                }
                Some(MonitorRow::Entry(entry)) => self.open_from_monitor(entry, effects),
                None => {}
            },
            KeyCode::Char('s') => self.monitor_control(Method::Start, &rows, effects),
            KeyCode::Char('x') => self.monitor_control(Method::Stop, &rows, effects),
            KeyCode::Char('r') => self.monitor_control(Method::Restart, &rows, effects),
            _ => {}
        }
        self.clamp_monitor();
    }

    /// Esc on the monitor steps back one thing at a time: the detail zoom,
    /// then a kept filter; true when it did one of them.
    fn monitor_back(&mut self) -> bool {
        if self.monitor.zoom {
            self.monitor.zoom = false;
        } else if !self.monitor.filter.is_empty() {
            self.monitor.filter.clear();
            self.clamp_monitor();
        } else {
            return false;
        }
        true
    }

    /// Enter on a unit row: its project, lane and row, with its terminal.
    fn open_from_monitor(&mut self, entry: &crate::monitor::Entry, effects: &mut Vec<Effect>) {
        if entry.is_container() {
            self.set_notice(format!(
                "{} is a container: it has no terminal",
                entry.label
            ));
            return;
        }
        if !self.lanes.iter().any(|l| l.reference == entry.lane) {
            return;
        }
        self.view = View::Project;
        self.select(Selected::unit(&entry.lane, &entry.unit), effects);
    }

    /// `s`, `x` and `r` on the monitor: a unit row acts on the unit, a lane
    /// row on the whole lane. Containers start and stop with their lane.
    fn monitor_control(&mut self, method: Method, rows: &[MonitorRow], effects: &mut Vec<Effect>) {
        let verb = method_verb(method);
        match rows.get(self.monitor.index) {
            Some(MonitorRow::Entry(entry)) if entry.is_container() => {
                self.set_notice(format!(
                    "{} starts and stops with its lane; select the lane row",
                    entry.label
                ));
            }
            Some(MonitorRow::Entry(entry)) => {
                let active = entry.summary.as_ref().is_some_and(|u| is_active(&u.state));
                let reference = unit_ref(&entry.lane, &entry.unit);
                if method == Method::Start && active {
                    self.set_notice(format!("{reference} already running"));
                    return;
                }
                self.pending_begin(&entry.lane);
                effects.push(Effect::Control {
                    method,
                    lane: entry.lane.clone(),
                    selection: devlanes_proto::Selection {
                        units: vec![reference.clone()],
                        ..Default::default()
                    },
                    label: format!("{verb} {reference}"),
                    target: Target::Unit {
                        lane: entry.lane.clone(),
                        unit: entry.unit.clone(),
                    },
                });
            }
            Some(MonitorRow::Group(group)) if !group.shared => {
                self.pending_begin(&group.key);
                effects.push(Effect::Control {
                    method,
                    lane: group.key.clone(),
                    selection: devlanes_proto::Selection::default(),
                    label: format!("{verb} {}", group.key),
                    target: Target::Lane(group.key.clone()),
                });
            }
            Some(MonitorRow::Group(group)) => {
                self.set_notice(format!("{} starts and stops with its lanes", group.key));
            }
            None => {}
        }
    }

    /// Keys while the monitor filter takes typing.
    fn monitor_filter_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        match key.code {
            KeyCode::Esc => {
                self.monitor.filter.clear();
                self.monitor.editing = false;
            }
            KeyCode::Enter => self.monitor.editing = false,
            KeyCode::Backspace => {
                self.monitor.filter.pop();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                effects.push(Effect::Quit);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.monitor.filter.push(c);
            }
            _ => {}
        }
        self.clamp_monitor();
    }

    // ---- actions ----------------------------------------------------------

    /// `s`, `x` and `r` on the selected row: the whole lane, a section or
    /// group (its units), one unit, or a task (`s` and `r` run it).
    fn control(&mut self, method: Method, effects: &mut Vec<Effect>) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let reference = lane.reference.clone();
        let Some(row) = self.selected_id().cloned() else {
            return;
        };
        let verb = method_verb(method);
        let task = match &row {
            RowId::Task(name) => Some(name.clone()),
            RowId::Unit(name)
                if lane
                    .units
                    .iter()
                    .any(|u| u.name == *name && tree::role(u) == tree::Role::Task) =>
            {
                Some(name.clone())
            }
            _ => None,
        };
        if let Some(task) = task {
            let running = lane
                .units
                .iter()
                .any(|u| u.name == task && is_active(&u.state));
            match method {
                Method::Stop if running => {}
                Method::Stop => {
                    self.set_notice(format!("{task} is not running"));
                    return;
                }
                _ => {
                    self.run_task(reference, task, effects);
                    return;
                }
            }
        }
        let (units, target, error_target) = match &row {
            // A start of what already runs would only come back as `already_running`.
            RowId::Unit(name)
                if method == Method::Start
                    && lane
                        .units
                        .iter()
                        .any(|u| u.name == *name && is_active(&u.state)) =>
            {
                self.set_notice(format!("{} already running", unit_ref(&reference, name)));
                return;
            }
            RowId::Unit(name) => {
                let unit = unit_ref(&reference, name);
                let error = Target::Unit {
                    lane: reference.clone(),
                    unit: name.clone(),
                };
                (vec![unit.clone()], unit, error)
            }
            RowId::Section(_) | RowId::Group(..) => {
                let nodes = self.nodes();
                let members = tree::headers(&nodes)
                    .into_iter()
                    .find(|node| node.id == row)
                    .map(Node::leaves)
                    .unwrap_or_default();
                // Start sends every unit; stop and restart only what runs.
                let units: Vec<String> = members
                    .iter()
                    .filter_map(|id| match id {
                        RowId::Unit(name) => lane.units.iter().find(|u| u.name == *name),
                        _ => None,
                    })
                    .filter(|u| matches!(tree::role(u), tree::Role::Service | tree::Role::Exec))
                    .filter(|u| (method == Method::Start) != is_active(&u.state))
                    .map(|u| unit_ref(&reference, &u.name))
                    .collect();
                if units.is_empty() {
                    self.set_notice(match method {
                        Method::Start => "everything here is already running".into(),
                        _ => "nothing running here".into(),
                    });
                    return;
                }
                let count = units.len();
                let what = match count {
                    1 => units[0].clone(),
                    n => format!("{n} units in {reference}"),
                };
                (units, what, Target::Lane(reference.clone()))
            }
            RowId::Container(_) => {
                self.set_notice(
                    "containers follow the lane: s on the whole lane starts them".into(),
                );
                return;
            }
            // Restart names units explicitly (the CLI requires them); start and stop act on the lane.
            RowId::WholeLane if method == Method::Restart => {
                let units: Vec<String> = lane
                    .units
                    .iter()
                    .filter(|u| tree::role(u) == tree::Role::Service && is_active(&u.state))
                    .map(|u| unit_ref(&reference, &u.name))
                    .collect();
                if units.is_empty() {
                    self.set_notice(format!("nothing running in {reference}"));
                    return;
                }
                (units, reference.clone(), Target::Lane(reference.clone()))
            }
            RowId::WholeLane | RowId::Task(_) => (
                Vec::new(),
                reference.clone(),
                Target::Lane(reference.clone()),
            ),
            RowId::Gap(_) => return,
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

    /// Runs a task; asks first only when its config sets `confirm`, as
    /// `devlanes run` does.
    fn run_task(&mut self, lane: String, task: String, effects: &mut Vec<Effect>) {
        let confirm = self
            .task_info(&lane, &task)
            .is_some_and(|info| info.confirm);
        let effect = Effect::Run {
            lane: lane.clone(),
            params: RunParams {
                task: task.clone(),
                env: BTreeMap::new(),
                // The TUI shows the task in its pane instead of streaming the request.
                detach: true,
                terminal: true,
            },
            // Only a confirmed dialog answers the task's confirm.
            yes: confirm,
        };
        if confirm {
            self.dialog = Some(Dialog::Confirm {
                title: format!("run {task}"),
                prompt: format!("Run {task} in {lane}?"),
                detail: vec!["Its config asks for a confirmation first.".into()],
                action: "run",
                strict: false,
                effect: Box::new(effect),
            });
            return;
        }
        self.begin(&effect);
        effects.push(effect);
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
                self.set_notice(format!("started {task} in {lane}"));
                self.select(Selected::unit(&lane, &task), effects);
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
            None => {
                let notice = match (self.selected_unit(), self.selected_lane()) {
                    (Some(unit), _) => format!("{} has no url", unit.label()),
                    (None, Some(lane)) => format!("no unit in {} has a url", lane.reference),
                    (None, None) => "no url".into(),
                };
                self.set_notice(notice);
            }
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
        let what = format!("load logs of {}", unit_ref(&attached.lane, &attached.unit));
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
            self.set_notice("select a unit with output first".into());
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

    /// Clicks on tabs, chips, hints and rows; the wheel scrolls; a click in the
    /// pane types into the unit and a drag there selects text.
    fn mouse(&mut self, mouse: MouseEvent, effects: &mut Vec<Effect>) {
        let press = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let position: Position = (mouse.column, mouse.row).into();
        if self.dialog.is_some() {
            // A click outside a dialog closes it; nothing it guards happens.
            let inside = self
                .dialog_area
                .borrow()
                .is_some_and(|rect| rect.contains(position));
            if press && !inside {
                self.dialog = None;
            }
            return;
        }
        let Some(layout) = self.areas() else {
            return;
        };
        if press {
            let click = self
                .hits
                .borrow()
                .iter()
                .find(|(rect, _)| rect.contains(position))
                .map(|(_, click)| click.clone());
            if let Some(click) = click {
                self.stop_typing();
                self.click(click, effects);
                return;
            }
        }
        match self.view {
            View::Project => self.project_mouse(mouse, &layout, effects),
            View::Monitor => self.monitor_mouse(mouse, &layout, effects),
            View::Daemon => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.daemon.scroll = (self.daemon.scroll + 3).min(self.daemon.log.len());
                }
                MouseEventKind::ScrollDown => {
                    self.daemon.scroll = self.daemon.scroll.saturating_sub(3);
                }
                _ => {}
            },
        }
    }

    /// What a click on a chrome item does.
    fn click(&mut self, click: Click, effects: &mut Vec<Effect>) {
        match click {
            Click::Project(name) => {
                self.learned.1 = true;
                self.select_project(&name, effects);
            }
            Click::Lane(lane) => {
                self.view = View::Project;
                self.select_lane(&lane, effects);
            }
            Click::StartLane(lane) => {
                self.view = View::Project;
                self.select(Selected::lane(&lane), effects);
            }
            Click::FirstProblem => {
                self.view = View::Project;
                if let Some(first) = self.problems().into_iter().next() {
                    self.select(first, effects);
                }
            }
            Click::Key(code, modifiers) => self.key(KeyEvent::new(code, modifiers), effects),
            Click::MonitorRow(id) => {
                // A second click on the selected row is Enter: open or fold it.
                if self.monitor.selected.as_ref() == Some(&id) {
                    self.monitor_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), effects);
                } else {
                    self.monitor.selected = Some(id);
                    self.clamp_monitor();
                }
            }
        }
    }

    /// The monitor: the wheel moves the selection three rows; clicks on rows
    /// come through their hit areas.
    fn monitor_mouse(&mut self, mouse: MouseEvent, layout: &Areas, effects: &mut Vec<Effect>) {
        let _ = (layout, effects);
        let rows = self.monitor_rows();
        let index = self.monitor.index;
        match mouse.kind {
            MouseEventKind::ScrollUp => self.monitor.select_index(&rows, index.saturating_sub(3)),
            MouseEventKind::ScrollDown => self.monitor.select_index(&rows, index + 3),
            _ => {}
        }
    }

    /// The list and the pane of the project view.
    fn project_mouse(&mut self, mouse: MouseEvent, layout: &Areas, effects: &mut Vec<Effect>) {
        let position: Position = (mouse.column, mouse.row).into();
        let layout = *layout;
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
            let cell = (
                mouse.row - layout.pane_inner.y,
                mouse.column - layout.pane_inner.x,
            );
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll(WHEEL_LINES, effects),
                MouseEventKind::ScrollDown => self.scroll(-WHEEL_LINES, effects),
                MouseEventKind::Down(MouseButton::Left) if self.focus == Focus::List => {
                    self.pane_press = Some(cell);
                }
                // A drag selects text like copy mode; the release copies it.
                MouseEventKind::Drag(MouseButton::Left) if self.focus == Focus::List => {
                    if let (Some(anchor), Some(attached)) =
                        (self.pane_press, self.attached.as_mut())
                        && matches!(attached.mode, PaneMode::Live | PaneMode::Copy(_))
                    {
                        attached.mode = PaneMode::Copy(CopyState {
                            cursor: cell,
                            anchor: Some(anchor),
                        });
                    }
                }
                MouseEventKind::Up(MouseButton::Left) if self.focus == Focus::List => {
                    let dragged = self.attached.as_ref().is_some_and(|a| {
                        matches!(
                            a.mode,
                            PaneMode::Copy(CopyState {
                                anchor: Some(_),
                                ..
                            })
                        )
                    }) && self.pane_press.is_some_and(|press| press != cell);
                    let pressed = self.pane_press.take().is_some();
                    if dragged {
                        self.copy_key(
                            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                            effects,
                        );
                    } else if pressed && self.selected_unit().is_some_and(|u| is_active(&u.state)) {
                        if let Some(attached) = self.attached.as_mut() {
                            attached.mode = PaneMode::Live;
                        }
                        self.focus_terminal(effects);
                    }
                }
                _ => {}
            }
            return;
        }
        let Some(list) = layout.list.filter(|list| list.contains(position)) else {
            return;
        };
        match mouse.kind {
            // The wheel scrolls the list; the selection stays.
            MouseEventKind::ScrollUp => self.scroll_list(-WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_list(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left) => {
                self.stop_typing();
                let index = self.list_offset + usize::from(mouse.row - list.y);
                let rows = self.rows();
                let Some(row) = rows.get(index).cloned() else {
                    return;
                };
                let now = std::time::Instant::now();
                let double = self.last_click.as_ref().is_some_and(|(at, id)| {
                    *id == row.id && now.duration_since(*at).as_millis() <= DOUBLE_CLICK_MS
                });
                self.last_click = Some((now, row.id.clone()));
                let selected = self.selected_id() == Some(&row.id);
                // Like Enter: a click on the arrow, or on a header already
                // selected, folds or unfolds it.
                let arrow = list.x + row_indent(row.depth);
                let on_arrow = mouse.column >= arrow && mouse.column < arrow + 2;
                self.select_row(row.id.clone(), effects);
                if row.folded.is_some() && (selected || on_arrow) {
                    self.toggle_fold(&row.id, effects);
                } else if double {
                    // A third click is a new first one, not another double.
                    self.last_click = None;
                    self.activate(effects);
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
        let online = self.connection == Connection::Online;
        if self.view == View::Monitor && online {
            effects.push(Effect::FetchMetrics(self.metrics_query()));
        }
        // Phase timings and the spinner on terminals that only redraw per tick.
        self.spin = self.spin.wrapping_add(1);
        for (lane, phases) in &mut self.phases {
            let starting = self
                .lanes
                .iter()
                .any(|l| l.reference == *lane && l.state == "starting");
            if starting {
                phases.seconds += 1;
                if let Some(step) = phases
                    .steps
                    .iter_mut()
                    .rev()
                    .find(|step| step.state == "started")
                {
                    step.seconds += 1;
                }
            }
        }
        // The daemon button's `!` comes from doctor: refreshed now and then
        // in the background, since doctor asks Docker for its version.
        if self.view != View::Daemon && online && self.daemon.busy.is_none() {
            self.daemon.background = self.daemon.background.saturating_sub(1);
            if self.daemon.background == 0 {
                self.daemon.background = DOCTOR_BACKGROUND_TICKS;
                if !self.daemon.stopped {
                    effects.push(Effect::FetchDaemon);
                }
            }
        }
        if self.view == View::Daemon {
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
                format!("daemon {daemon}, TUI {own}: restart the daemon to match (stops units)"),
            )),
            _ => None,
        };
        let notice = notice.or_else(|| {
            let latest = status.pointer("/update/latest").and_then(Value::as_str)?;
            if !devlanes_proto::release::is_newer(latest, own) {
                return None;
            }
            let command = self.release.0.as_ref()?.upgrade_advice(latest);
            Some((
                update::update_key(latest),
                format!("{latest} available: {command}"),
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

    /// Rows of the monitor table: from metrics when the daemon sent them,
    /// else the running units of lane status.
    pub fn monitor_rows(&self) -> Vec<MonitorRow> {
        crate::monitor::rows(self.metrics.as_ref(), &self.lanes, &self.monitor)
    }

    /// Puts the monitor's selection back on its row after the rows changed.
    fn clamp_monitor(&mut self) {
        let rows = self.monitor_rows();
        self.monitor.settle(&rows);
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

/// `rows` with a blank row before each section, as the mockups draw them.
fn with_gaps(rows: Vec<Row>) -> Vec<Row> {
    let mut out = Vec::with_capacity(rows.len() + 3);
    let mut gaps = 0u8;
    for row in rows {
        if matches!(row.id, RowId::Section(_)) {
            out.push(Row {
                id: RowId::Gap(gaps),
                depth: 0,
                folded: None,
                members: Vec::new(),
            });
            gaps += 1;
        }
        out.push(row);
    }
    out
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
        (None, Some(code)) => Worded::local(&words::exited_with(code)),
        (None, None) => Worded::local("never became ready"),
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

/// The notice for a finished action: `restarted acme@0:api`, or `acme@0:api
/// already running` when a start started nothing because it all ran.
fn done_notice(label: &str, value: &Value) -> String {
    let list = |key: &str| value.get(key).and_then(Value::as_array).map(Vec::len);
    let (verb, rest) = label.split_once(' ').unwrap_or((label, ""));
    if verb == "start" && list("started") == Some(0) && list("already_running").unwrap_or(0) > 0 {
        let what = rest.split(" with ").next().unwrap_or(rest);
        return format!("{what} already running");
    }
    let done = match verb {
        "start" => "started",
        "stop" => "stopped",
        "restart" => "restarted",
        other => return format!("{other} {rest}: done"),
    };
    format!("{done} {rest}")
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
