//! TUI state and the pure update function: messages in, effects out.
//!
//! Nothing here performs I/O, so key handling, event application and resync
//! behavior are tested without a terminal or a daemon.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use devlanes_proto::{
    ActorKind, AttachResult, Event, LaneSummary, LogLine, LogQuery, Method, Page, RunParams,
    Selection, UnitSummary, events,
};
use ratatui::layout::Rect;
use serde_json::Value;

use crate::api::{ApiError, UnitMetrics};
use crate::input::{self, InputModes};
use crate::screen::TerminalScreen;

/// Smallest terminal that still fits a list, a pane and the help bar.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 8;
/// Notices stay this many one-second ticks.
const NOTICE_TICKS: u8 = 6;
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

/// Top-level views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Lanes,
    Monitor,
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

/// Modal prompts; while one is open it receives every key.
#[derive(Debug)]
pub enum Dialog {
    Confirm {
        prompt: String,
        effect: Box<Effect>,
    },
    Tasks {
        lane: String,
        tasks: Vec<String>,
        index: usize,
    },
    Profile {
        lane: String,
        input: String,
    },
    Help,
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
    Failed(String),
}

/// The one unit parsed client-side.
pub struct Attached {
    pub generation: u64,
    pub lane: String,
    pub unit: String,
    pub state: AttachState,
    pub screen: TerminalScreen,
    pub mode: PaneMode,
}

