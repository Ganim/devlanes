//! Rendering. Reads [`App`] and draws; never mutates state.

use devlanes_proto::{LaneSummary, UnitSummary};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Row as TableRow, Sparkline, Table, Wrap,
    },
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::api::UnitMetrics;
use crate::app::{
    App, AttachState, Connection, CopyState, Dialog, Focus, LogView, MIN_HEIGHT, MIN_WIDTH,
    PaneMode, Row, Tab, areas, started_by_agent, unit_ref,
};

/// Brand palette A (brand/design.md): yellow means "this one", never a status.
const PAINT: Color = Color::Rgb(0xf5, 0xc5, 0x42);
const ON_PAINT: Color = Color::Rgb(0x1a, 0x19, 0x17);
const MUTED: Color = Color::Rgb(0xa6, 0xa3, 0x99);
const RULE: Color = Color::Rgb(0x3d, 0x3b, 0x36);
const READY: Color = Color::Rgb(0x3e, 0xa6, 0x6b);
const FAILED: Color = Color::Rgb(0xd9, 0x4f, 0x3d);

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
    }
    render_footer(app, frame, layout.footer);
    if let Some(dialog) = &app.dialog {
        render_dialog(dialog, frame, size);
    }
}

/// Shown instead of the layout below the minimum size.
fn render_too_small(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from("terminal too small"),
        Line::from(format!(
            "need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
            area.width, area.height
        ))
        .style(Style::new().fg(MUTED)),
    ];
    let top = area.height.saturating_sub(2) / 2;
    let area = Rect::new(area.x, area.y + top, area.width, area.height - top);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}

/// Wordmark, tabs, and the notice or connection state on the right.
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
    ]);
    frame.render_widget(Paragraph::new(left), area);
    let right = match (app.connection, &app.notice) {
        (Connection::Reconnecting { .. }, _) => {
            Span::styled("daemon unreachable, reconnecting ", Style::new().fg(FAILED))
        }
        (Connection::Connecting, _) => Span::styled("connecting ", Style::new().fg(MUTED)),
        (Connection::Online, Some(notice)) => Span::raw(format!("{notice} ")),
        (Connection::Online, None) => Span::raw(""),
    };
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );
}

/// A bordered block; the focused one gets the paint border.
fn block(title: Line<'static>, focused: bool) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(if focused { PAINT } else { RULE }))
        .title(title)
}

