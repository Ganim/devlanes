//! Rendering. Reads [`App`] and draws; never mutates state.
//!
//! Every string from the daemon is cleaned and fitted to its widget before it
//! reaches the buffer (see [`crate::text`]), so long or hostile text cannot
//! spill into another widget or move the real terminal's cursor.

use devlanes_proto::{LaneSummary, ORPHANED, UnitSummary};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Chart, Clear, Dataset, GraphType, Paragraph,
        Row as TableRow, Table, TableState,
    },
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::api::UnitMetrics;
use crate::app::{
    App, AttachState, Connection, CopyState, Dialog, ErrorEntry, Focus, LogView, MIN_HEIGHT,
    MIN_WIDTH, MonitorRow, PaneMode, PanelLine, ProfileChoice, ProfilesState, Row, Tab, Target,
    areas, error_panel_area, is_orphaned, lane_profiles, started_by_agent, unit_ref,
};
use crate::doctor;
use crate::text::{self, clean, clip};
use crate::words::{self, STDERR_TAIL, Worded};

/// Brand palette A (brand/design.md): yellow means "this one", never a status.
const PAINT: Color = Color::Rgb(0xf5, 0xc5, 0x42);
const ON_PAINT: Color = Color::Rgb(0x1a, 0x19, 0x17);
const MUTED: Color = Color::Rgb(0xa6, 0xa3, 0x99);
const RULE: Color = Color::Rgb(0x3d, 0x3b, 0x36);
const READY: Color = Color::Rgb(0x3e, 0xa6, 0x6b);
const FAILED: Color = Color::Rgb(0xd9, 0x4f, 0x3d);
/// Needs attention without being a failure (orphaned processes); red stays for failures.
const WARN: Color = Color::Rgb(0xe8, 0x8a, 0x3c);
/// Width of the label column in detail panes.
const LABEL: usize = 10;
/// `warn sandbox ` fits the doctor check label column.
const CHECK_LABEL: usize = 13;

/// Draws one frame.
pub fn render(app: &App, frame: &mut Frame) {
    let size = frame.area();
    let Some(layout) = areas(size.width, size.height, app.zoom) else {
        render_too_small(frame, size);
        return;
    };
    render_header(app, frame, layout.header);
    match app.tab {
        Tab::Lanes => {
            if let Some(list) = layout.list {
                render_list(app, frame, list);
            }
            render_pane(app, frame, layout.pane, layout.pane_inner);
        }
        Tab::Monitor => render_monitor(app, frame, layout.body),
        Tab::Daemon => render_daemon(app, frame, layout.body),
    }
    render_footer(app, frame, layout.footer);
    if let Some(dialog) = &app.dialog {
        render_dialog(app, dialog, frame, size);
    }
}

/// Cuts styled spans to `width` cells, ending in `…` where text was cut.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut left = width;
    for span in spans {
        if left == 0 {
            break;
        }
        let content = span.content.to_string();
        let span_width = text::width(&content);
        if span_width <= left {
            left -= span_width;
            out.push(span);
        } else {
            out.push(Span::styled(clip(&content, left), span.style));
            break;
        }
    }
    Line::from(out)
}

/// Shown instead of the layout below the minimum size.
fn render_too_small(frame: &mut Frame, area: Rect) {
    let width = usize::from(area.width);
    let text = vec![
        Line::from(clip("terminal too small", width)),
        Line::from(clip(
            &format!(
                "need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
                area.width, area.height
            ),
            width,
        ))
        .style(Style::new().fg(MUTED)),
    ];
    let top = area.height.saturating_sub(2) / 2;
    let area = Rect::new(area.x, area.y + top, area.width, area.height - top);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}

/// Wordmark, tabs, and the notice or connection state in the space left over.
fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let tab = |name: &'static str, active: bool| {
        if active {
            Span::styled(format!(" {name} "), Style::new().fg(ON_PAINT).bg(PAINT))
        } else {
            Span::styled(format!(" {name} "), Style::new().fg(MUTED))
        }
    };
    let left = Line::from(vec![
        Span::styled(" devlanes ", Style::new().add_modifier(Modifier::BOLD)),
        tab("lanes", app.tab == Tab::Lanes),
        tab("monitor", app.tab == Tab::Monitor),
        tab("daemon", app.tab == Tab::Daemon),
    ]);
    let used = u16::try_from(left.width())
        .unwrap_or(area.width)
        .min(area.width);
    frame.render_widget(Paragraph::new(left), area);
    let (message, style) = match (app.connection, &app.notice) {
        _ if app.daemon.busy.is_some() => (
            format!("daemon: {}", app.daemon.busy.unwrap_or_default()),
            Style::new().fg(MUTED),
        ),
        _ if app.daemon.stopped => (
            "daemon stopped · s on the daemon tab starts it".to_owned(),
            Style::new().fg(MUTED),
        ),
        (Connection::Reconnecting { .. }, _) => (
            "daemon unreachable, reconnecting".to_owned(),
            Style::new().fg(FAILED),
        ),
        (Connection::Connecting, _) => ("connecting".to_owned(), Style::new().fg(MUTED)),
        (Connection::Online, Some(notice)) if notice.error => {
            (format!("! {}", notice.text), Style::new().fg(FAILED))
        }
        (Connection::Online, Some(notice)) => (notice.text.clone(), Style::new()),
        (Connection::Online, None) => return,
    };
    // One column of space after the tabs and one at the right edge.
    let room = area.width.saturating_sub(used + 2);
    if room == 0 {
        return;
    }
    let message = clip(&clean(&message), usize::from(room));
    let message_width = u16::try_from(text::width(&message))
        .unwrap_or(room)
        .min(room);
    let slot = Rect::new(area.right() - 1 - message_width, area.y, message_width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(message, style))),
        slot,
    );
}

/// A bordered block whose title is clipped to the block's width.
fn block(title: Vec<Span<'static>>, focused: bool, width: u16) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(if focused { PAINT } else { RULE }))
        .title(fit(title, usize::from(width.saturating_sub(2))))
}