/// Inputs to [`App::update`].
#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    /// Outer terminal size.
    Resize(u16, u16),
    /// One second passed.
    Tick,
    Status(Result<Vec<LaneSummary>, ApiError>),
    Subscribed(Result<(), ApiError>),
    Event(Event),
    EventsEnded,
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
    ActionDone {
        label: String,
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
    /// A helper (URL opener, terminal launcher) failed.
    Failed(String),
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
    pub notice: Option<String>,
    pub size: (u16, u16),
    expanded: HashMap<String, bool>,
    notice_ticks: u8,
    generation: u64,
    reconnect_pending: bool,
}

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
            size: (width, height),
            expanded: HashMap::new(),
            notice_ticks: 0,
            generation: 0,
            reconnect_pending: false,
        }
    }

    /// Subscribes before querying so no change between the two is missed.
    pub fn start(&self) -> Vec<Effect> {
        vec![Effect::Subscribe, Effect::Refresh]
    }

    /// Applies one message.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        let mut effects = Vec::new();
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
            }
            Msg::Status(Err(error)) => self.api_error("status", error, &mut effects),
            Msg::Subscribed(Ok(())) => {
                let reconnected = matches!(self.connection, Connection::Reconnecting { .. });
                self.connection = Connection::Online;
                if reconnected {
                    self.reattach_if_closed(&mut effects);
                }
            }
            Msg::Subscribed(Err(error)) => {
                self.lost_connection(&mut effects);
                if !error.is_disconnect() {
                    self.set_notice(format!("subscribe: {}", describe(&error)));
                }
            }
            Msg::EventsEnded => self.lost_connection(&mut effects),
            Msg::Reconnect => {
                self.reconnect_pending = false;
                effects.extend([Effect::Subscribe, Effect::Refresh]);
            }
            Msg::Event(event) => self.event(event, &mut effects),
            Msg::AttachOpened { generation, result } => {
                if let Some(attached) = self.attached_mut(generation) {
                    attached.state = match result {
                        Ok(_) => AttachState::Live,
                        Err(error) => AttachState::Failed(describe(&error)),
                    };
                }
            }
            Msg::AttachOutput { generation, bytes } => {
                if let Some(attached) = self.attached_mut(generation) {
                    attached.screen.process(&bytes);
                }
            }
            Msg::AttachClosed { generation } => {
                if let Some(attached) = self.attached_mut(generation) {
                    if !matches!(attached.state, AttachState::Failed(_)) {
                        attached.state = AttachState::Closed;
                    }
                    if self.focus == Focus::Terminal {
                        self.focus = Focus::List;
                    }
                }
            }
            Msg::ActionDone { label, result } => match result {
                Ok(_) => self.set_notice(format!("{label}: ok")),
                Err(error) => self.api_error(&label, error, &mut effects),
            },
            Msg::RunDone { lane, task, result } => self.run_done(lane, task, result, &mut effects),
            Msg::Logs {
                generation,
                older,
                result,
            } => self.logs(generation, older, result),
            Msg::Metrics(Ok(metrics)) => {
                self.metrics = metrics;
                self.clamp_monitor();
            }
            // The table falls back to per-unit numbers from status, so this stays quiet.
            Msg::Metrics(Err(_)) => self.metrics.clear(),
            Msg::Failed(message) => self.set_notice(message),
        }
        effects
    }

    // ---- connection -------------------------------------------------------

    /// Marks the daemon unreachable and schedules one retry with backoff.
    fn lost_connection(&mut self, effects: &mut Vec<Effect>) {
        let attempt = match self.connection {
            Connection::Reconnecting { attempt } => attempt.saturating_add(1),
            _ => 0,
        };
        self.connection = Connection::Reconnecting { attempt };
        if !self.reconnect_pending {
            self.reconnect_pending = true;
            let after_ms = (250u64 << attempt.min(5)).min(MAX_BACKOFF_MS);
            effects.push(Effect::ScheduleReconnect { after_ms });
        }
    }

    /// Shows a request failure, or treats it as a lost connection.
    fn api_error(&mut self, what: &str, error: ApiError, effects: &mut Vec<Effect>) {
        if error.is_disconnect() {
            self.lost_connection(effects);
        } else {
            self.set_notice(format!("{what}: {}", describe(&error)));
        }
    }

    /// Re-opens the attach stream after the daemon or the unit came back.
    fn reattach_if_closed(&mut self, effects: &mut Vec<Effect>) {
        let closed = self
            .attached
            .as_ref()
            .is_some_and(|a| matches!(a.state, AttachState::Closed | AttachState::Failed(_)));
        if closed {
            self.attached = None;
            effects.push(Effect::Detach);
            self.sync_attach(effects);
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
                    None => lane.units.push(unit),
                }
                if is_attached {
                    // A restart starts a new PTY; reopen a stream that ended with the old one.
                    self.reattach_if_closed(effects);
                }
                self.after_change(effects);
            }
            events::PHASE if event.data.get("state").and_then(Value::as_str) == Some("failed") => {
                let lane = event
                    .data
                    .get("lane")
                    .and_then(Value::as_str)
                    .unwrap_or("lane");
                let phase = event
                    .data
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("start");
                self.set_notice(format!("{lane}: {phase} failed"));
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
        if row < self.list_offset {
            // Keep a project header visible above its first lane.
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

    /// Attaches to the selected unit and detaches from anything else.
    fn sync_attach(&mut self, effects: &mut Vec<Effect>) {
        let wanted = match &self.selected {
            Some(Selected::Unit { lane, unit }) => Some((lane.clone(), unit.clone())),
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
        if self.focus == Focus::Terminal {
            self.focus = Focus::List;
        }
        let Some((lane, unit)) = wanted else {
            return;
        };
        let (columns, rows) = self.pane_size();
        self.generation += 1;
        self.attached = Some(Attached {
            generation: self.generation,
            lane: lane.clone(),
            unit: unit.clone(),
            state: AttachState::Opening,
            screen: TerminalScreen::new(rows, columns),
            mode: PaneMode::Live,
        });
        effects.push(Effect::Attach {
            generation: self.generation,
            unit: unit_ref(&lane, &unit),
            lane,
            columns,
            rows,
        });
    }

    /// The pane's inner `(columns, rows)`, using the lanes layout on every tab so
    /// switching tabs never resizes the unit's PTY.
    pub fn pane_size(&self) -> (u16, u16) {
        areas(self.size.0, self.size.1, self.zoom).map_or((MIN_WIDTH, MIN_HEIGHT), |a| {
            (a.pane_inner.width.max(1), a.pane_inner.height.max(1))
        })
    }

    /// Resizes the local grid and the remote PTY after a layout change.
    fn resize_pane(&mut self, effects: &mut Vec<Effect>) {
        let (columns, rows) = self.pane_size();
        self.ensure_visible();
        if let Some(attached) = self.attached.as_mut()
            && attached.screen.size() != (rows, columns)
        {
            attached.screen.resize(rows, columns);
            if attached.state == AttachState::Live {
                effects.push(Effect::ResizePty { columns, rows });
            }
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
        if self.dialog.is_some() {
            self.dialog_key(key, effects);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('a') {
            self.toggle_focus();
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
                PaneMode::Logs(_) if key.code == KeyCode::Esc => {
                    attached.mode = PaneMode::Live;
                    attached.screen.scroll_to_bottom();
                    return;
                }
                _ => {}
            }
        }
        if self.tab == Tab::Monitor {
            self.monitor_key(key, effects);
            return;
        }
        self.list_key(key, effects);
    }

    /// Toggles list and terminal focus; the terminal needs an attached unit.
    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Terminal => Focus::List,
            Focus::List if self.tab == Tab::Lanes && self.attached.is_some() => Focus::Terminal,
            Focus::List => {
                self.set_notice("select a unit to type into it".into());
                Focus::List
            }
        };
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
                Some(Selected::Unit { .. }) => self.toggle_focus(),
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
            KeyCode::Tab | KeyCode::Char('2') => self.tab = Tab::Monitor,
            KeyCode::Char('1') => self.tab = Tab::Lanes,
            KeyCode::Char('s') => self.control(Method::Start, effects),
            KeyCode::Char('x') => self.control(Method::Stop, effects),
            KeyCode::Char('r') => self.control(Method::Restart, effects),
            KeyCode::Char('t') => self.run_prompt(),
            KeyCode::Char('p') => self.profile_prompt(),
            KeyCode::Char('z') => {
                self.zoom = !self.zoom;
                self.resize_pane(effects);
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

    /// Keys in terminal focus go to the unit, except the focus toggle.
    fn terminal_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let modes = self.modes();
        let Some(attached) = self.attached.as_mut() else {
            self.focus = Focus::List;
            return;
        };
        if attached.state != AttachState::Live {
            return;
        }
        // Typing returns to the live screen, like a real terminal.
        attached.mode = PaneMode::Live;
        attached.screen.scroll_to_bottom();
        if let Some(bytes) = input::encode_key(&key, modes) {
            effects.push(Effect::Input(bytes));
        }
    }

    /// Paste goes to the profile prompt or, in terminal focus, to the unit.
    fn paste(&mut self, text: &str, effects: &mut Vec<Effect>) {
        if let Some(Dialog::Profile { input, .. }) = self.dialog.as_mut() {
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
            Dialog::Confirm { prompt, effect } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => effects.push(*effect),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.dialog = Some(Dialog::Confirm { prompt, effect }),
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
            Dialog::Profile { lane, mut input } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let profile = input.trim().to_owned();
                    let label = if profile.is_empty() {
                        format!("start {lane} with the default profile")
                    } else {
                        format!("start {lane} with profile {profile}")
                    };
                    let selection = Selection {
                        profile: (!profile.is_empty()).then_some(profile),
                        ..Selection::default()
                    };
                    effects.push(Effect::Control {
                        method: Method::Start,
                        lane,
                        selection,
                        label,
                    });
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.dialog = Some(Dialog::Profile { lane, input });
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(c);
                    self.dialog = Some(Dialog::Profile { lane, input });
                }
                _ => self.dialog = Some(Dialog::Profile { lane, input }),
            },
        }
    }

    /// Keys on the monitor tab.
    fn monitor_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => effects.push(Effect::Quit),
            KeyCode::Char('c') if ctrl => effects.push(Effect::Quit),
            KeyCode::Tab | KeyCode::Char('1') | KeyCode::Esc => self.tab = Tab::Lanes,
            KeyCode::Char('j') | KeyCode::Down => {
                self.monitor_index = self.monitor_index.saturating_add(1);
                self.clamp_monitor();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.monitor_index = self.monitor_index.saturating_sub(1);
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
        effects.push(Effect::Control {
            method,
            lane: reference,
            selection: Selection {
                units,
                ..Selection::default()
            },
            label: format!("{verb} {target}"),
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
        match result {
            Ok(value) => {
                self.set_notice(format!("run {task}: started"));
                self.expanded.insert(lane.clone(), true);
                self.selected = Some(Selected::Unit { lane, unit: task });
                self.after_change(effects);
                let interactive = value.get("interactive").and_then(Value::as_bool) == Some(true);
                if interactive && self.attached.is_some() {
                    self.focus = Focus::Terminal;
                }
            }
            Err(error) => self.api_error(&format!("run {task}"), error, effects),
        }
    }

    /// Opens the profile prompt prefilled with the lane's current profile.
    fn profile_prompt(&mut self) {
        if let Some(lane) = self.selected_lane() {
            self.dialog = Some(Dialog::Profile {
                lane: lane.reference.clone(),
                input: lane.profile.clone().unwrap_or_default(),
            });
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
    fn logs(&mut self, generation: u64, older: bool, result: Result<Page<LogLine>, ApiError>) {
        let Some(attached) = self.attached_mut(generation) else {
            return;
        };
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
            Err(error) => {
                let notice = format!("logs: {}", describe(&error));
                self.set_notice(notice);
            }
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
                    self.set_notice("copied".into());
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
                MouseEventKind::Down(_) if self.attached.is_some() => {
                    self.focus = Focus::Terminal;
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

    /// Ages the notice and polls metrics while the monitor is visible.
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
    }

    /// Shows a short status message in the header.
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
        self.notice_ticks = NOTICE_TICKS;
    }

    /// Rows of the monitor table: metrics when the daemon has them, else status numbers.
    pub fn monitor_rows(&self) -> Vec<UnitMetrics> {
        if !self.metrics.is_empty() {
            return self.metrics.clone();
        }
        self.lanes
            .iter()
            .flat_map(|lane| lane.units.iter())
            .filter(|unit| is_active(&unit.state))
            .map(|unit| UnitMetrics {
                lane: unit.lane.clone(),
                unit: unit.name.clone(),
                cpu_percent: unit.cpu_percent,
                memory_bytes: unit.memory_bytes,
                history: Vec::new(),
            })
            .collect()
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

/// States with a live process behind them.
pub fn is_active(state: &str) -> bool {
    matches!(state, "starting" | "ready" | "running" | "stopping")
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

/// Short lowercase error text: the code plus a message field when present.
pub fn describe(error: &ApiError) -> String {
    match error {
        ApiError::Rpc(rpc) => {
            let detail = rpc
                .data
                .get("message")
                .and_then(Value::as_str)
                .map(|message| format!(" ({message})"))
                .unwrap_or_default();
            format!("{}{detail}", rpc.code.replace('_', " "))
        }
        other => other.to_string(),
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

/// Projects by name, lanes by index, unassigned lanes last.
fn sort_lanes(lanes: &mut [LaneSummary]) {
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