/// The project/lane/unit tree.
fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::List;
    let block = block(Line::from(" lanes "), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.lanes.is_empty() {
        let text = if app.loaded {
            vec![
                Line::from("no lanes"),
                Line::from("run devlanes start in a project").style(Style::new().fg(MUTED)),
            ]
        } else {
            vec![Line::from("loading").style(Style::new().fg(MUTED))]
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
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
            let line = row_line(app, *row);
            if Some(index) != selected {
                return line;
            }
            let style = if focused {
                Style::new().fg(ON_PAINT).bg(PAINT)
            } else {
                Style::new().fg(PAINT).add_modifier(Modifier::BOLD)
            };
            // Selection restyles every span so state colors do not fight the highlight.
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            let width = usize::from(inner.width);
            Line::from(Span::styled(format!("{text:<width$}"), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One tree row.
fn row_line(app: &App, row: Row) -> Line<'static> {
    match row {
        Row::Project(lane) => Line::from(Span::styled(
            app.lanes[lane].project.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Row::Lane(lane) => lane_line(app, &app.lanes[lane]),
        Row::Unit(lane, unit) => unit_line(&app.lanes[lane].units[unit]),
    }
}

/// `▸ ● @2 feat/checkout  idle · s start  config changed`
fn lane_line(app: &App, lane: &LaneSummary) -> Line<'static> {
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
        spans.push(Span::raw(format!(" {branch}")));
    }
    // Badges come before the state word: they are rarer and get truncated last.
    let mut badges = Vec::new();
    if let Some(next) = lane.moves_to {
        badges.push(format!("moves to lane {next}"));
    }
    if lane.config_changed {
        badges.push("config changed".to_owned());
    }
    if lane.needs_env {
        badges.push("needs start from a shell".to_owned());
    }
    for badge in badges {
        spans.push(Span::styled(format!("  {badge}"), Style::new().fg(MUTED)));
    }
    // The glyph already shows running; other states are spelled out.
    let state = match lane.state.as_str() {
        "running" => None,
        "no-config" => Some("no config"),
        other => Some(other),
    };
    if let Some(state) = state {
        spans.push(Span::styled(format!("  {state}"), Style::new().fg(color)));
    }
    Line::from(spans)
}

/// `    ● api  ready  agent`
fn unit_line(unit: &UnitSummary) -> Line<'static> {
    let (glyph, color) = unit_glyph(&unit.state);
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::raw(unit.name.clone()),
        Span::styled(format!("  {}", unit_state(unit)), Style::new().fg(color)),
    ];
    if unit.kind == "task" {
        spans.push(Span::styled("  task", Style::new().fg(MUTED)));
    }
    if started_by_agent(unit) {
        spans.push(Span::styled(
            "  agent",
            Style::new().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// Lowercase state word, with the exit code for failed and exited units.
pub fn unit_state(unit: &UnitSummary) -> String {
    match (unit.state.as_str(), unit.exit_code) {
        ("failed" | "exited", Some(code)) => format!("{} {code}", unit.state),
        (state, _) => state.to_owned(),
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
        "failed" => ("✕", FAILED),
        _ => ("○", MUTED),
    }
}

/// The terminal pane, or lane details when a lane row is selected.
fn render_pane(app: &App, frame: &mut Frame, area: Rect, inner: Rect) {
    let focused = app.focus == Focus::Terminal;
    let Some(attached) = &app.attached else {
        let title = app.selected_lane().map_or_else(
            || " devlanes ".to_owned(),
            |lane| format!(" {} ", lane.reference),
        );
        frame.render_widget(block(Line::from(title), false), area);
        frame.render_widget(Paragraph::new(lane_details(app)), inner);
        return;
    };
    let state = app
        .attached_unit()
        .map_or_else(String::new, |unit| format!(" · {}", unit_state(unit)));
    let mut title = vec![Span::raw(format!(
        " {}{state} ",
        unit_ref(&attached.lane, &attached.unit)
    ))];
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
    frame.render_widget(block(Line::from(title), focused), area);
    match (&attached.state, &attached.mode) {
        (AttachState::Failed(reason), _) => {
            let text = vec![
                Line::from("no terminal for this unit"),
                Line::from(reason.clone()).style(Style::new().fg(MUTED)),
            ];
            frame.render_widget(Paragraph::new(text), inner);
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
        }
    }
}

/// Lane facts for the pane when no unit is selected.
fn lane_details(app: &App) -> Vec<Line<'static>> {
    let Some(lane) = app.selected_lane() else {
        return vec![Line::from("no lane selected").style(Style::new().fg(MUTED))];
    };
    let field = |name: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name:<10}"), Style::new().fg(MUTED)),
            Span::raw(value),
        ])
    };
    let mut lines = vec![
        field("state", lane.state.clone()),
        field(
            "branch",
            lane.branch.clone().unwrap_or_else(|| "detached".into()),
        ),
        field("path", lane.path.display().to_string()),
    ];
    if let Some(profile) = &lane.profile {
        lines.push(field("profile", profile.clone()));
    }
    if let Some(domain) = &lane.domain {
        lines.push(field("domain", domain.clone()));
    }
    if let Some((first, last)) = lane.port_block {
        lines.push(field("ports", format!("{first}-{last}")));
    }
    if let Some(next) = lane.moves_to {
        lines.push(field("moves to", format!("lane {next} at next start")));
    }
    if lane.config_changed {
        lines.push(field("config", "changed; restart to apply".into()));
    }
    if let Some(reason) = &lane.reason {
        lines.push(field("reason", reason.code.replace('_', " ")));
    }
    if lane.needs_env {
        lines.push(field("env", "run devlanes start from a shell".into()));
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from("s start · x stop · r restart · t task · p profile · O terminal")
            .style(Style::new().fg(MUTED)),
    );
    lines
}

/// Persisted log lines, newest at the bottom, scrolled by `view.offset`.
fn render_logs(view: &LogView, frame: &mut Frame, area: Rect) {
    let height = usize::from(area.height);
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
        .map(|line| Line::from(line.text.clone()))
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
    let table_block = block(Line::from(" resources "), true);
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
    let body: Vec<TableRow> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let cpu_history: Vec<f64> = row.history.iter().map(|point| point.1).collect();
            let cells = vec![
                row.lane.clone(),
                row.unit.clone(),
                row.cpu_percent
                    .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.0}%")),
                row.memory_bytes
                    .map_or_else(|| "-".to_owned(), format_bytes),
                text_sparkline(&cpu_history, usize::from(spark_width), 100.0),
            ];
            let style = if index == app.monitor_index {
                Style::new().fg(ON_PAINT).bg(PAINT)
            } else {
                Style::new()
            };
            TableRow::new(cells).style(style)
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
    .block(table_block);
    frame.render_widget(table, table_area);
    if detail_area.height > 0
        && let Some(row) = rows.get(app.monitor_index)
    {
        render_memory_detail(row, frame, detail_area);
    }
}