/// The project/lane/unit tree.
fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::List;
    let block = block(vec![Span::raw(" lanes ")], focused, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);
    if app.lanes.is_empty() {
        let mut lines = Vec::new();
        if app.loaded {
            lines.extend(text::wrap("no lanes", width).into_iter().map(Line::from));
            lines.extend(
                text::wrap("run devlanes start in a project", width)
                    .into_iter()
                    .map(|line| Line::from(line).style(Style::new().fg(MUTED))),
            );
        } else {
            lines.push(Line::from("loading").style(Style::new().fg(MUTED)));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    let selected = app.selected_row();
    let lines: Vec<Line> = app
        .rows()
        .iter()
        .enumerate()
        .skip(app.list_offset)
        .take(usize::from(inner.height))
        .map(|(index, row)| {
            let spans = row_spans(app, *row);
            if Some(index) != selected {
                return fit(spans, width);
            }
            let style = if focused {
                Style::new().fg(ON_PAINT).bg(PAINT)
            } else {
                Style::new().fg(PAINT).add_modifier(Modifier::BOLD)
            };
            // Selection restyles every span so state colors do not fight the highlight.
            let joined: String = spans.iter().map(|span| span.content.as_ref()).collect();
            let clipped = clip(&joined, width);
            let pad = width.saturating_sub(text::width(&clipped));
            Line::from(Span::styled(format!("{clipped}{}", " ".repeat(pad)), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One tree row as spans.
fn row_spans(app: &App, row: Row) -> Vec<Span<'static>> {
    match row {
        Row::Project(lane) => vec![Span::styled(
            clean(&app.lanes[lane].project),
            Style::new().add_modifier(Modifier::BOLD),
        )],
        Row::Lane(lane) => lane_spans(app, &app.lanes[lane]),
        Row::Unit(lane, unit) => {
            let lane = &app.lanes[lane];
            let unit = &lane.units[unit];
            let error = app.target_error(&Target::Unit {
                lane: lane.reference.clone(),
                unit: unit.name.clone(),
            });
            unit_spans(unit, error)
        }
    }
}

/// `! start failed`, the row badge for a target with a failed action.
fn failure_badge(entry: &ErrorEntry) -> Span<'static> {
    Span::styled(
        format!("  ! {} failed", clean(entry.verb())),
        Style::new().fg(FAILED),
    )
}

/// `▸ ● @2 feat/checkout  moves to lane 3  idle`
fn lane_spans(app: &App, lane: &LaneSummary) -> Vec<Span<'static>> {
    let arrow = if lane.units.is_empty() {
        "  "
    } else if app.is_expanded(lane) {
        "▾ "
    } else {
        "▸ "
    };
    let (glyph, color) = lane_glyph(&lane.state);
    let index = lane
        .index
        .map_or_else(|| "--".to_owned(), |index| format!("@{index}"));
    let index_style = if lane.index == Some(0) {
        Style::new().fg(PAINT)
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::raw(arrow),
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(index, index_style),
    ];
    if let Some(branch) = &lane.branch {
        spans.push(Span::raw(format!(" {}", clean(branch))));
    }
    // Progress and failed actions come first: they are what the user is looking for.
    if app.is_cleaning(&lane.reference) {
        spans.push(Span::styled("  cleaning…", Style::new().fg(PAINT)));
    }
    if let Some(entry) = app.target_error(&Target::Lane(lane.reference.clone())) {
        spans.push(failure_badge(entry));
    }
    let orphans = lane.units.iter().filter(|unit| is_orphaned(unit)).count();
    if orphans > 0 {
        spans.push(Span::styled(
            format!("  ◆ {orphans} orphaned"),
            Style::new().fg(WARN),
        ));
    }
    let mut badges = Vec::new();
    if let Some(next) = lane.moves_to {
        badges.push(format!("moves to lane {next}"));
    }
    if lane.config_changed {
        badges.push("config changed".to_owned());
    }
    // No badge for `needs_env`: `s` here sends this TUI's environment, which
    // is all such a lane needs; the details pane says so.
    for badge in badges {
        spans.push(Span::styled(format!("  {badge}"), Style::new().fg(MUTED)));
    }
    // The glyph already shows running; other states are spelled out.
    let state = match lane.state.as_str() {
        "running" => None,
        "no-config" => Some("no config".to_owned()),
        other => Some(clean(other)),
    };
    if let Some(state) = state {
        spans.push(Span::styled(format!("  {state}"), Style::new().fg(color)));
    }
    spans
}

/// `    ● api  ready  ! start failed  agent`
fn unit_spans(unit: &UnitSummary, error: Option<&ErrorEntry>) -> Vec<Span<'static>> {
    let (glyph, state_color) = unit_glyph(&unit.state);
    let own = unit.color.as_deref().and_then(palette_color);
    // Failed and orphaned markers keep their state color whatever the unit's
    // color, so both stand out (red for failures, amber for orphans).
    let marker = match own {
        Some(color) if unit.state != "failed" && unit.state != ORPHANED => color,
        _ => state_color,
    };
    let name = own.map_or_else(Style::new, |color| Style::new().fg(color));
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(format!("{glyph} "), Style::new().fg(marker)),
        Span::styled(clean(&unit.name), name),
        Span::styled(
            format!("  {}", unit_state(unit)),
            Style::new().fg(state_color),
        ),
    ];
    if let Some(entry) = error {
        spans.push(failure_badge(entry));
    }
    if unit.kind == "task" {
        spans.push(Span::styled("  task", Style::new().fg(MUTED)));
    }
    if started_by_agent(unit) {
        spans.push(Span::styled(
            "  agent",
            Style::new().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
    }
    spans
}

/// Lowercase state word, with the exit code for failed and exited units and
/// the pid for orphans, which can only be found by it.
pub fn unit_state(unit: &UnitSummary) -> String {
    let state = clean(&unit.state);
    match (state.as_str(), unit.exit_code, unit.pid) {
        ("failed" | "exited", Some(code), _) => format!("{state} {code}"),
        (ORPHANED, _, Some(pid)) => format!("{state} pid {pid}"),
        _ => state,
    }
}

/// Glyph and color per lane state.
fn lane_glyph(state: &str) -> (&'static str, Color) {
    match state {
        "running" => ("●", READY),
        "starting" | "stopping" => ("◐", MUTED),
        "failed" => ("✕", FAILED),
        _ => ("○", MUTED),
    }
}

/// Glyph and color per unit state.
fn unit_glyph(state: &str) -> (&'static str, Color) {
    match state {
        "ready" | "running" => ("●", READY),
        "starting" | "stopping" => ("◐", MUTED),
        "queued" => ("◌", MUTED),
        "failed" => ("✕", FAILED),
        ORPHANED => ("◆", WARN),
        _ => ("○", MUTED),
    }
}

/// A unit color from the daemon's palette (`devlanes_core::color::PALETTE`) as
/// the terminal's own ANSI color, so it follows the user's theme. Unknown
/// names fall back to the default style.
pub fn palette_color(name: &str) -> Option<Color> {
    Some(match name {
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        // ANSI 7 and 8: ratatui calls them Gray and DarkGray.
        "white" => Color::Gray,
        "gray" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        _ => return None,
    })
}

/// The terminal pane, or details when there is no live terminal to show.
fn render_pane(app: &App, frame: &mut Frame, area: Rect, inner: Rect) {
    let focused = app.focus == Focus::Terminal;
    let width = usize::from(inner.width);
    let Some(attached) = &app.attached else {
        if let (Some(lane), Some(unit)) = (app.selected_lane(), app.selected_unit())
            && is_orphaned(unit)
        {
            let title = format!(
                " {} · {} ",
                clean(&unit_ref(&lane.reference, &unit.name)),
                unit_state(unit)
            );
            frame.render_widget(block(vec![Span::raw(title)], false, area.width), area);
            let target = Target::Unit {
                lane: lane.reference.clone(),
                unit: unit.name.clone(),
            };
            let lines = unit_details(app, Some(unit), &target, None, width);
            render_details(app, frame, lines, inner);
            return;
        }
        let title = app.selected_lane().map_or_else(
            || " devlanes ".to_owned(),
            |lane| format!(" {} ", clean(&lane.reference)),
        );
        frame.render_widget(block(vec![Span::raw(title)], false, area.width), area);
        render_details(app, frame, lane_details(app, width), inner);
        return;
    };
    let reference = unit_ref(&attached.lane, &attached.unit);
    let state = app
        .attached_unit()
        .map_or_else(String::new, |unit| format!(" · {}", unit_state(unit)));
    let mut title = vec![Span::raw(format!(" {}{state} ", clean(&reference)))];
    let mode = match (&attached.mode, &attached.state) {
        (PaneMode::Copy(_), _) => Some("copy".to_owned()),
        (PaneMode::Logs(view), _) if view.loading => Some("logs · loading".to_owned()),
        (PaneMode::Logs(_), _) => Some("logs".to_owned()),
        (_, AttachState::Closed) => Some("stream ended".to_owned()),
        (PaneMode::Live, _) if attached.screen.scroll_offset() > 0 => {
            Some(format!("scroll {}", attached.screen.scroll_offset()))
        }
        _ => None,
    };
    if let Some(mode) = mode {
        title.push(Span::styled(format!("{mode} "), Style::new().fg(MUTED)));
    }
    frame.render_widget(block(title, focused, area.width), area);
    let target = Target::Unit {
        lane: attached.lane.clone(),
        unit: attached.unit.clone(),
    };
    match (&attached.state, &attached.mode) {
        (AttachState::Failed(reason), _) => {
            let lines = unit_details(app, app.attached_unit(), &target, Some(reason), width);
            render_details(app, frame, lines, inner);
        }
        (_, PaneMode::Logs(view)) => render_logs(view, frame, inner),
        (_, mode) => {
            let view = attached.screen.view();
            let mut cursor = Cursor::default();
            if !focused {
                cursor.hide();
            }
            frame.render_widget(PseudoTerminal::new(&view).cursor(cursor), inner);
            if let PaneMode::Copy(state) = mode {
                highlight_copy(*state, frame, inner);
            }
            // A failed action on a running unit stays visible over the last row.
            if let Some(entry) = app.target_error(&target)
                && inner.height > 1
            {
                let row = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
                let line = fit(
                    vec![Span::styled(
                        format!(
                            "! {} · e shows the error",
                            clean(&format!("{}: {}", entry.what, entry.worded.summary))
                        ),
                        Style::new().fg(FAILED),
                    )],
                    width,
                );
                frame.render_widget(Clear, row);
                frame.render_widget(Paragraph::new(line), row);
            }
        }
    }
}

/// Draws pre-wrapped detail lines; when they do not fit, the last row says
/// where the rest is.
fn render_details(app: &App, frame: &mut Frame, lines: Vec<Line<'static>>, area: Rect) {
    let more = if app.errors.is_empty() {
        "…"
    } else {
        "… e shows the full error"
    };
    render_clipped(frame, lines, area, more);
}

/// Draws pre-wrapped lines, replacing the last visible row with `more` when
/// they do not fit.
fn render_clipped(
    frame: &mut Frame,
    mut lines: Vec<Line<'static>>,
    area: Rect,
    more: &'static str,
) {
    let height = usize::from(area.height);
    if height > 0 && lines.len() > height {
        lines.truncate(height - 1);
        lines.push(fit(
            vec![Span::styled(more, Style::new().fg(MUTED))],
            usize::from(area.width),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// `label     value`, the value wrapped under itself.
fn field(label: &str, value: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    labeled(label, LABEL, value, width, style)
}

/// [`field`] with a label column of `column` cells; a longer label keeps one space.
fn labeled(
    label: &str,
    column: usize,
    value: &str,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let column = column.max(text::width(label) + usize::from(!label.is_empty()));
    let value_width = width.saturating_sub(column).max(1);
    text::wrap(&clean(value), value_width)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let label = if index == 0 { label } else { "" };
            fit(
                vec![
                    Span::styled(format!("{label:<column$}"), Style::new().fg(MUTED)),
                    Span::styled(line, style),
                ],
                width,
            )
        })
        .collect()
}

/// A worded error under a label: summary, detail with the stderr tail, hint.
fn worded_lines(
    label: &str,
    what: Option<&str>,
    worded: &Worded,
    width: usize,
) -> Vec<Line<'static>> {
    let red = Style::new().fg(FAILED);
    let mut lines = Vec::new();
    match what {
        Some(what) => {
            lines.extend(field(label, &format!("{what} failed"), width, red));
            lines.extend(field("", &worded.summary, width, red));
        }
        None => lines.extend(field(label, &worded.summary, width, red)),
    }
    // Long stderr keeps its last lines, where tools print the actual failure.
    let detail = &worded.detail;
    let shown = if detail.len() > STDERR_TAIL + 1 {
        lines.extend(field("", "…", width, Style::new().fg(MUTED)));
        &detail[detail.len() - STDERR_TAIL..]
    } else {
        &detail[..]
    };
    for line in shown {
        lines.extend(field("", line, width, Style::new()));
    }
    if let Some(command) = &worded.command {
        lines.extend(field(
            "",
            &format!("command: {command}"),
            width,
            Style::new().fg(MUTED),
        ));
    }
    if let Some(hint) = &worded.hint {
        lines.extend(field(
            "",
            &format!("hint: {hint}"),
            width,
            Style::new().fg(MUTED),
        ));
    }
    lines
}

/// Unit facts, its last failed action and why there is no terminal.
fn unit_details(
    app: &App,
    unit: Option<&UnitSummary>,
    target: &Target,
    attach: Option<&Worded>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // An orphan has no stream, so the pane says what it is and how to end it.
    if let Some(unit) = unit.filter(|unit| is_orphaned(unit)) {
        let pid = unit
            .pid
            .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
        let text = format!(
            "orphaned process (pid {pid}) left running by a previous daemon; it has no terminal. press x to terminate it"
        );
        lines.extend(
            text::wrap(&text, width)
                .into_iter()
                .map(|line| Line::from(line).style(Style::new().fg(WARN))),
        );
        lines.push(Line::from(""));
    }
    if let Some(unit) = unit.filter(|unit| !is_orphaned(unit)) {
        lines.extend(field("state", &unit_state(unit), width, Style::new()));
    }
    let entry = app.target_error(target);
    if let Some(entry) = entry {
        lines.extend(worded_lines(
            "error",
            Some(&entry.what),
            &entry.worded,
            width,
        ));
    }
    if let Some(reason) = unit.and_then(|unit| unit.reason.as_ref()) {
        let worded = words::rpc(reason);
        if entry.is_none_or(|entry| entry.worded.code != worded.code) {
            lines.extend(worded_lines("reason", None, &worded, width));
        }
    }
    if let Some(unit) = unit {
        if !unit.urls.is_empty() {
            lines.extend(field("urls", &unit.urls.join(" "), width, Style::new()));
        }
        if !unit.ports.is_empty() {
            let ports: Vec<String> = unit
                .ports
                .iter()
                .map(|(name, port)| format!("{name} {port}"))
                .collect();
            lines.extend(field("ports", &ports.join(", "), width, Style::new()));
        }
    }
    if let Some(attach) = attach {
        lines.extend(field(
            "terminal",
            &attach.summary,
            width,
            Style::new().fg(MUTED),
        ));
    }
    lines.push(Line::from(""));
    let mut hint = "s start · x stop · r restart".to_owned();
    if !app.errors.is_empty() {
        hint.push_str(" · e errors");
    }
    lines.extend(
        text::wrap(&hint, width)
            .into_iter()
            .map(|line| Line::from(line).style(Style::new().fg(MUTED))),
    );
    lines
}

/// Lane facts for the pane when no unit is selected.
fn lane_details(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(lane) = app.selected_lane() else {
        return vec![Line::from(clip("no lane selected", width)).style(Style::new().fg(MUTED))];
    };
    let plain = Style::new();
    let mut lines = Vec::new();
    lines.extend(field("state", &lane.state, width, plain));
    lines.extend(field(
        "branch",
        lane.branch.as_deref().unwrap_or("detached"),
        width,
        plain,
    ));
    lines.extend(field(
        "path",
        &lane.path.display().to_string(),
        width,
        plain,
    ));
    let profiles = lane_profiles(lane);
    if !profiles.is_empty() {
        let label = if profiles.len() == 1 {
            "profile"
        } else {
            "profiles"
        };
        lines.extend(field(label, &profiles.join(", "), width, plain));
    }
    if let Some(domain) = &lane.domain {
        lines.extend(field("domain", domain, width, plain));
    }
    if let Some((first, last)) = lane.port_block {
        lines.extend(field("ports", &format!("{first}-{last}"), width, plain));
    }
    if let Some(next) = lane.moves_to {
        lines.extend(field(
            "moves to",
            &format!("lane {next} at next start"),
            width,
            plain,
        ));
    }
    if lane.config_changed {
        lines.extend(field("config", "changed; restart to apply", width, plain));
    }
    let entry = app.target_error(&Target::Lane(lane.reference.clone()));
    if let Some(entry) = entry {
        lines.extend(worded_lines(
            "error",
            Some(&entry.what),
            &entry.worded,
            width,
        ));
    }
    if let Some(reason) = &lane.reason {
        let worded = words::rpc(reason);
        // The daemon keeps a failed start as the lane's reason; show it once.
        if entry.is_none_or(|entry| entry.worded.code != worded.code) {
            lines.extend(worded_lines("reason", None, &worded, width));
        }
    }
    if lane.needs_env {
        lines.extend(field(
            "env",
            "lost when the daemon restarted; s starts the lane with this TUI's environment",
            width,
            plain,
        ));
    }
    lines.push(Line::from(""));
    let mut hint = "s start · x stop · r restart · t task · p profile · O terminal".to_owned();
    if !app.errors.is_empty() {
        hint.push_str(" · e errors");
    }
    lines.extend(
        text::wrap(&hint, width)
            .into_iter()
            .map(|line| Line::from(line).style(Style::new().fg(MUTED))),
    );
    lines
}

/// Persisted log lines, newest at the bottom, scrolled by `view.offset`.
fn render_logs(view: &LogView, frame: &mut Frame, area: Rect) {
    let height = usize::from(area.height);
    let width = usize::from(area.width);
    if view.lines.is_empty() {
        let text = if view.loading {
            "loading"
        } else {
            "no stored lines"
        };
        frame.render_widget(Paragraph::new(text).style(Style::new().fg(MUTED)), area);
        return;
    }
    let end = view.lines.len().saturating_sub(view.offset);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = view.lines[start..end]
        .iter()
        .map(|line| Line::from(clip(&clean(&line.text), width)))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Reverses the selected cells and marks the copy cursor.
fn highlight_copy(state: CopyState, frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    let cursor = state.cursor;
    let (start, end) = match state.anchor {
        Some(anchor) if anchor <= cursor => (anchor, cursor),
        Some(anchor) => (cursor, anchor),
        None => (cursor, cursor),
    };
    for row in start.0..=end.0 {
        let first = if row == start.0 { start.1 } else { 0 };
        let last = if row == end.0 {
            end.1
        } else {
            area.width.saturating_sub(1)
        };
        for column in first..=last {
            let (x, y) = (area.x + column, area.y + row);
            if x < area.right() && y < area.bottom() {
                let cell = &mut buffer[(x, y)];
                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            }
        }
    }
    let (x, y) = (area.x + cursor.1, area.y + cursor.0);
    if x < area.right() && y < area.bottom() {
        buffer[(x, y)].set_style(Style::new().fg(ON_PAINT).bg(PAINT));
    }
}

/// Resource table with sparklines and a detail graph for the selected unit.
fn render_monitor(app: &App, frame: &mut Frame, area: Rect) {
    let rows = app.monitor_rows();
    let [table_area, detail_area] = if area.height >= 14 {
        Layout::vertical([Constraint::Min(5), Constraint::Length(7)]).areas(area)
    } else {
        [area, Rect::new(area.x, area.bottom(), area.width, 0)]
    };
    let table_block = block(vec![Span::raw(" resources ")], true, table_area.width);
    if rows.is_empty() {
        let inner = table_block.inner(table_area);
        frame.render_widget(table_block, table_area);
        let text = if app.lanes.is_empty() {
            "no lanes"
        } else {
            "nothing running"
        };
        frame.render_widget(Paragraph::new(text).style(Style::new().fg(MUTED)), inner);
        return;
    }
    let spark_width = table_area.width.saturating_sub(48).clamp(4, 30);
    let number = |cpu: Option<f64>, memory: Option<u64>| {
        (
            cpu.map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.0}%")),
            memory.map_or_else(|| "-".to_owned(), format_bytes),
        )
    };
    let body: Vec<TableRow> = rows
        .iter()
        .map(|row| match row {
            MonitorRow::Lane {
                lane,
                cpu_percent,
                memory_bytes,
            } => {
                let (cpu, memory) = number(*cpu_percent, *memory_bytes);
                TableRow::new(vec![
                    clean(lane),
                    "total".to_owned(),
                    cpu,
                    memory,
                    String::new(),
                ])
                .style(Style::new().add_modifier(Modifier::BOLD))
            }
            MonitorRow::Unit(unit) => {
                let history: Vec<f64> = unit.history.iter().map(|point| point.1).collect();
                let (cpu, memory) = number(unit.cpu_percent, unit.memory_bytes);
                TableRow::new(vec![
                    String::new(),
                    clean(&unit.unit),
                    cpu,
                    memory,
                    text_sparkline(&history, usize::from(spark_width), 100.0),
                ])
            }
        })
        .collect();
    let header =
        TableRow::new(["lane", "unit", "cpu", "mem", "cpu history"]).style(Style::new().fg(MUTED));
    let table = Table::new(
        body,
        [
            Constraint::Length(16),
            Constraint::Min(10),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(spark_width),
        ],
    )
    .header(header)
    .block(table_block)
    .row_highlight_style(Style::new().fg(ON_PAINT).bg(PAINT));
    // The table scrolls itself to keep the selected row in view.
    let mut state = TableState::default().with_selected(Some(app.monitor_index));
    frame.render_stateful_widget(table, table_area, &mut state);
    if detail_area.height > 0 {
        match rows.get(app.monitor_index) {
            Some(MonitorRow::Unit(unit)) => render_memory_detail(unit, frame, detail_area),
            Some(MonitorRow::Lane { lane, .. }) => {
                let block = block(
                    vec![Span::raw(format!(" {} ", clean(lane)))],
                    false,
                    detail_area.width,
                );
                let inner = block.inner(detail_area);
                frame.render_widget(block, detail_area);
                frame.render_widget(
                    Paragraph::new("select a unit to see its memory over time")
                        .style(Style::new().fg(MUTED)),
                    inner,
                );
            }
            None => {}
        }
    }
}

/// Memory over time for one unit: the newest samples that fit, on a scale
/// padded around them so steady memory reads as a flat line, not a full block.
/// The baseline (typical memory of recent runs) is drawn when the daemon sends one.
fn render_memory_detail(row: &UnitMetrics, frame: &mut Frame, area: Rect) {
    let current = row
        .memory_bytes
        .map_or_else(|| "-".to_owned(), format_bytes);
    let mut title = vec![Span::raw(format!(
        " {} memory {current} ",
        clean(&unit_ref(&row.lane, &row.unit))
    ))];
    if let Some(baseline) = row.baseline_memory_bytes {
        title.push(Span::styled(
            format!("baseline {} ", format_bytes(baseline)),
            Style::new().fg(MUTED),
        ));
    }
    let block = block(title, false, area.width);
    if row.history.len() < 2 {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("no history yet").style(Style::new().fg(MUTED)),
            inner,
        );
        return;
    }
    // Braille gives two points per column.
    let keep = usize::from(area.width.saturating_sub(12)).max(2) * 2;
    let recent = &row.history[row.history.len().saturating_sub(keep)..];
    let points: Vec<(f64, f64)> = recent
        .iter()
        .map(|(ts, _, memory)| (*ts as f64, *memory as f64))
        .collect();
    let first = points.first().map_or(0.0, |p| p.0);
    let last = points.last().map_or(1.0, |p| p.0).max(first + 1.0);
    let baseline = row.baseline_memory_bytes.map(|b| b as f64);
    let values = points.iter().map(|p| p.1).chain(baseline);
    let low = values.clone().fold(f64::MAX, f64::min);
    let high = values.fold(0.0, f64::max);
    let pad = ((high - low) * 0.15).max(high * 0.05).max(1.0);
    let (bottom, top) = ((low - pad).max(0.0), high + pad);
    let baseline_points: Vec<(f64, f64)> =
        baseline.map_or_else(Vec::new, |b| vec![(first, b), (last, b)]);
    let mut datasets = vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(READY))
            .data(&points),
    ];
    if !baseline_points.is_empty() {
        datasets.push(
            Dataset::default()
                .marker(Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(MUTED))
                .data(&baseline_points),
        );
    }
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(Axis::default().bounds([first, last]))
        .y_axis(
            Axis::default()
                .bounds([bottom, top])
                .labels([format_bytes(bottom as u64), format_bytes(top as u64)])
                .style(Style::new().fg(MUTED)),
        );
    frame.render_widget(chart, area);
}

/// The last `width` samples as block characters scaled to `max(floor, peak)`.
pub fn text_sparkline(values: &[f64], width: usize, floor: f64) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let values = &values[values.len().saturating_sub(width)..];
    let peak = values.iter().copied().fold(floor, f64::max);
    values
        .iter()
        .map(|value| {
            let level = (value / peak * 7.0).round().clamp(0.0, 7.0) as usize;
            BARS[level]
        })
        .collect()
}

