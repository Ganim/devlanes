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
    /// A lane chip: the lane's status.
    Lane(String),
    /// The start in the top bar, or the list's title: the lane's status.
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
    /// Shown entries scrolled up from the newest.
    pub scroll: usize,
    /// Not following the file: arriving lines leave the view where it is.
    pub paused: bool,
    /// Lines that arrived while paused, for the title.
    pub new_lines: usize,
    /// Level floor, text filter and folding of the log.
    pub filters: crate::daemonlog::Filters,
    /// Keys go to the text filter until Enter or Esc.
    pub filter_editing: bool,
    /// `i`: every check, not only the problems.
    pub all_checks: bool,
    /// Copy mode on the log, over the shown entries.
    pub copy: Option<LogCopy>,
    /// Where a left button press in the log was, until its release.
    press: Option<crate::daemonlog::LogPos>,
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

/// The cursor and selection of copy mode on the daemon log, a cell of the
/// log as drawn, like copy mode in a unit's pane. Entries index the shown
/// entries (0 is the oldest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCopy {
    pub cursor: crate::daemonlog::LogPos,
    /// Where `v` started the selection.
    pub anchor: Option<crate::daemonlog::LogPos>,
}

impl LogCopy {
    /// The selection, first to last cell inclusive; the cursor alone before `v`.
    pub fn range(self) -> (crate::daemonlog::LogPos, crate::daemonlog::LogPos) {
        let anchor = self.anchor.unwrap_or(self.cursor);
        (anchor.min(self.cursor), anchor.max(self.cursor))
    }

    /// What `y` copies: the selection, or the cursor's whole line before `v`.
    pub fn copied(self) -> (crate::daemonlog::LogPos, crate::daemonlog::LogPos) {
        if self.anchor.is_some() {
            return self.range();
        }
        let line = |column| crate::daemonlog::LogPos {
            column,
            ..self.cursor
        };
        (line(0), line(usize::MAX))
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
    /// The Esc menu (`crate::menu`).
    Menu(crate::menu::Menu),
    /// The options page of the Esc menu (`crate::settings`).
    Settings(Box<crate::settings::Settings>),
    /// q while lanes of the project run: keep them, stop them or stay.
    Quit(QuitDialog),
    /// `T` on the daemon page: the log's targets to show.
    Targets(TargetPicker),
    /// The first launch in a home (`crate::welcome`).
    Welcome(crate::welcome::Welcome),
}

/// The `T` picker: every target of the kept log lines with its count and
/// whether it is checked; none checked shows them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPicker {
    pub items: Vec<(String, usize, bool)>,
    pub index: usize,
}

/// What quitting does with running lanes: `tui.on_quit` from the user config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnQuit {
    #[default]
    Ask,
    Keep,
    Stop,
}

impl OnQuit {
    /// The config word; anything else asks.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("keep") => Self::Keep,
            Some("stop") => Self::Stop,
            _ => Self::Ask,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Keep => "keep",
            Self::Stop => "stop",
        }
    }
}

/// The quit dialog: the running lanes it would stop, the highlighted choice
/// and the "remember this choice" box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitDialog {
    pub lanes: Vec<String>,
    /// [`QUIT_CHOICES`] index.
    pub index: usize,
    pub remember: bool,
}

/// The quit dialog's choices, in order.
pub const QUIT_CHOICES: [&str; 3] = [
    "Close the TUI; lanes keep running",
    "Close and stop the running lanes",
    "Cancel",
];

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
    /// The frame drawn while a resize or zoom waits for the reopened stream:
    /// the last one before it, so the pane goes from that frame straight to
    /// the daemon's redraw at the new size, never through a locally cut grid
    /// or a blank one.
    pub held: Option<vt100::Screen>,
}

impl Attached {
    /// The screen the pane shows and keys are encoded for: the held frame
    /// while there is one, else the live grid.
    pub fn shown(&self) -> &vt100::Screen {
        self.held.as_ref().unwrap_or_else(|| self.screen.screen())
    }
}

/// The PTY is never made smaller than this, whatever the pane size.
pub const MIN_PTY: (u16, u16) = (20, 5);
/// A resize settles this long before the stream is reopened for a fresh screen.
const RESIZE_SETTLE_MS: u64 = 200;

/// Inputs to [`App::update`].
#[derive(Debug)]
pub enum Msg {
    /// [`Effect::QuitWith`] is done. With an `error` (a stop or the config
    /// write failed) the TUI stays open to show it.
    QuitReady {
        error: Option<String>,
    },
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    /// A frame of the start this TUI was opened for (`devlanes start`).
    Start(StartFrame),
    /// Outer terminal size.
    Resize(u16, u16),
    /// One second passed.
    Tick,
    /// The terminal gained (`true`) or lost focus.
    Focus(bool),
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
    /// config.yaml as read for the options page.
    Settings(crate::settings::Reply),
}

/// Side effects the runtime performs; results come back as [`Msg`].
#[derive(Debug)]
pub enum Effect {
    /// Writes `tui.on_quit` when `remember` is set, stops `stop`'s lanes,
    /// then sends [`Msg::QuitReady`]. Quitting waits for it, so the stops
    /// reach the daemon before the process ends.
    QuitWith {
        stop: Vec<String>,
        remember: Option<OnQuit>,
    },
    Refresh,
    Subscribe,
    /// A desktop notification, asked for only while the terminal is not
    /// focused; the runtime drops it where notifications are off and when
    /// the last one is too recent.
    Notify {
        title: String,
        body: String,
    },
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
    /// Reads or edits config.yaml for the options page.
    Settings(crate::settings::Request),
    Quit,
}

/// Screen regions shared by rendering, mouse hit-testing and PTY sizing: the
/// top bar, the lane bar, the list and pane boxes, the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    /// Row 0: devlanes, project tabs, messages, the view buttons.
    pub header: Rect,
    /// Row 1 in the project view when the project has several lanes.
    pub lane_bar: Option<Rect>,
    /// The list's box, its title in the top border, and its rows inside.
    pub list_box: Option<Rect>,
    pub list: Option<Rect>,
    /// The pane's box; the unit's name and state are its title.
    pub pane_box: Rect,
    /// The pane's content: the unit's terminal or the details.
    pub pane_inner: Rect,
    /// Everything between the header and the footer, for the monitor and daemon views.
    pub body: Rect,
    pub footer: Rect,
}

/// Splits the screen, or `None` when it is below the minimum size.
pub fn areas(width: u16, height: u16, zoom: bool) -> Option<Areas> {
    areas_with(width, height, zoom, true)
}