/// Memory over time for one unit.
fn render_memory_detail(row: &UnitMetrics, frame: &mut Frame, area: Rect) {
    let current = row
        .memory_bytes
        .map_or_else(|| "-".to_owned(), format_bytes);
    let title = format!(" {}:{} memory {current} ", row.lane, row.unit);
    let block = block(Line::from(title), false);
    if row.history.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("no history yet").style(Style::new().fg(MUTED)),
            inner,
        );
        return;
    }
    let data: Vec<u64> = row.history.iter().map(|point| point.2).collect();
    frame.render_widget(
        Sparkline::default()
            .block(block)
            .data(&data)
            .style(Style::new().fg(READY)),
        area,
    );
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

/// Key hints for the current focus and mode.
fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let hints: &[(&str, &str)] = match (app.tab, app.focus, app.attached.as_ref().map(|a| &a.mode))
    {
        (Tab::Monitor, _, _) => &[("j/k", "move"), ("tab", "lanes"), ("q", "quit")],
        (_, Focus::Terminal, _) => &[("C-a", "back to list"), ("wheel", "scroll")],
        (_, _, Some(PaneMode::Copy(_))) => &[
            ("h/j/k/l", "move"),
            ("v", "select"),
            ("y", "copy"),
            ("esc", "cancel"),
        ],
        (_, _, Some(PaneMode::Logs(_))) => &[("pgup/pgdn", "scroll"), ("esc", "live")],
        _ => &[
            ("j/k", "move"),
            ("s", "start"),
            ("x", "stop"),
            ("r", "restart"),
            ("t", "task"),
            ("C-a", "terminal"),
            ("z", "zoom"),
            ("o", "url"),
            ("O", "open"),
            ("c", "copy"),
            ("p", "profile"),
            ("tab", "monitor"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    // Hints are ordered by importance; the ones that do not fit are dropped whole.
    for (key, label) in hints {
        let width = key.chars().count() + label.chars().count() + 3;
        if used + width > usize::from(area.width) {
            break;
        }
        used += width;
        spans.push(Span::styled(*key, Style::new().fg(PAINT)));
        spans.push(Span::styled(format!(" {label}  "), Style::new().fg(MUTED)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Every binding, for the help dialog.
const HELP: &[(&str, &str)] = &[
    ("j/k, arrows", "move"),
    ("enter, h/l", "expand or collapse a lane"),
    ("C-a", "switch between list and terminal"),
    ("s / x / r", "start / stop / restart unit or lane"),
    ("t", "run a task"),
    ("p", "start the lane with a profile"),
    ("z", "zoom the terminal"),
    ("pgup/pgdn, wheel", "scroll; older lines load from logs"),
    ("c", "copy mode (y copies with OSC 52)"),
    ("o", "open the unit's url"),
    ("O", "open the unit in a new terminal"),
    ("tab", "lanes / monitor"),
    ("q", "quit; everything keeps running"),
];

/// Modal dialogs over a cleared, centered box.
fn render_dialog(dialog: &Dialog, frame: &mut Frame, area: Rect) {
    let (title, lines): (&str, Vec<Line>) = match dialog {
        Dialog::Confirm { prompt, .. } => (
            " confirm ",
            vec![
                Line::from(prompt.clone()),
                Line::from(""),
                Line::from("y run · n cancel").style(Style::new().fg(MUTED)),
            ],
        ),
        Dialog::Tasks { lane, tasks, index } => {
            let mut lines = vec![Line::from(format!("tasks in {lane}"))];
            lines.extend(tasks.iter().enumerate().map(|(i, task)| {
                if i == *index {
                    Line::from(format!("> {task}")).style(Style::new().fg(ON_PAINT).bg(PAINT))
                } else {
                    Line::from(format!("  {task}"))
                }
            }));
            lines.push(Line::from(""));
            lines.push(Line::from("enter choose · esc cancel").style(Style::new().fg(MUTED)));
            (" run task ", lines)
        }
        Dialog::Profile { lane, input } => (
            " profile ",
            vec![
                Line::from(format!("start {lane} with profile:")),
                Line::from(vec![
                    Span::styled(input.clone(), Style::new().fg(PAINT)),
                    Span::raw("▏"),
                ]),
                Line::from(""),
                Line::from("enter start · empty for default · esc cancel")
                    .style(Style::new().fg(MUTED)),
            ],
        ),
        Dialog::Help => (
            " keys ",
            HELP.iter()
                .map(|(key, what)| {
                    Line::from(vec![
                        Span::styled(format!("{key:<18}"), Style::new().fg(PAINT)),
                        Span::raw(*what),
                    ])
                })
                .chain([
                    Line::from(""),
                    Line::from("any key closes").style(Style::new().fg(MUTED)),
                ])
                .collect(),
        ),
    };
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .saturating_add(4)
        .min(usize::from(area.width));
    let width = u16::try_from(width)
        .unwrap_or(area.width)
        .max(20)
        .min(area.width);
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
        Paragraph::new(lines).block(block(Line::from(title), true)),
        rect,
    );
}