/// Binary units with one decimal above a gigabyte.
pub fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let mib = bytes as f64 / MIB;
    if mib >= 1024.0 {
        format!("{:.1}G", mib / 1024.0)
    } else if mib >= 1.0 {
        format!("{mib:.0}M")
    } else {
        format!("{}K", bytes / 1024)
    }
}

/// The daemon tab: facts and doctor checks on top, the daemon log below.
fn render_daemon(app: &App, frame: &mut Frame, area: Rect) {
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let lines = daemon_lines(app, width);
    // The log keeps at least a few rows; small screens split evenly.
    let wanted = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let top = if area.height < 12 {
        (area.height / 2).max(3).min(area.height)
    } else {
        wanted.min(area.height - 7)
    };
    let [info_area, log_area] =
        Layout::vertical([Constraint::Length(top), Constraint::Min(0)]).areas(area);
    let info = block(vec![Span::raw(" daemon ")], true, info_area.width);
    let inner = info.inner(info_area);
    frame.render_widget(info, info_area);
    // Failing checks sort first, so a short screen hides only passing ones.
    render_clipped(
        frame,
        lines,
        inner,
        "… more checks; devlanes doctor lists them all",
    );
    if log_area.height >= 3 {
        render_daemon_log(app, frame, log_area);
    }
}