/// [`areas`] with the lane bar only when `lane_bar`.
pub fn areas_with(width: u16, height: u16, zoom: bool, lane_bar: bool) -> Option<Areas> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }
    let header = Rect::new(0, 0, width, 1);
    let footer = Rect::new(0, height - 1, width, 1);
    let bottom = height - 1;
    let body = Rect::new(0, 1, width, bottom - 1);
    // A project with one lane has nothing to switch to: no lane bar.
    let lane_bar = lane_bar.then(|| Rect::new(0, 1, width, 1));
    // Two boxes under the lane bar, as the navigation.md mockups draw them.
    let top = 1 + u16::from(lane_bar.is_some());
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
    pub connection: Connection,
    pub dialog: Option<Dialog>,
    pub attached: Option<Attached>,
    /// The latest `metrics` reply; `None` until one arrives or after an error.
    pub metrics: Option<Metrics>,
    pub monitor: MonitorView,
    /// The monitor graphs' finished columns and sticky scales.
    pub graphs: crate::graph::Columns,
    /// Seconds the widest monitor graph showed in the last frame (one per
    /// column); the next `metrics` call asks for that much history.
    pub monitor_seconds: std::cell::Cell<usize>,
    /// Rows the monitor table showed in the last frame, so keys keep the
    /// selection in view and the wheel stops at the last row.
    pub monitor_table_rows: std::cell::Cell<usize>,
    /// Rows the daemon log box showed in the last frame, as drawn.
    pub daemon_log_view: std::cell::Cell<crate::daemonlog::LogView>,
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
    /// A resize waits for its settle timer; output meanwhile keeps the held frame.
    resize_settling: bool,
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
    /// `tui.on_quit`; the quit dialog's "remember" box updates it.
    pub on_quit: OnQuit,
    /// Where daemon.log times are shown: UTC until the runtime sets the
    /// system's zone, so tests draw the same everywhere.
    pub time_zone: jiff::tz::TimeZone,
    /// This TUI's version: the menu shows it and the release notice compares
    /// it with the daemon's. The screens example pins it, so a release does
    /// not change the committed screens.
    pub version: &'static str,
    /// The per-user service definition (`Options::service`), merged into
    /// doctor facts for the stop-window check.
    pub service_report: Option<serde_json::Value>,
    /// The user's home, so paths under it show as `~/…`; `None` shows them whole.
    pub user_home: Option<PathBuf>,
    /// [`Effect::QuitWith`] runs; q again quits at once.
    quitting: bool,
    /// The terminal has focus, as its focus reports say; assumed until one
    /// says otherwise, so a terminal that never reports never notifies.
    pub focused: bool,
    /// A start must run this long before its end is worth a notification.
    pub notify_after: std::time::Duration,
    /// Each lane's state when last seen, and since when it is starting.
    watched: HashMap<String, (String, Option<std::time::Instant>)>,
    /// Each service's state when last seen, by `(lane, unit)`.
    watched_units: HashMap<(String, String), String>,
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
            connection: Connection::Connecting,
            dialog: None,
            attached: None,
            metrics: None,
            monitor: MonitorView::default(),
            graphs: crate::graph::Columns::default(),
            monitor_seconds: std::cell::Cell::new(0),
            monitor_table_rows: std::cell::Cell::new(0),
            daemon_log_view: std::cell::Cell::default(),
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
            resize_settling: false,
            last_metrics_error: None,
            notice_ticks: 0,
            generation: 0,
            reconnect_pending: false,
            starts: StartTracker::viewer(),
            start_error_ticks: 0,
            start_run: None,
            on_quit: OnQuit::Ask,
            time_zone: jiff::tz::TimeZone::UTC,
            version: devlanes_version::VERSION,
            service_report: None,
            user_home: None,
            quitting: false,
            focused: true,
            notify_after: devlanes_term::LONG_START,
            watched: HashMap::new(),
            watched_units: HashMap::new(),
        }
    }

    /// The URLs worth a click on screen: the first of each unit of the lane
    /// shown, as the pane header, its details and section lists print them.
    pub fn link_targets(&self) -> Vec<String> {
        if matches!(self.dialog, Some(Dialog::Welcome(_))) {
            return vec![crate::welcome::DOCS_URL.into()];
        }
        self.selected_lane()
            .map(|lane| {
                lane.units
                    .iter()
                    .filter_map(|unit| unit.urls.first())
                    .map(|url| text::clean(url))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The tab title: `devlanes · acme@0`, with how the lane stands when that
    /// is worth a glance (`starting 3/8`, `stopping`, `cleaning`, `2 failed`,
    /// `reconnecting`).
    pub fn tab_title(&self) -> String {
        let Some(lane) = self.selected_lane() else {
            return "devlanes".into();
        };
        let mut title = format!("devlanes · {}", lane.reference);
        let counts = tree::counts(lane);
        let state = if matches!(self.connection, Connection::Reconnecting { .. }) {
            Some("reconnecting".to_owned())
        } else if lane.state == "starting" {
            Some(if counts.total > 0 {
                format!("starting {}/{}", counts.up, counts.total)
            } else {
                "starting".into()
            })
        } else if lane.state == "stopping" {
            Some("stopping".into())
        } else if self.cleaning.contains(&lane.reference) {
            Some("cleaning".into())
        } else {
            (counts.failed > 0).then(|| format!("{} failed", counts.failed))
        };
        if let Some(state) = state {
            title.push_str(" · ");
            title.push_str(&state);
        }
        title
    }

    /// Compares every lane and service with how it was last seen, and asks
    /// for a notification while the terminal is not focused when a start
    /// that ran at least [`App::notify_after`] ended, or a running service
    /// failed outside a start.
    fn watch(&mut self, effects: &mut Vec<Effect>) {
        let now = std::time::Instant::now();
        let mut notes = Vec::new();
        let mut seen_units = HashMap::new();
        for lane in &self.lanes {
            let before = self.watched.get(&lane.reference);
            let was_starting = before.is_some_and(|(state, _)| state == "starting");
            let since = if lane.state == "starting" {
                Some(before.and_then(|(_, since)| *since).unwrap_or(now))
            } else {
                None
            };
            if was_starting
                && lane.state != "starting"
                && before
                    .and_then(|(_, since)| *since)
                    .is_some_and(|since| now.duration_since(since) >= self.notify_after)
            {
                let counts = tree::counts(lane);
                notes.push(if counts.failed > 0 {
                    (
                        format!("start of {} failed", lane.reference),
                        format!("{} failed", counts.failed),
                    )
                } else {
                    (
                        format!("{} is up", lane.reference),
                        format!("{} of {} ready", counts.up, counts.total),
                    )
                });
            }
            for unit in lane
                .units
                .iter()
                .filter(|unit| tree::role(unit) == tree::Role::Service)
            {
                let key = (lane.reference.clone(), unit.name.clone());
                let was_up = self
                    .watched_units
                    .get(&key)
                    .is_some_and(|state| matches!(state.as_str(), "ready" | "running"));
                // A failure during a start is the start's, reported when it ends.
                if was_up && unit.state == "failed" && lane.state != "starting" && !was_starting {
                    let body = unit
                        .exit_code
                        .map(|code| {
                            devlanes_cli::exit::words(code.into())
                                .map_or_else(|| format!("exited {code}"), str::to_owned)
                        })
                        .unwrap_or_default();
                    notes.push((format!("{}:{} crashed", lane.reference, unit.name), body));
                }
                seen_units.insert(key, unit.state.clone());
            }
            self.watched
                .insert(lane.reference.clone(), (lane.state.clone(), since));
        }
        let lanes: HashSet<&str> = self.lanes.iter().map(|l| l.reference.as_str()).collect();
        self.watched
            .retain(|reference, _| lanes.contains(reference.as_str()));
        self.watched_units = seen_units;
        if !self.focused {
            effects.extend(
                notes
                    .into_iter()
                    .map(|(title, body)| Effect::Notify { title, body }),
            );
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
            Msg::QuitReady { error: None } => effects.push(Effect::Quit),
            Msg::QuitReady { error: Some(error) } => {
                self.quitting = false;
                self.set_notice(format!("{error}; q quits anyway"));
                if let Some(notice) = &mut self.notice {
                    notice.error = true;
                }
                // Asked once; the next q closes without asking again.
                self.on_quit = OnQuit::Keep;
            }
            Msg::Key(key) => self.key(key, &mut effects),
            Msg::Mouse(mouse) => self.mouse(mouse, &mut effects),
            Msg::Paste(text) => self.paste(&text, &mut effects),
            Msg::Resize(width, height) => {
                self.size = (width, height);
                self.resize_pane(&mut effects);
            }
            Msg::Tick => self.tick(&mut effects),
            Msg::Focus(focused) => self.focused = focused,
            Msg::Settings(reply) => match self.dialog.as_mut() {
                Some(Dialog::Settings(settings)) => settings.reply(reply),
                // The page closed before the write finished; only a failure matters.
                _ => {
                    let crate::settings::Reply::Loaded { error, .. } = reply;
                    if let Some(error) = error {
                        self.set_notice(format!("options not saved: {error}"));
                    }
                }
            },
            Msg::Status(Ok(lanes)) => {
                self.connection = Connection::Online;
                self.loaded = true;
                self.lanes = lanes;
                sort_lanes(&mut self.lanes);
                self.after_change(&mut effects);
                self.focus_start(&mut effects);
                self.show_welcome();
            }
            Msg::Status(Err(error)) => self.api_error("daemon status", None, error, &mut effects),
            Msg::Start(frame) => self.start_frame(frame, &mut effects),
            Msg::Subscribed(Ok(())) => {
                let reconnected = matches!(self.connection, Connection::Reconnecting { .. });
                if reconnected {
                    // The daemon may be a new one, numbering its starts from 1.
                    self.starts.reset();
                }
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
                        attached.held = None;
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
                attached.held = None;
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
                let settling = self.resize_settling;
                if let Some(attached) = self.attached_mut(generation) {
                    attached.screen.process(&bytes);
                    // The reopened stream's first output is the daemon's full
                    // redraw; before the reopen, output only reaches the live grid.
                    if !settling {
                        attached.held = None;
                    }
                }
            }
            Msg::AttachClosed { generation } => {
                let Some(attached) = self.attached_mut(generation) else {
                    return effects;
                };
                attached.held = None;
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
                    self.resize_settling = false;
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
                        // A unit that could not spawn comes back in `failures` of an
                        // answer that succeeded; it never ran, so status says `stopped`.
                        let failures = crate::start::start_failures(&value);
                        if failures.is_empty() {
                            self.set_notice(done_notice(&label, &value));
                        }
                        let verb = label.split(' ').next().unwrap_or("start");
                        for failure in &failures {
                            let target = Target::Unit {
                                lane: failure.lane.clone(),
                                unit: failure.unit.clone(),
                            };
                            let what = format!("{verb} {}", unit_ref(&failure.lane, &failure.unit));
                            self.report(&what, Some(target), failure_words(failure));
                        }
                        // A whole-lane start can fail dozens of units the same way.
                        if let [first, _, ..] = failures.as_slice() {
                            self.notice = Some(Notice {
                                text: text::clean(&format!(
                                    "{label}: {} units failed; {}",
                                    failures.len(),
                                    failure_words(first).summary
                                )),
                                error: true,
                            });
                        }
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
            Msg::Notices { method, dismissed } => {
                self.release = (Some(method), dismissed);
                self.show_welcome();
            }
            Msg::DaemonInfo { status, doctor } => {
                let mut failure = None;
                match status {
                    Ok(status) => self.daemon.status = Some(status),
                    Err(error) => failure = Some(("daemon status", error)),
                }
                match doctor {
                    Ok(mut doctor) => {
                        // The service manager's stop window is this machine's
                        // to read, not the daemon's; `devlanes doctor` adds it
                        // the same way.
                        if let (Some(service), Some(facts)) =
                            (&self.service_report, doctor.as_object_mut())
                        {
                            facts.insert("service".into(), service.clone());
                        }
                        self.daemon.doctor = Some(doctor);
                    }
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
                let before = if chunk.reset {
                    0
                } else {
                    self.daemon_shown().1.len()
                };
                if chunk.reset {
                    self.daemon.log.clear();
                    self.daemon.scroll = 0;
                    self.daemon.new_lines = 0;
                }
                self.daemon
                    .log
                    .extend(chunk.lines.iter().map(|line| text::clean(line)));
                while self.daemon.log.len() > MAX_DAEMON_LOG {
                    self.daemon.log.pop_front();
                }
                self.daemon.log_position = chunk.position;
                let after = self.daemon_shown().1.len();
                // Positions counted from the newest entry stay put while new
                // entries arrive below them.
                let moved = |index: usize| {
                    let from_end = before.saturating_sub(1 + index);
                    after.saturating_sub(1 + from_end)
                };
                if self.daemon.paused && !chunk.reset {
                    self.daemon.scroll =
                        (self.daemon.scroll + after.saturating_sub(before)).min(after);
                    self.daemon.new_lines += added;
                }
                // Copy mode stays on the same entries; a rotated log ends it.
                self.daemon.copy = match self.daemon.copy {
                    Some(copy) if !chunk.reset && after > 0 => {
                        let shift = |pos: crate::daemonlog::LogPos| crate::daemonlog::LogPos {
                            entry: moved(pos.entry),
                            ..pos
                        };
                        let copy = LogCopy {
                            cursor: shift(copy.cursor),
                            anchor: copy.anchor.map(shift),
                        };
                        // Copy mode pauses, so the view already stays put.
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
        self.watch(effects);
        let lane_known = self
            .selected
            .as_ref()
            .is_some_and(|s| self.lanes.iter().any(|lane| lane.reference == s.lane));
        if !lane_known {
            // A lane that went away (cleaned, removed) leaves the selection
            // in its project, on the lane numbered closest to it.
            let neighbour = self
                .selected
                .as_ref()
                .and_then(|s| self.neighbour_of(&s.lane));
            self.selected = neighbour
                .or_else(|| self.initial_lane())
                .map(|lane| Selected {
                    row: self.default_row(&lane),
                    lane,
                });
        }
        if let Some(lane) = self.selected.as_ref().map(|s| s.lane.clone()) {
            self.auto_fold(&lane);
            let rows = self.rows();
            let row = self.selected.as_ref().map(|s| s.row.clone());
            // The lane itself is selected through its title, not a row.
            if let Some(row) = row
                && row != RowId::WholeLane
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
                                .find(|r| r.folded.is_none() && !matches!(r.id, RowId::Gap(_)))
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
            let nodes = self.lane_tree(summary, None);
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
        self.lane_tree(lane, filter)
    }

    /// A lane's tree with the project's shared containers.
    fn lane_tree(&self, lane: &LaneSummary, filter: Option<&str>) -> Vec<Node> {
        let shared = self.shared_containers(&lane.project);
        tree::tree(lane, &shared, self.task_list(&lane.reference), filter)
    }

    /// The project's shared containers, once each, from whichever of its
    /// lanes reports them, so every lane lists them even when its own infra
    /// is not up.
    pub fn shared_containers(&self, project: &str) -> Vec<devlanes_proto::ContainerSummary> {
        let mut shared: Vec<devlanes_proto::ContainerSummary> = Vec::new();
        for container in self
            .lanes
            .iter()
            .filter(|lane| lane.project == project)
            .flat_map(|lane| lane.containers.iter())
            .filter(|c| c.shared)
        {
            if !shared.iter().any(|c| c.name == container.name) {
                shared.push(container.clone());
            }
        }
        shared.sort_by(|a, b| a.name.cmp(&b.name));
        shared
    }

    /// A container by name: the lane's own, else a shared one another lane
    /// of the project reports.
    pub fn find_container(
        &self,
        lane: &LaneSummary,
        name: &str,
    ) -> Option<devlanes_proto::ContainerSummary> {
        lane.containers
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .or_else(|| {
                self.shared_containers(&lane.project)
                    .into_iter()
                    .find(|c| c.name == name)
            })
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
        let nodes = self.lane_tree(summary, None);
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
        let nodes = self.lane_tree(summary, None);
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

    /// `t` and `i`: the tasks or infra header of the shown lane.
    fn select_section(&mut self, section: tree::Section, effects: &mut Vec<Effect>) {
        let id = RowId::Section(section);
        if self.rows().iter().any(|row| row.id == id) {
            self.select_row(id, effects);
        } else {
            self.set_notice(format!("this lane has no {}", section.name()));
        }
    }

    /// Moves the selection by `delta` rows, over the blank rows between
    /// sections.
    fn move_selection(&mut self, delta: isize, effects: &mut Vec<Effect>) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        // The lane title sits above the first row: k from the top selects
        // the lane, j from the lane the first row.
        let current = match self.selected_row() {
            Some(current) => current,
            None if delta > 0 => {
                let first = rows
                    .iter()
                    .position(|r| !matches!(r.id, RowId::Gap(_)))
                    .unwrap_or(0);
                let next = first.saturating_add_signed(delta - 1).min(rows.len() - 1);
                self.select_row(rows[next].id.clone(), effects);
                return;
            }
            None => return,
        };
        if delta < 0 && current.checked_add_signed(delta).is_none() {
            self.select_row(RowId::WholeLane, effects);
            return;
        }
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
            .map(|summary| self.lane_tree(summary, None))
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
        // The lane shown there last, unless it is gone since.
        let remembered = self
            .project_lane
            .get(name)
            .filter(|lane| self.lanes.iter().any(|l| l.reference == **lane))
            .cloned();
        let lane = remembered.or_else(|| {
            self.lanes
                .iter()
                .find(|lane| lane.project == name)
                .map(|lane| lane.reference.clone())
        });
        if let Some(lane) = lane {
            self.select_lane(&lane, effects);
        }
    }

    /// A lane of the project `gone` belonged to, numbered closest to it (the
    /// lower one on a tie); `None` when that project has no lanes left.
    fn neighbour_of(&self, gone: &str) -> Option<String> {
        let project = self
            .project_lane
            .iter()
            .find(|(_, lane)| lane.as_str() == gone)
            .map(|(project, _)| project.clone())?;
        let index = |reference: &str| {
            reference
                .rsplit_once('@')
                .and_then(|(_, n)| n.parse::<i64>().ok())
        };
        let target = index(gone).unwrap_or(0);
        self.lanes
            .iter()
            .filter(|lane| lane.project == project)
            .min_by_key(|lane| {
                let n = lane.index.map_or(i64::MAX / 2, i64::from);
                ((n - target).abs(), n)
            })
            .map(|lane| lane.reference.clone())
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
            let nodes = self.lane_tree(lane, None);
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

    /// The screen's regions.
    pub fn areas(&self) -> Option<Areas> {
        self.areas_sized(self.size.0, self.size.1)
    }

    /// [`App::areas`] for a frame of `width` x `height`: the lane bar only
    /// when the project has several lanes and the top bar has project tabs;
    /// with one project, the lanes share the top bar.
    pub fn areas_sized(&self, width: u16, height: u16) -> Option<Areas> {
        areas_with(
            width,
            height,
            self.zoom,
            self.project_lanes().len() > 1 && self.projects().len() > 1,
        )
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
            held: None,
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
        if let Some(interactive) = self.attached.as_ref().map(|a| a.interactive) {
            self.reopen_as(interactive, effects);
        }
    }

    /// Reopens the current stream as `interactive`, at the pane's size. The
    /// frame on screen stays until the new stream draws; a copy selection
    /// does not survive the reopen, so its frame is not kept.
    fn reopen_as(&mut self, interactive: bool, effects: &mut Vec<Effect>) {
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        let (lane, unit) = (attached.lane.clone(), attached.unit.clone());
        let held = attached.held.take().or_else(|| {
            matches!(attached.mode, PaneMode::Live).then(|| attached.screen.screen().clone())
        });
        self.open_attach(lane, unit, interactive, effects);
        if let Some(attached) = self.attached.as_mut() {
            attached.held = held;
        }
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
    /// cannot redraw what it cut off. Until the reopened stream draws, the
    /// pane keeps the frame from before the resize.
    fn resize_pane(&mut self, effects: &mut Vec<Effect>) {
        let (columns, rows) = self.attach_size();
        self.ensure_visible();
        let no_pty = self.attached_has_no_pty();
        if let Some(attached) = self.attached.as_mut()
            && attached.screen.size() != (rows, columns)
        {
            if matches!(attached.mode, PaneMode::Live) && attached.held.is_none() {
                attached.held = Some(attached.screen.screen().clone());
            }
            self.resize_settling = true;
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
            .map(|a| InputModes::from_screen(a.shown()))
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
        if self.view == View::Daemon && self.daemon.filter_editing {
            self.daemon_filter_key(key, effects);
            return;
        }
        // Esc clears the log filter before it leaves the page.
        if self.view == View::Daemon
            && key.code == KeyCode::Esc
            && !self.daemon.filters.text.is_empty()
        {
            self.daemon.filters.text.clear();
            self.daemon_filters_changed();
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
            KeyCode::Char('c') if ctrl => self.quit(effects),
            KeyCode::Char('q') if plain => self.quit(effects),
            KeyCode::Char('u') if plain => self.dismiss_release(effects),
            KeyCode::Tab => self.step_project(1, effects),
            KeyCode::BackTab => self.step_project(-1, effects),
            // Shift+1-9 picks a project. Without the kitty keyboard protocol
            // the terminal sends the shifted symbol instead; these are the
            // US layout's.
            KeyCode::Char(c) if !ctrl && project_digit(c, key.modifiers).is_some() => {
                let index = project_digit(c, key.modifiers).unwrap_or_default();
                if let Some(name) = self.projects().get(index).cloned() {
                    self.select_project(&name, effects);
                }
            }
            // 1-9, then 0, pick the project's lanes by their place in the bar,
            // so the first lane is always 1 whatever its number: one project
            // with many lanes is the common case.
            KeyCode::Char(digit @ '0'..='9') if plain => {
                let place = (usize::from(digit as u8 - b'0') + 9) % 10;
                if let Some(lane) = self.project_lanes().get(place).map(|l| l.reference.clone()) {
                    self.view = View::Project;
                    self.select_lane(&lane, effects);
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
                self.quit(effects);
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
        let shown = self.daemon_shown().1.len();
        let max = self.daemon_log_layout(shown).max_scroll;
        let scroll = self.daemon.scroll.min(max);
        // Scrolling up pauses following; G or F follows again. It stops once
        // the oldest entries fill the box.
        let scroll_to = |view: &mut DaemonView, scroll: usize| {
            view.scroll = scroll.min(max);
            if view.scroll > 0 {
                view.paused = true;
            }
        };
        match key.code {
            // On the bottom line of the box, like copy mode in a unit's pane.
            KeyCode::Char('c') if !ctrl && shown > 0 => {
                self.daemon.scroll = scroll;
                let last = self.daemon_visible().last().copied().unwrap_or_default();
                self.daemon.copy = Some(LogCopy {
                    cursor: last,
                    anchor: None,
                });
                self.daemon.paused = true;
            }
            KeyCode::Char('k') | KeyCode::Up => scroll_to(&mut self.daemon, scroll + 1),
            KeyCode::Char('j') | KeyCode::Down => {
                scroll_to(&mut self.daemon, scroll.saturating_sub(1));
            }
            KeyCode::PageUp => scroll_to(&mut self.daemon, scroll + page),
            KeyCode::PageDown => scroll_to(&mut self.daemon, scroll.saturating_sub(page)),
            // g/G mean top/bottom on every tab: the oldest entry, then following again.
            KeyCode::Home | KeyCode::Char('g') => scroll_to(&mut self.daemon, shown),
            KeyCode::End | KeyCode::Char('G') => self.daemon_follow(),
            KeyCode::Char('F') if self.daemon.paused => self.daemon_follow(),
            KeyCode::Char('F') => self.daemon.paused = true,
            KeyCode::Char('w') => {
                self.daemon.filters.floor = self.daemon.filters.floor.next();
                self.daemon_filters_changed();
            }
            KeyCode::Char('d') => {
                self.daemon.filters.unfolded = !self.daemon.filters.unfolded;
                self.daemon_filters_changed();
            }
            KeyCode::Char('/') => self.daemon.filter_editing = true,
            KeyCode::Char('i') => self.daemon.all_checks = !self.daemon.all_checks,
            KeyCode::Char('T') => {
                let (entries, _) = self.daemon_shown();
                let items: Vec<(String, usize, bool)> = crate::daemonlog::targets(&entries)
                    .into_iter()
                    .map(|(target, count)| {
                        let checked = self.daemon.filters.targets.contains(&target);
                        (target, count, checked)
                    })
                    .collect();
                if items.is_empty() {
                    self.set_notice("no log lines yet".into());
                } else {
                    self.dialog = Some(Dialog::Targets(TargetPicker { items, index: 0 }));
                }
            }
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
        // A restart starts the running lanes again in the new daemon; a stop
        // leaves that to `devlanes lane resume`.
        let again = if action == "restart" {
            "The new daemon starts the running lanes again."
        } else {
            "devlanes lane resume starts a lane again."
        };
        let processes = match self.daemon.managed_processes() {
            Some(0) => "Nothing is running.".to_owned(),
            Some(1) => format!("It stops 1 process. {again}"),
            Some(n) => format!("It stops {n} processes. {again}"),
            None => format!("It stops every process the daemon runs. {again}"),
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

    /// The parsed daemon log and the entries the page shows, oldest first.
    pub fn daemon_shown(&self) -> (Vec<crate::daemonlog::Entry>, Vec<crate::daemonlog::Shown>) {
        let entries = crate::daemonlog::parse(&self.daemon.log);
        let shown = crate::daemonlog::shown(&entries, &self.daemon.filters);
        (entries, shown)
    }

    /// Follows the file again from its newest entry.
    fn daemon_follow(&mut self) {
        self.daemon.scroll = 0;
        self.daemon.paused = false;
        self.daemon.new_lines = 0;
    }

    /// Another level floor, text or folding shows other entries: the view
    /// goes back to the newest of them, still paused if it was.
    fn daemon_filters_changed(&mut self) {
        self.daemon.scroll = 0;
        self.daemon.copy = None;
    }

    /// Keys of the `T` picker: move, space checks a target, `a` checks none
    /// (every target), Enter applies, Esc leaves the filter as it was.
    fn targets_key(&mut self, key: KeyEvent, mut picker: TargetPicker) {
        let last = picker.items.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => picker.index = (picker.index + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => picker.index = picker.index.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(item) = picker.items.get_mut(picker.index) {
                    item.2 = !item.2;
                }
            }
            KeyCode::Char('a') => {
                for item in &mut picker.items {
                    item.2 = false;
                }
            }
            KeyCode::Enter => {
                self.daemon.filters.targets = picker
                    .items
                    .into_iter()
                    .filter(|(_, _, checked)| *checked)
                    .map(|(target, _, _)| target)
                    .collect();
                self.daemon_filters_changed();
                return;
            }
            KeyCode::Esc | KeyCode::Char('q') => return,
            _ => {}
        }
        self.dialog = Some(Dialog::Targets(picker));
    }

    /// Keys while the log's text filter takes typing: Enter keeps it, Esc
    /// clears it.
    fn daemon_filter_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        match key.code {
            KeyCode::Esc => {
                self.daemon.filters.text.clear();
                self.daemon.filter_editing = false;
            }
            KeyCode::Enter => self.daemon.filter_editing = false,
            KeyCode::Backspace => {
                self.daemon.filters.text.pop();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit(effects);
                return;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.daemon.filters.text.push(c);
            }
            _ => return,
        }
        self.daemon_filters_changed();
    }

    /// The daemon log box as the last frame drew it; before the first frame,
    /// the body's width and half its height, scrolling up to every entry.
    fn daemon_log_layout(&self, shown: usize) -> crate::daemonlog::LogView {
        let view = self.daemon_log_view.get();
        if view.height > 0 {
            return view;
        }
        crate::daemonlog::LogView {
            x: 0,
            y: 0,
            width: usize::from(self.size.0.saturating_sub(2)).max(1),
            height: usize::from(self.size.1 / 2).max(1),
            max_scroll: shown,
        }
    }

    /// The places of the daemon log's lines on screen, top to bottom.
    fn daemon_visible(&self) -> Vec<crate::daemonlog::LogPos> {
        let (entries, shown) = self.daemon_shown();
        if shown.is_empty() {
            return Vec::new();
        }
        let layout = self.daemon_log_layout(shown.len());
        crate::ui::daemon::visible_lines(
            self,
            &entries,
            &shown,
            self.daemon.scroll,
            layout.width,
            layout.height,
        )
        .into_iter()
        .map(|(place, _)| place)
        .collect()
    }

    /// Keys in copy mode on the daemon log, as in a unit's pane: `h j k l`
    /// move a cell cursor, `v` marks, `y` copies the text as shown, `Y` the
    /// whole entries it touches as they are in the file, Esc leaves.
    fn daemon_copy_key(&mut self, key: KeyEvent, page: usize, effects: &mut Vec<Effect>) {
        use crate::daemonlog::LogPos;
        let (entries, shown) = self.daemon_shown();
        let Some(mut copy) = self.daemon.copy else {
            return;
        };
        let layout = self.daemon_log_layout(shown.len());
        let width = layout.width;
        let lines = |index: usize| -> Vec<ratatui::text::Line<'static>> {
            crate::ui::daemon::entry_lines(self, &entries, &shown, index, width)
        };
        // One drawn line down or up, across entries; `None` at either end.
        let down = |pos: LogPos| -> Option<LogPos> {
            if pos.line + 1 < lines(pos.entry).len() {
                Some(LogPos {
                    line: pos.line + 1,
                    ..pos
                })
            } else {
                (pos.entry + 1 < shown.len()).then_some(LogPos {
                    entry: pos.entry + 1,
                    line: 0,
                    ..pos
                })
            }
        };
        let up = |pos: LogPos| -> Option<LogPos> {
            if pos.line > 0 {
                return Some(LogPos {
                    line: pos.line - 1,
                    ..pos
                });
            }
            let entry = pos.entry.checked_sub(1)?;
            Some(LogPos {
                entry,
                line: lines(entry).len().saturating_sub(1),
                ..pos
            })
        };
        let step = |pos: LogPos, times: usize, by: &dyn Fn(LogPos) -> Option<LogPos>| {
            (0..times).try_fold(pos, |at, _| by(at)).unwrap_or_else(|| {
                // Past an end: stop on the last line there is.
                let mut at = pos;
                while let Some(next) = by(at) {
                    at = next;
                }
                at
            })
        };
        let last_column = width.saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => copy.cursor = step(copy.cursor, 1, &down),
            KeyCode::Char('k') | KeyCode::Up => copy.cursor = step(copy.cursor, 1, &up),
            KeyCode::PageDown => copy.cursor = step(copy.cursor, page, &down),
            KeyCode::PageUp => copy.cursor = step(copy.cursor, page, &up),
            KeyCode::Char('h') | KeyCode::Left => {
                copy.cursor.column = copy.cursor.column.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                copy.cursor.column = (copy.cursor.column + 1).min(last_column);
            }
            KeyCode::Char('0') | KeyCode::Home => copy.cursor.column = 0,
            KeyCode::Char('$') | KeyCode::End => copy.cursor.column = last_column,
            KeyCode::Char('g') => {
                copy.cursor = LogPos {
                    column: copy.cursor.column,
                    ..LogPos::default()
                };
            }
            KeyCode::Char('G') => {
                let entry = shown.len().saturating_sub(1);
                copy.cursor = LogPos {
                    entry,
                    line: lines(entry).len().saturating_sub(1),
                    column: copy.cursor.column,
                };
            }
            KeyCode::Char('v') | KeyCode::Char(' ') => {
                copy.anchor = match copy.anchor {
                    Some(_) => None,
                    None => Some(copy.cursor),
                };
            }
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                let (first, last) = copy.copied();
                let mut text: Vec<String> = Vec::new();
                if key.code == KeyCode::Char('Y') {
                    for row in shown.get(first.entry..=last.entry).unwrap_or_default() {
                        if let Some(entry) = entries.get(row.entry) {
                            text.extend(entry.raw.iter().map(|line| text::clean(line)));
                        }
                    }
                } else {
                    for index in first.entry..=last.entry.min(shown.len().saturating_sub(1)) {
                        for (line, drawn) in lines(index).iter().enumerate() {
                            let start = LogPos {
                                entry: index,
                                line,
                                column: 0,
                            };
                            let end = LogPos {
                                column: usize::MAX,
                                ..start
                            };
                            if end < first || start > last {
                                continue;
                            }
                            let from = if start < first { first.column } else { 0 };
                            let to = if (LogPos { column: 0, ..last }) == start {
                                last.column
                            } else {
                                usize::MAX
                            };
                            let plain = crate::ui::daemon::plain(drawn);
                            let part: String = plain
                                .chars()
                                .skip(from)
                                .take(to.saturating_sub(from).saturating_add(1))
                                .collect();
                            text.push(part.trim_end().to_owned());
                        }
                    }
                }
                self.daemon.copy = None;
                let text = text.join("\n");
                if !text.trim().is_empty() {
                    effects.push(Effect::Copy(text));
                    self.set_notice("copied (OSC 52; some terminals block it)".into());
                }
                return;
            }
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => {
                self.daemon.copy = None;
                return;
            }
            _ => {}
        }
        self.daemon.copy = Some(copy);
        self.daemon.scroll = self.daemon_reveal(copy.cursor, &entries, &shown, layout);
    }

    /// The least scroll that brings `cursor`'s line into view. The box is
    /// anchored at its newest entry, so the line shows while it and the lines
    /// below it fit: count down from it, one entry's lines at a time.
    fn daemon_reveal(
        &self,
        cursor: crate::daemonlog::LogPos,
        entries: &[crate::daemonlog::Entry],
        shown: &[crate::daemonlog::Shown],
        layout: crate::daemonlog::LogView,
    ) -> usize {
        let count = shown.len();
        // Below the box: its entry becomes the newest shown.
        let bottom = count.saturating_sub(1 + cursor.entry);
        if self.daemon.scroll > bottom {
            return bottom;
        }
        let lines = |index: usize| {
            crate::ui::daemon::entry_lines(self, entries, shown, index, layout.width).len()
        };
        let mut used = lines(cursor.entry).saturating_sub(cursor.line);
        let mut last = cursor.entry;
        while last + 1 < count && used + lines(last + 1) <= layout.height {
            last += 1;
            used += lines(last);
        }
        let needed = (count - 1 - last).min(layout.max_scroll);
        self.daemon.scroll.max(needed)
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
            KeyCode::Char('t') => self.select_section(tree::Section::Tasks, effects),
            KeyCode::Char('i') => self.select_section(tree::Section::Infra, effects),
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
            KeyCode::Char('S') => self.start_everything(effects),
            KeyCode::Char('x') => self.control(Method::Stop, effects),
            KeyCode::Char('r') => self.control(Method::Restart, effects),
            KeyCode::Char('p') => self.profile_prompt(effects),
            KeyCode::Char('z') => {
                self.zoom = !self.zoom;
                // Zooming means looking at this terminal: take its size.
                let watching = self.attached.as_ref().is_some_and(|a| !a.interactive);
                if watching {
                    self.ensure_visible();
                    self.reopen_as(true, effects);
                } else {
                    self.resize_pane(effects);
                }
            }
            KeyCode::Char('o') => self.open_url(effects),
            KeyCode::Char('c') => self.enter_copy(),
            KeyCode::PageUp => {
                let page = self.page();
                self.scroll(page, effects);
            }
            KeyCode::PageDown => {
                let page = self.page();
                self.scroll(-page, effects);
            }
            // One step back: the kept filter, then a scrolled pane; with
            // nothing left to cancel, the Esc menu.
            KeyCode::Esc => {
                if self.filter.take().is_some() {
                    self.after_change(effects);
                } else if let Some(attached) = self
                    .attached
                    .as_mut()
                    .filter(|attached| attached.screen.scroll_offset() > 0)
                {
                    attached.screen.scroll_to_bottom();
                } else {
                    self.dialog = Some(Dialog::Menu(crate::menu::Menu::default()));
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
            .is_some_and(|a| a.shown().alternate_screen())
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
            Dialog::Menu(mut menu) => {
                let action = menu.key(key);
                self.menu_action(action, menu, effects);
            }
            Dialog::Settings(mut settings) => {
                let outcome = settings.key(key, &mut self.theme, effects);
                self.settings_outcome(outcome, settings, effects);
            }
            dialog @ Dialog::Quit(_) => {
                self.dialog = Some(dialog);
                self.quit_dialog_key(key, effects);
            }
            Dialog::Targets(picker) => self.targets_key(key, picker),
            Dialog::Welcome(welcome) => {
                let action = welcome.key(key);
                self.welcome_action(action, welcome, effects);
            }
        }
    }

    /// Opens the welcome once the first status and the dismissed notices are
    /// both in, unless it was closed before in this home.
    fn show_welcome(&mut self) {
        let dismissed = self.release.1.iter().any(|key| key == crate::welcome::KEY);
        if self.loaded && self.release.0.is_some() && !dismissed && self.dialog.is_none() {
            self.dialog = Some(Dialog::Welcome(crate::welcome::Welcome::default()));
        }
    }

    /// What a key or click in the welcome asked for. Leaving it in any way
    /// hides it for good.
    fn welcome_action(
        &mut self,
        action: crate::welcome::Action,
        welcome: crate::welcome::Welcome,
        effects: &mut Vec<Effect>,
    ) {
        use crate::welcome::{Action, DOCS_URL, KEY};
        match action {
            Action::Stay => self.dialog = Some(Dialog::Welcome(welcome)),
            Action::OpenDocs => {
                effects.push(Effect::OpenUrl(DOCS_URL.into()));
                self.dialog = Some(Dialog::Welcome(welcome));
            }
            Action::Close | Action::Quit => {
                self.release.1.push(KEY.into());
                effects.push(Effect::DismissNotice(KEY.into()));
                if action == Action::Quit {
                    self.global_key(
                        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                        effects,
                    );
                }
            }
        }
    }

    /// What a key or click in the Esc menu asked for.
    fn menu_action(
        &mut self,
        action: crate::menu::Action,
        menu: crate::menu::Menu,
        effects: &mut Vec<Effect>,
    ) {
        use crate::menu::{Action, Item};
        match action {
            Action::Stay => self.dialog = Some(Dialog::Menu(menu)),
            Action::Close => {}
            Action::Pick(Item::Options) => {
                self.dialog = Some(Dialog::Settings(Box::default()));
                effects.push(Effect::Settings(crate::settings::Request::Load));
            }
            Action::Pick(Item::Help) => self.dialog = Some(Dialog::Help),
            // The same path as q, so quitting asks what q asks.
            Action::Pick(Item::Quit) => {
                self.global_key(
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                    effects,
                );
            }
        }
    }

    /// What a key or click on the options page asked for.
    fn settings_outcome(
        &mut self,
        outcome: crate::settings::Outcome,
        settings: Box<crate::settings::Settings>,
        effects: &mut Vec<Effect>,
    ) {
        use crate::settings::Outcome;
        if outcome != Outcome::Stay && !settings.restart.is_empty() {
            self.set_notice("changed settings apply after a daemon restart: D, then R".into());
        }
        match outcome {
            Outcome::Stay => self.dialog = Some(Dialog::Settings(settings)),
            Outcome::Menu => self.dialog = Some(Dialog::Menu(crate::menu::Menu::default())),
            Outcome::Close => {}
            Outcome::Quit => {
                let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
                self.global_key(ctrl_c, effects);
            }
            Outcome::RestartDaemon => self.daemon_prompt(
                "restart the daemon",
                "Restart the daemon?",
                "restart",
                "This TUI reconnects on its own.",
                Effect::RestartDaemon,
            ),
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

    /// What the monitor asks `metrics` for: one raw sample per second for as
    /// many seconds as the widest graph has columns (before the first frame,
    /// two per terminal column), at most the daemon's raw ring.
    pub fn metrics_query(&self) -> MetricsQuery {
        let seconds = match self.monitor_seconds.get() {
            0 => usize::from(self.size.0) * 2,
            seconds => seconds,
        };
        let range_ms = (seconds as u64 * 1000).clamp(60_000, crate::monitor::RAW_RANGE_MS);
        MetricsQuery {
            range_ms,
            points: 0,
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
            KeyCode::Char('c') => self.monitor.memory_first = false,
            KeyCode::Char('m') => self.monitor.memory_first = true,
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
        // Keys bring the selection into view, wherever the wheel left it.
        if self.monitor.index != index {
            let height = self.monitor_height();
            self.monitor.reveal(height);
        }
    }

    /// Rows the monitor table shows; before its first frame, half the screen.
    fn monitor_height(&self) -> usize {
        match self.monitor_table_rows.get() {
            0 => usize::from(self.size.1 / 2),
            rows => rows,
        }
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

    /// `S`: every service of the shown lane that does not run yet, autostart
    /// or not, named one by one.
    fn start_everything(&mut self, effects: &mut Vec<Effect>) {
        let Some(lane) = self.selected_lane() else {
            return;
        };
        let reference = lane.reference.clone();
        let units: Vec<String> = lane
            .units
            .iter()
            .filter(|u| tree::role(u) == tree::Role::Service && !is_active(&u.state))
            .map(|u| unit_ref(&reference, &u.name))
            .collect();
        if units.is_empty() {
            self.set_notice(format!("every service in {reference} already runs"));
            return;
        }
        self.pending_begin(&reference);
        effects.push(Effect::Control {
            method: Method::Start,
            lane: reference.clone(),
            selection: Selection {
                units,
                ..Selection::default()
            },
            label: format!("start every service in {reference}"),
            target: Target::Lane(reference),
        });
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
                self.quit(effects);
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
        // `s` on the lane starts what the default profile (or autostart)
        // selects, whatever the last start chose; `S` starts everything.
        let profiles = if method == Method::Start && row == RowId::WholeLane {
            vec![devlanes_proto::DEFAULT_PROFILE.to_owned()]
        } else {
            Vec::new()
        };
        self.pending_begin(&reference);
        effects.push(Effect::Control {
            method,
            lane: reference,
            selection: Selection {
                units,
                profiles,
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

    /// Opens the unit's address, or the lane's first unit address.
    fn open_url(&mut self, effects: &mut Vec<Effect>) {
        // `public_url` is the first host of `hosts` over https (with the
        // proxy's https port when it is not 443), the address people use;
        // older daemons only list it among `urls`, after the plain URL.
        let preferred = |unit: &UnitSummary| {
            unit.public_url
                .as_ref()
                .or_else(|| unit.urls.iter().find(|url| url.starts_with("https://")))
                .or_else(|| unit.urls.first())
                .cloned()
        };
        let url = match self.selected_unit() {
            Some(unit) => preferred(unit),
            None => self
                .selected_lane()
                .and_then(|lane| lane.units.iter().find_map(preferred)),
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
        match self.dialog.take() {
            Some(Dialog::Menu(mut menu)) => {
                let action = menu.mouse(mouse);
                self.menu_action(action, menu, effects);
                return;
            }
            Some(Dialog::Settings(mut settings)) => {
                let outcome = settings.mouse(mouse, &mut self.theme, effects);
                self.settings_outcome(outcome, settings, effects);
                return;
            }
            Some(Dialog::Welcome(welcome)) => {
                let action = welcome.mouse(mouse);
                self.welcome_action(action, welcome, effects);
                return;
            }
            other => self.dialog = other,
        }
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
            View::Daemon => self.daemon_mouse(mouse, effects),
        }
    }

    /// The daemon tab: the wheel scrolls the log, a drag over it selects
    /// text as copy mode does and the release copies it.
    fn daemon_mouse(&mut self, mouse: MouseEvent, effects: &mut Vec<Effect>) {
        let shown = self.daemon_shown().1.len();
        let max = self.daemon_log_layout(shown).max_scroll;
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.daemon.scroll = (self.daemon.scroll + 3).min(max);
                if self.daemon.scroll > 0 {
                    self.daemon.paused = true;
                }
            }
            MouseEventKind::ScrollDown => {
                self.daemon.scroll = self.daemon.scroll.saturating_sub(3);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // The cells are mapped through the lines as drawn, which use the clamped scroll.
                self.daemon.scroll = self.daemon.scroll.min(max);
                self.daemon.press = self.daemon_cell(mouse.column, mouse.row, false);
                if self.daemon.press.is_some() {
                    self.daemon.copy = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.daemon.press
                    && let Some(cursor) = self.daemon_cell(mouse.column, mouse.row, true)
                {
                    self.daemon.copy = Some(LogCopy {
                        cursor,
                        anchor: Some(anchor),
                    });
                    // Arriving lines would move the text under the selection.
                    self.daemon.paused = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let press = self.daemon.press.take();
                let dragged = self.daemon.copy.is_some_and(|copy| {
                    copy.anchor.is_some()
                        && copy.anchor == press
                        && copy.anchor != Some(copy.cursor)
                });
                if dragged {
                    let page = usize::from(self.size.1 / 2).max(1);
                    let copy = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
                    self.daemon_copy_key(copy, page, effects);
                }
            }
            _ => {}
        }
    }

    /// The daemon log cell at screen `(column, row)`. Outside the lines shown
    /// it is `None`, or with `clamp` the nearest cell, so a drag past the
    /// box's edge selects up to its first or last line.
    fn daemon_cell(&self, column: u16, row: u16, clamp: bool) -> Option<crate::daemonlog::LogPos> {
        let view = self.daemon_log_view.get();
        if view.height == 0 {
            return None;
        }
        let visible = self.daemon_visible();
        let last = visible.len().checked_sub(1)?;
        let dy = i32::from(row) - i32::from(view.y);
        let dx = i32::from(column) - i32::from(view.x);
        let line = usize::try_from(dy).ok().filter(|line| *line <= last);
        let cell = usize::try_from(dx).ok().filter(|cell| *cell < view.width);
        if !clamp && (line.is_none() || cell.is_none()) {
            return None;
        }
        let line = line.unwrap_or(if dy < 0 { 0 } else { last });
        let cell = cell.unwrap_or(if dx < 0 { 0 } else { view.width - 1 });
        Some(crate::daemonlog::LogPos {
            column: cell,
            ..visible[line]
        })
    }

    /// What a click on a chrome item does.
    fn click(&mut self, click: Click, effects: &mut Vec<Effect>) {
        match click {
            Click::Project(name) => self.select_project(&name, effects),
            // A chip, like the list's title, shows the lane's status.
            Click::Lane(lane) => {
                self.view = View::Project;
                self.select(Selected::lane(&lane), effects);
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

    /// The monitor: the wheel scrolls the table three rows and leaves the
    /// selection where it is; clicks on rows come through their hit areas.
    fn monitor_mouse(&mut self, mouse: MouseEvent, layout: &Areas, effects: &mut Vec<Effect>) {
        let _ = (layout, effects);
        let rows = self.monitor_rows().len();
        let height = self.monitor_height();
        match mouse.kind {
            MouseEventKind::ScrollUp => self.monitor.scroll(-3, rows, height),
            MouseEventKind::ScrollDown => self.monitor.scroll(3, rows, height),
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
        let own = self.version;
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
            let command = self.release.0.as_ref()?.upgrade_command(latest);
            Some((
                update::update_key(latest),
                format!("{latest} available: {command}"),
            ))
        })?;
        (!self.release.1.contains(&notice.0)).then_some(notice)
    }

    /// Hides the release notice for good; a newer version shows again.
    fn dismiss_release(&mut self, effects: &mut Vec<Effect>) {
        let Some((key, _)) = self.release_badge() else {
            return;
        };
        self.release.1.push(key.clone());
        effects.push(Effect::DismissNotice(key));
    }

    /// q, Ctrl+C or the menu's quit: closes at once unless lanes of the
    /// project on screen run, then does what `tui.on_quit` says (asks by
    /// default). Pressed again while stopping, it closes without waiting.
    pub fn quit(&mut self, effects: &mut Vec<Effect>) {
        let running: Vec<String> = self
            .project_lanes()
            .into_iter()
            .filter(|lane| tree::lane_is_live(lane))
            .map(|lane| lane.reference.clone())
            .collect();
        if self.quitting || running.is_empty() {
            effects.push(Effect::Quit);
            return;
        }
        match self.on_quit {
            OnQuit::Keep => effects.push(Effect::Quit),
            OnQuit::Stop => self.quit_with(running, None, effects),
            OnQuit::Ask => {
                self.dialog = Some(Dialog::Quit(QuitDialog {
                    lanes: running,
                    index: 0,
                    remember: false,
                }));
            }
        }
    }

    /// Saves the remembered choice and stops `stop`, then quits.
    fn quit_with(
        &mut self,
        stop: Vec<String>,
        remember: Option<OnQuit>,
        effects: &mut Vec<Effect>,
    ) {
        if stop.is_empty() && remember.is_none() {
            effects.push(Effect::Quit);
            return;
        }
        self.quitting = true;
        if !stop.is_empty() {
            self.set_notice(format!("stopping {}… q closes now", stop.join(", ")));
        }
        effects.push(Effect::QuitWith { stop, remember });
    }

    /// Keys of the quit dialog: up and down pick, space ticks "remember",
    /// enter acts, esc or n stays, q closes keeping the lanes, Ctrl+C closes.
    fn quit_dialog_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let Some(Dialog::Quit(dialog)) = &mut self.dialog else {
            return;
        };
        let last = QUIT_CHOICES.len() - 1;
        let choice = match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.dialog = None;
                effects.push(Effect::Quit);
                return;
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                dialog.index = (dialog.index + 1).min(last);
                return;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                dialog.index = dialog.index.saturating_sub(1);
                return;
            }
            KeyCode::Char(' ' | 'r') => {
                dialog.remember = !dialog.remember;
                return;
            }
            KeyCode::Esc | KeyCode::Char('n') => last,
            KeyCode::Char('q') => 0,
            KeyCode::Enter => dialog.index,
            _ => return,
        };
        self.choose_quit(choice, effects);
    }

    /// Acts on quit dialog choice `index` (see [`QUIT_CHOICES`]).
    pub fn choose_quit(&mut self, index: usize, effects: &mut Vec<Effect>) {
        let Some(Dialog::Quit(dialog)) = self.dialog.take() else {
            return;
        };
        let choice = match index {
            0 => OnQuit::Keep,
            1 => OnQuit::Stop,
            _ => return,
        };
        let remember = dialog.remember.then_some(choice);
        if let Some(choice) = remember {
            self.on_quit = choice;
        }
        let stop = if choice == OnQuit::Stop {
            dialog.lanes
        } else {
            Vec::new()
        };
        self.quit_with(stop, remember, effects);
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

/// The project index for Shift+1-9: the digit with SHIFT (kitty keyboard
/// protocol), or the US layout's shifted symbol.
fn project_digit(c: char, modifiers: KeyModifiers) -> Option<usize> {
    if c.is_ascii_digit() && c != '0' && modifiers.contains(KeyModifiers::SHIFT) {
        return Some(usize::from(c as u8 - b'1'));
    }
    "!@#$%^&*(".find(c)
}

/// `rows` with a blank row between sections, as the mockups draw them.
fn with_gaps(rows: Vec<Row>) -> Vec<Row> {
    let mut out = Vec::with_capacity(rows.len() + 3);
    let mut gaps = 0u8;
    for row in rows {
        if matches!(row.id, RowId::Section(_)) && !out.is_empty() {
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
        (None, Some(code)) => Worded::local(
            &devlanes_cli::exit::words(code.into())
                .map_or_else(|| format!("exited with code {code}"), str::to_owned),
        ),
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

/// Display order: devlanes.yaml's (`order`), with ad-hoc exec units after
/// the declared ones. The sort is stable, so units without an `order` (an
/// older daemon, exec units) keep the daemon's order at the end of their kind.
pub fn sort_units(units: &mut [UnitSummary]) {
    units.sort_by_key(|unit| (unit.kind == "exec", unit.order.unwrap_or(u32::MAX)));
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