/// Facts and checks, wrapped to `width`.
fn daemon_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let view = &app.daemon;
    let plain = Style::new();
    let mut lines = Vec::new();
    if let Some(busy) = view.busy {
        lines.extend(field(
            "daemon",
            &format!("{busy}…"),
            width,
            Style::new().fg(PAINT),
        ));
    }
    if view.stopped {
        lines.extend(field(
            "daemon",
            "stopped · press s to start it",
            width,
            Style::new().fg(MUTED),
        ));
        return lines;
    }
    let status = view.status.as_ref();
    let doctor = view.doctor.as_ref();
    if status.is_none() && doctor.is_none() {
        lines.push(Line::from("loading").style(Style::new().fg(MUTED)));
        return lines;
    }
    let source = status.or(doctor).cloned().unwrap_or_default();
    let mut version = format!(
        "{} · pid {}",
        doctor::fact(&source, "version"),
        doctor::fact(&source, "pid")
    );
    if let Some(ms) = status
        .and_then(|s| s.get("uptime_ms"))
        .and_then(serde_json::Value::as_u64)
    {
        version.push_str(&format!(" · up {}", doctor::uptime(ms)));
    }
    lines.extend(field("version", &version, width, plain));
    lines.extend(field(
        "socket",
        &doctor::fact(&source, "socket"),
        width,
        plain,
    ));
    let mut counts = Vec::new();
    if let Some(processes) = view.managed_processes() {
        counts.push(format!("{processes} managed"));
    }
    if let Some(doctor) = doctor {
        counts.push(format!(
            "{} event subscribers",
            doctor::fact(doctor, "subscribers")
        ));
        counts.push(format!("{} projects", doctor::fact(doctor, "projects")));
    }
    if !counts.is_empty() {
        lines.extend(field("processes", &counts.join(" · "), width, plain));
    }
    let Some(doctor) = doctor else {
        return lines;
    };
    lines.push(Line::from(""));
    let mut checks = doctor::checks(doctor);
    checks.sort_by_key(|check| match check.state {
        doctor::CheckState::Fail => 0,
        doctor::CheckState::Warn => 1,
        doctor::CheckState::Ok => 2,
    });
    for check in checks {
        let style = match check.state {
            doctor::CheckState::Ok => Style::new().fg(READY),
            doctor::CheckState::Warn => Style::new().fg(PAINT),
            doctor::CheckState::Fail => Style::new().fg(FAILED),
        };
        let label = format!("{:<4} {}", check.state.word(), check.group);
        let mut first = labeled(&label, CHECK_LABEL, &check.text, width, plain);
        // The state word carries the color; the group stays muted.
        if let Some(line) = first.first_mut()
            && let Some(span) = line.spans.first_mut()
        {
            *span = Span::styled(span.content.clone(), style);
        }
        lines.extend(first);
        if let Some(fix) = &check.fix {
            lines.extend(labeled(
                "",
                CHECK_LABEL,
                &format!("fix: {fix}"),
                width,
                Style::new().fg(MUTED),
            ));
        }
    }
    lines
}

/// The tail of the daemon's log, wrapped; follows the file unless scrolled up.
fn render_daemon_log(app: &App, frame: &mut Frame, area: Rect) {
    let view = &app.daemon;
    let state = if view.scroll == 0 {
        "following ".to_owned()
    } else {
        format!("paused {} lines up · G follows ", view.scroll)
    };
    let title = vec![
        Span::raw(" daemon.log "),
        Span::styled(state, Style::new().fg(MUTED)),
    ];
    let block = block(title, false, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width).max(1);
    let height = usize::from(inner.height);
    if let Some(error) = &view.log_error {
        let lines: Vec<Line> = text::wrap(&format!("cannot read the log: {error}"), width)
            .into_iter()
            .map(|line| Line::from(line).style(Style::new().fg(FAILED)))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if view.log.is_empty() {
        frame.render_widget(
            Paragraph::new("no lines yet").style(Style::new().fg(MUTED)),
            inner,
        );
        return;
    }
    let end = view.log.len().saturating_sub(view.scroll);
    // Wrap only what can be shown: walk back from `end` until the rows are filled.
    let mut rows: Vec<String> = Vec::new();
    for line in view.log.range(..end).rev() {
        let mut wrapped = text::wrap(&clean(line), width);
        wrapped.extend(rows);
        rows = wrapped;
        if rows.len() >= height {
            break;
        }
    }
    let start = rows.len().saturating_sub(height);
    let lines: Vec<Line> = rows.into_iter().skip(start).map(Line::from).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Key hints for the current focus and mode, most important first.
fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let mode = app.attached.as_ref().map(|a| &a.mode);
    let mut hints: Vec<(&str, &str)> = match (app.tab, app.focus, mode) {
        (Tab::Monitor, _, _) => vec![("j/k", "move"), ("tab", "daemon"), ("q", "quit")],
        (Tab::Daemon, _, _) => {
            let mut hints = vec![
                ("j/k", "scroll log"),
                ("R", "restart"),
                ("S", "stop"),
                ("X", "lane gc"),
                ("tab", "lanes"),
                ("q", "quit"),
            ];
            if app.daemon.stopped {
                hints.insert(0, ("s", "start daemon"));
            }
            hints
        }
        (_, Focus::Terminal, _) => vec![("shift+tab", "back to list"), ("wheel", "scroll")],
        (_, _, Some(PaneMode::Copy(_))) => vec![
            ("h/j/k/l", "move"),
            ("v", "select"),
            ("y", "copy"),
            ("esc", "cancel"),
        ],
        (_, _, Some(PaneMode::Logs(_))) => vec![("pgup/pgdn", "scroll"), ("esc", "live")],
        _ => vec![
            ("j/k", "move"),
            ("s", "start"),
            ("x", "stop"),
            ("r", "restart"),
            ("t", "task"),
            ("shift+tab", "terminal"),
            ("z", "zoom"),
            ("o", "url"),
            ("O", "open"),
            ("c", "copy"),
            ("p", "profile"),
            ("C", "clean"),
            ("tab", "monitor"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    if !app.errors.is_empty() && app.focus == Focus::List {
        hints.insert(1, ("e", "errors"));
    }
    // `? help` and `q quit` always fit; others are dropped whole from the end.
    let fixed: Vec<(&str, &str)> = [("?", "help"), ("q", "quit")]
        .into_iter()
        .filter(|hint| hints.contains(hint))
        .collect();
    hints.retain(|hint| !fixed.contains(hint));
    let size = |(key, label): &(&str, &str)| text::width(key) + text::width(label) + 3;
    let budget = usize::from(area.width).saturating_sub(1 + fixed.iter().map(size).sum::<usize>());
    let mut used = 0;
    let mut shown: Vec<(&str, &str)> = Vec::new();
    for hint in hints {
        if used + size(&hint) > budget {
            break;
        }
        used += size(&hint);
        shown.push(hint);
    }
    shown.extend(fixed);
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in shown {
        spans.push(Span::styled(key, Style::new().fg(PAINT)));
        spans.push(Span::styled(format!(" {label}  "), Style::new().fg(MUTED)));
    }
    frame.render_widget(Paragraph::new(fit(spans, usize::from(area.width))), area);
}

/// Every binding, for the help dialog.
const HELP: &[(&str, &str)] = &[
    ("j/k, arrows", "move"),
    ("g / G", "first / last, on every list"),
    ("enter, h/l", "expand or collapse a lane"),
    ("shift+tab", "switch between list and terminal"),
    ("s / x / r", "start / stop / restart unit or lane"),
    ("t", "run a task"),
    ("p", "start the lane with chosen profiles"),
    ("z", "zoom the terminal"),
    ("pgup/pgdn, wheel", "scroll; older lines load from logs"),
    ("c", "copy mode (y copies with OSC 52)"),
    ("o / O", "open the url / open in a new terminal"),
    ("C", "clean the lane (asks you to type its name)"),
    ("e", "recent errors in full"),
    ("tab", "lanes / monitor / daemon, from the list"),
    ("R / S", "daemon tab: restart / stop the daemon"),
    ("X", "daemon tab: lane gc"),
    ("s", "daemon tab: start a stopped daemon"),
    ("q", "quit; everything keeps running"),
];

/// `> [x] web  default  no infra  api, +frontend`: the checkbox and name,
/// then the markers, then the include selectors dimmed.
fn profile_row(item: &ProfileChoice, selected: bool, width: usize) -> Line<'static> {
    let check = if item.checked { "[x]" } else { "[ ]" };
    let cursor = if selected { "> " } else { "  " };
    let profile = &item.profile;
    if selected {
        // One style for the whole row keeps the highlight readable.
        let mut text = format!("{cursor}{check} {}", clean(&profile.name));
        if profile.is_default {
            text.push_str("  default");
        }
        if profile.skips_infra {
            text.push_str("  no infra");
        }
        if !profile.include.is_empty() {
            text.push_str(&format!("  {}", clean(&profile.include.join(", "))));
        }
        let clipped = clip(&text, width);
        let pad = width.saturating_sub(text::width(&clipped));
        return Line::from(Span::styled(
            format!("{clipped}{}", " ".repeat(pad)),
            Style::new().fg(ON_PAINT).bg(PAINT),
        ));
    }
    let mut spans = vec![
        Span::raw(format!("{cursor}{check} ")),
        Span::styled(
            clean(&profile.name),
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ];
    if profile.is_default {
        spans.push(Span::styled("  default", Style::new().fg(MUTED)));
    }
    if profile.skips_infra {
        spans.push(Span::styled("  no infra", Style::new().fg(PAINT)));
    }
    if !profile.include.is_empty() {
        spans.push(Span::styled(
            format!("  {}", clean(&profile.include.join(", "))),
            Style::new().fg(MUTED).add_modifier(Modifier::DIM),
        ));
    }
    fit(spans, width)
}

/// The end of typed input that fits in `width` cells: the end is what the
/// user is typing, so it stays visible.
fn tail_fit(input: &str, width: usize) -> String {
    let mut used = 0;
    let mut kept: Vec<char> = Vec::new();
    for c in input.chars().rev() {
        let w = text::width(c.encode_utf8(&mut [0; 4]));
        if used + w > width {
            break;
        }
        used += w;
        kept.push(c);
    }
    kept.into_iter().rev().collect()
}

/// First index of a `room`-row window over `len` items that shows `selected`.
fn window(len: usize, selected: usize, room: usize) -> usize {
    if len <= room {
        return 0;
    }
    selected.saturating_sub(room / 2).min(len - room)
}

/// Fits a dialog into `rows`: the tail always shows; the body loses its last
/// lines, the final kept one replaced by `…`.
fn compact(
    mut body: Vec<Line<'static>>,
    tail: Vec<Line<'static>>,
    rows: usize,
) -> Vec<Line<'static>> {
    // A blank line separates body and tail when there is room for it.
    if body.len() + tail.len() < rows && !body.is_empty() {
        body.push(Line::from(""));
    }
    if body.len() + tail.len() <= rows {
        body.extend(tail);
        return body;
    }
    if tail.len() >= rows {
        let skip = tail.len() - rows;
        return tail.into_iter().skip(skip).collect();
    }
    let room = rows - tail.len();
    body.truncate(room.saturating_sub(1));
    body.push(Line::from("…").style(Style::new().fg(MUTED)));
    body.extend(tail);
    body
}

/// Modal dialogs over a cleared box.
fn render_dialog(app: &App, dialog: &Dialog, frame: &mut Frame, area: Rect) {
    if let Dialog::Errors { scroll } = dialog {
        render_errors(app, *scroll, frame);
        return;
    }
    // Text wraps to the widest box that fits, leaving a column of margin each side.
    let max = usize::from(area.width.saturating_sub(6)).max(1);
    let muted = Style::new().fg(MUTED);
    let wrapped = |text: &str, style: Style| -> Vec<Line<'static>> {
        text::wrap(&clean(text), max)
            .into_iter()
            .map(|line| Line::from(line).style(style))
            .collect()
    };
    // Explanations read better in a narrow box than across a wide screen.
    let narrow = max.min(64);
    let explained = |prompt: &str, detail: &[String]| -> Vec<Line<'static>> {
        let mut lines: Vec<Line> = text::wrap(&clean(prompt), narrow)
            .into_iter()
            .map(|line| Line::from(line).style(Style::new().add_modifier(Modifier::BOLD)))
            .collect();
        for sentence in detail {
            lines.extend(
                text::wrap(&clean(sentence), narrow)
                    .into_iter()
                    .map(Line::from),
            );
        }
        lines
    };
    // Rows inside the border; lists are windowed to fit next to the tail.
    let rows = usize::from(area.height.saturating_sub(2)).max(1);
    // `body` may be cut to fit; `tail` (input line, key hints) always stays.
    let (title, body, tail): (&str, Vec<Line>, Vec<Line>) = match dialog {
        Dialog::Errors { .. } => return,
        Dialog::Confirm {
            prompt,
            detail,
            action,
            strict,
            ..
        } => {
            let keys = if *strict {
                format!("y {action} · n or esc cancels")
            } else {
                format!("y or enter {action} · n or esc cancels")
            };
            let tail = text::wrap(&keys, narrow)
                .into_iter()
                .map(|line| Line::from(line).style(muted))
                .collect();
            (" confirm ", explained(prompt, detail), tail)
        }
        Dialog::TypeConfirm {
            prompt,
            detail,
            expected,
            input,
            mismatch,
            ..
        } => {
            let mut tail: Vec<Line> =
                text::wrap(&format!("type {} and press enter", clean(expected)), narrow)
                    .into_iter()
                    .map(Line::from)
                    .collect();
            let input = clean(input);
            let shown = tail_fit(&input, narrow.saturating_sub(3));
            tail.push(Line::from(vec![
                Span::styled("> ", Style::new().fg(MUTED)),
                Span::styled(shown, Style::new().fg(PAINT)),
                Span::raw("▏"),
            ]));
            if *mismatch {
                tail.extend(
                    text::wrap("that is not the lane name", narrow)
                        .into_iter()
                        .map(|line| Line::from(line).style(Style::new().fg(FAILED))),
                );
            }
            tail.extend(
                text::wrap("esc cancels", narrow)
                    .into_iter()
                    .map(|line| Line::from(line).style(muted)),
            );
            (" confirm ", explained(prompt, detail), tail)
        }
        Dialog::Tasks { lane, tasks, index } => {
            let mut body = wrapped(&format!("tasks in {lane}"), Style::new());
            let hint = wrapped("enter choose · esc cancel", muted);
            let room = rows.saturating_sub(body.len() + hint.len()).max(1);
            let start = window(tasks.len(), *index, room);
            body.extend(
                tasks
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(room)
                    .map(|(i, task)| {
                        let task = clip(&clean(task), max.saturating_sub(2));
                        if i == *index {
                            Line::from(format!("> {task}"))
                                .style(Style::new().fg(ON_PAINT).bg(PAINT))
                        } else {
                            Line::from(format!("  {task}"))
                        }
                    }),
            );
            (" run task ", body, hint)
        }
        Dialog::Profiles { lane, state } => {
            let mut body = wrapped(&format!("start {lane} with:"), Style::new());
            let tail = match state {
                ProfilesState::Loading => {
                    body.extend(wrapped("loading profiles", muted));
                    wrapped("esc cancels", muted)
                }
                ProfilesState::Unsupported => {
                    body.extend(wrapped("profiles need a newer daemon", muted));
                    wrapped("esc closes", muted)
                }
                ProfilesState::Ready { items, .. } if items.is_empty() => {
                    body.extend(wrapped("this lane's config defines no profiles", muted));
                    wrapped("esc closes", muted)
                }
                ProfilesState::Ready { items, index } => {
                    let mut tail = wrapped("space toggles · enter starts · esc cancels", muted);
                    // Matches what Enter sends with nothing checked.
                    let empty = if items.iter().any(|item| item.profile.is_default) {
                        "nothing checked starts the default profile"
                    } else {
                        "nothing checked starts the lane without a profile"
                    };
                    tail.extend(wrapped(empty, muted));
                    let width = max.min(72);
                    let room = rows.saturating_sub(body.len() + tail.len()).max(1);
                    let start = window(items.len(), *index, room);
                    body.extend(
                        items
                            .iter()
                            .enumerate()
                            .skip(start)
                            .take(room)
                            .map(|(i, item)| profile_row(item, i == *index, width)),
                    );
                    tail
                }
            };
            (" profiles ", body, tail)
        }
        Dialog::Help => {
            let mut body: Vec<Line> = HELP
                .iter()
                .map(|(key, what)| {
                    fit(
                        vec![
                            Span::styled(format!("{key:<18}"), Style::new().fg(PAINT)),
                            Span::raw(*what),
                        ],
                        max,
                    )
                })
                .collect();
            body.push(Line::from(""));
            body.extend(wrapped(
                "shift+tab is reserved for focus; apps in the pane don't receive it",
                muted,
            ));
            (" keys ", body, wrapped("any key closes", muted))
        }
    };
    let lines = compact(body, tail, rows);
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = u16::try_from(content_width + 4)
        .unwrap_or(area.width)
        .clamp(20.min(area.width), area.width);
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(area.height)
        .min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(block(vec![Span::raw(title)], true, width)),
        rect,
    );
}

/// The error list: every recent error with its full wrapped text, scrollable.
fn render_errors(app: &App, scroll: usize, frame: &mut Frame) {
    let rect = error_panel_area(app.size).intersection(frame.area());
    let title = vec![
        Span::raw(" errors "),
        Span::styled("j/k scroll · esc close ", Style::new().fg(MUTED)),
    ];
    let block = block(title, true, rect.width);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    let width = usize::from(inner.width).max(1);
    let lines = app.error_panel_lines(width);
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new("no errors").style(Style::new().fg(MUTED)),
            inner,
        );
        return;
    }
    let height = usize::from(inner.height);
    let start = scroll.min(lines.len().saturating_sub(height));
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(start)
        .take(height)
        .map(|line| match line {
            PanelLine::Title(text) => {
                Line::from(text).style(Style::new().add_modifier(Modifier::BOLD))
            }
            PanelLine::Summary(text) => Line::from(text).style(Style::new().fg(FAILED)),
            PanelLine::Detail(text) => Line::from(text),
            PanelLine::Hint(text) => Line::from(text).style(Style::new().fg(MUTED)),
            PanelLine::Blank => Line::from(""),
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}
