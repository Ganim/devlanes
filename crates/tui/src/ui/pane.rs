//! The pane: the selected unit's terminal, or what the selection is about.
//! No box: a header row names it, the content follows.

use devlanes_proto::{LaneSummary, UnitSummary};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use tui_term::widget::{Cursor, PseudoTerminal};

use super::list::{container_look, lane_name, lane_summary};
use super::{fit, spread, target_error, unit_look};
use crate::app::{
    App, AttachState, Attached, CopyState, ErrorEntry, Focus, LogView, PaneMode, is_orphaned,
    lane_profiles, started_by_agent, unit_ref,
};
use crate::screen::ScreenView;
use crate::text::{self, clean, clip};
use crate::theme::{Glyph, Theme, Tone};
use crate::tree::{self, Role, RowId};
use crate::words::{self, STDERR_TAIL, Worded};

/// Width of the label column in facts.
const LABEL: usize = 10;

/// The pane's box and its content.
pub fn render(app: &App, frame: &mut Frame, boxed: Rect, area: Rect) {
    let Some(lane) = app.selected_lane() else {
        render_header(
            app,
            frame,
            boxed,
            vec![Span::styled("nothing selected", app.theme.quiet())],
            Vec::new(),
        );
        return;
    };
    let width = usize::from(area.width).max(1);
    let theme = &app.theme;
    match app.selected_id().cloned() {
        Some(RowId::Unit(name)) => match lane.units.iter().find(|u| u.name == name) {
            Some(unit) => render_unit(app, lane, unit, frame, boxed, area),
            None => render_header(app, frame, boxed, title(&name, "", app), Vec::new()),
        },
        Some(RowId::Task(name)) => {
            let look = super::task_look(theme, app.task_info(&lane.reference, &name), app.spin);
            let mut facts = vec![Span::styled(look.word, theme.tone(look.word_tone))];
            if !look.detail.is_empty() {
                facts.push(Span::styled(look.detail, theme.quiet()));
            }
            render_header(app, frame, boxed, title(&name, "", app), dotted(app, facts));
            details(frame, task_lines(app, lane, &name, width), area, app);
        }
        Some(RowId::Container(name)) => {
            // Shared containers may come from another lane of the project.
            let container = app.find_container(lane, &name);
            let facts = container
                .as_ref()
                .map(|c| {
                    let (_, tone, word) = container_look(app, Some(lane), c);
                    let quiet = if tone == Tone::Frozen {
                        Tone::Quiet
                    } else {
                        tone
                    };
                    let mut facts = vec![Span::styled(word, theme.tone(quiet))];
                    if c.shared {
                        facts.push(Span::styled("shared", theme.quiet()));
                    }
                    if c.hidden {
                        facts.push(Span::styled("hidden", theme.quiet()));
                    }
                    if !c.ports.is_empty() {
                        let ports: Vec<String> = c.ports.values().map(u16::to_string).collect();
                        facts.push(Span::styled(ports.join(" "), theme.quiet()));
                    }
                    dotted(app, facts)
                })
                .unwrap_or_default();
            let attached = app
                .attached
                .as_ref()
                .filter(|a| a.container && a.lane == lane.reference && a.unit == name);
            let right = attached
                .and_then(|a| container_mode_text(app, a))
                .unwrap_or_default();
            render_box(app, frame, boxed, title(&name, "", app), facts, right);
            match attached {
                Some(attached) if !matches!(attached.state, AttachState::Failed(_)) => {
                    render_terminal(app, attached, frame, area, false);
                    if let PaneMode::Copy(state) = &attached.mode {
                        highlight_copy(theme, *state, frame, area);
                    }
                }
                // No logs to show: the facts, and why there are no logs.
                _ => {
                    let mut lines = Vec::new();
                    if let Some(AttachState::Failed(reason)) = attached.map(|a| &a.state) {
                        lines.extend(field(
                            theme,
                            "logs",
                            &reason.summary,
                            width,
                            theme.tone(Tone::Failure),
                        ));
                    }
                    if let Some(container) = &container {
                        lines.extend(container_lines(app, container, width));
                    }
                    details(frame, lines, area, app);
                }
            }
        }
        Some(RowId::Section(_) | RowId::Group(..)) => {
            let row = app
                .rows()
                .into_iter()
                .find(|row| Some(&row.id) == app.selected_id());
            let label = match app.selected_id() {
                Some(RowId::Section(section)) => section.name().to_owned(),
                Some(RowId::Group(_, group)) => clean(group),
                _ => String::new(),
            };
            render_header(app, frame, boxed, title(&label, "", app), Vec::new());
            if let Some(row) = row {
                details(
                    frame,
                    members_table(app, lane, &row.members, width),
                    area,
                    app,
                );
            }
        }
        Some(RowId::WholeLane | RowId::Gap(_)) | None => {
            // `feat/checkout @1 · starting`, as the lane mockups.
            let mut name = title(&lane_name(lane), "", app);
            if let Some(index) = lane.index {
                name.push(Span::raw(format!(" @{index}")));
            }
            render_header(app, frame, boxed, name, lane_summary(app, lane));
            details(frame, lane_lines(app, lane, width), area, app);
        }
    }
}

/// `Gateway API  api-gateway`: the label bold, the second name when it
/// differs, dim.
fn title(label: &str, name: &str, app: &App) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        clean(label),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if !name.is_empty() && clean(label) != clean(name) {
        spans.push(Span::styled(
            format!("  {}", clean(name)),
            app.theme.quiet(),
        ));
    }
    spans
}

/// The pane's box: `title · facts` in the top border, the pane's mode at
/// its right end, the border in the focus color while typing.
fn render_header(
    app: &App,
    frame: &mut Frame,
    boxed: Rect,
    title: Vec<Span<'static>>,
    facts: Vec<Span<'static>>,
) {
    render_box(app, frame, boxed, title, facts, Vec::new());
}

/// [`render_header`] with a right-hand title.
fn render_box(
    app: &App,
    frame: &mut Frame,
    boxed: Rect,
    title: Vec<Span<'static>>,
    facts: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) {
    // `Gateway API  api-gateway · ready`, `wrk-export  failed · exit 1`.
    let named_twice = title.len() > 1;
    // Facts drop from the end (the URL first) before the name is cut, and
    // before the mode on the right goes.
    let mut facts = facts;
    let room = usize::from(boxed.width).saturating_sub(super::spans_width(&right) + 10);
    // The first fact (the state, with an idle unit's glyph before it) always stays.
    let first = facts
        .iter()
        .position(|span| span.content == " · ")
        .unwrap_or(facts.len())
        .max(1);
    while facts.len() > first && super::spans_width(&title) + super::spans_width(&facts) + 3 > room
    {
        facts.truncate(facts.len().saturating_sub(2));
    }
    let mut left = vec![Span::raw(" ")];
    left.extend(title);
    if !facts.is_empty() {
        let joint = if named_twice { " · " } else { "  " };
        left.push(Span::styled(joint, app.theme.quiet()));
        left.extend(facts);
    }
    left.push(Span::raw(" "));
    let mut padded = Vec::new();
    if !right.is_empty() {
        padded.push(Span::raw(" "));
        padded.extend(right);
        padded.push(Span::raw(" "));
    }
    let typing = app.focus == Focus::Terminal;
    super::draw_box(app, frame, boxed, left, padded, typing);
}

/// `a · b` spans from parts.
fn dotted(app: &App, parts: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for part in parts {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", app.theme.quiet()));
        }
        spans.push(part);
    }
    spans
}

/// Lines in the content area, one column in; the last row says when more
/// did not fit.
fn details(frame: &mut Frame, mut lines: Vec<Line<'static>>, area: Rect, app: &App) {
    let height = usize::from(area.height);
    let width = usize::from(area.width);
    if height > 0 && lines.len() > height {
        lines.truncate(height - 1);
        lines.push(fit(
            vec![Span::styled("… z shows the rest", app.theme.quiet())],
            width,
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// `label     value`, the value wrapped under itself.
fn field(
    theme: &Theme,
    label: &str,
    value: &str,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let column = LABEL.max(text::width(label) + usize::from(!label.is_empty()));
    let value_width = width.saturating_sub(column).max(1);
    text::wrap(&clean(value), value_width)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let label = if index == 0 { label } else { "" };
            fit(
                vec![
                    Span::styled(format!("{label:<column$}"), theme.quiet()),
                    Span::styled(line, style),
                ],
                width,
            )
        })
        .collect()
}

/// A sentence, wrapped, with an indent after the first line.
fn sentence(
    text: &str,
    width: usize,
    first: Vec<Span<'static>>,
    style: Style,
) -> Vec<Line<'static>> {
    let lead = super::spans_width(&first);
    let mut lines = Vec::new();
    for (index, line) in text::wrap(&clean(text), width.saturating_sub(lead).max(1))
        .into_iter()
        .enumerate()
    {
        let mut spans = if index == 0 {
            first.clone()
        } else {
            vec![Span::raw(" ".repeat(lead))]
        };
        spans.push(Span::styled(line, style));
        lines.push(Line::from(spans));
    }
    lines
}

/// A banner: a glyph and one sentence, then indented detail lines.
fn banner(
    theme: &Theme,
    glyph: Glyph,
    tone: Tone,
    headline: &str,
    rest: &[String],
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = sentence(
        headline,
        width,
        vec![Span::styled(
            format!("{} ", theme.glyph(glyph)),
            theme.tone(tone),
        )],
        Style::new(),
    );
    for line in rest {
        lines.extend(sentence(line, width, vec![Span::raw("  ")], Style::new()));
    }
    lines
}

/// The banner for a worded error: the summary, the end of its stderr, the
/// command that failed and the command to run.
fn error_banner(theme: &Theme, what: &str, worded: &Worded, width: usize) -> Vec<Line<'static>> {
    let headline = format!("{} failed: {}", capitalized(what), worded.summary);
    let mut lines = sentence(
        &headline,
        width,
        vec![Span::styled(
            format!("{} ", theme.glyph(Glyph::Failed)),
            theme.tone(Tone::Failure),
        )],
        Style::new(),
    );
    // Long stderr keeps its last lines, where tools print the actual failure.
    let detail = &worded.detail;
    let shown = if detail.len() > STDERR_TAIL + 1 {
        lines.push(Line::styled("  …", theme.quiet()));
        &detail[detail.len() - STDERR_TAIL..]
    } else {
        &detail[..]
    };
    for line in shown {
        lines.extend(sentence(line, width, vec![Span::raw("  ")], Style::new()));
    }
    let labeled = |label: &str, value: &str| {
        let mut lead = vec![
            Span::raw("  "),
            Span::styled(format!("{label:<9}"), theme.quiet()),
        ];
        lead.truncate(2);
        sentence(value, width, lead, Style::new())
    };
    if let Some(command) = &worded.command {
        lines.extend(labeled("command", command));
    }
    if let Some(hint) = &worded.hint {
        lines.extend(labeled("run", hint));
    }
    lines
}

/// `start acme@0:x` becomes `Start acme@0:x`.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A unit: its terminal, with a failure card above it when it failed.
fn render_unit(
    app: &App,
    lane: &LaneSummary,
    unit: &UnitSummary,
    frame: &mut Frame,
    boxed: Rect,
    area: Rect,
) {
    let theme = &app.theme;
    let width = usize::from(area.width).max(1);
    let attached = app
        .attached
        .as_ref()
        .filter(|a| a.lane == lane.reference && a.unit == unit.name && !a.container);
    // A unit that does not run has nothing to watch or type into.
    let right = attached
        .filter(|a| is_active_state(&unit.state) || !matches!(a.mode, PaneMode::Live))
        .and_then(|a| mode_text(app, a, usize::from(boxed.width)))
        .unwrap_or_default();
    render_box(
        app,
        frame,
        boxed,
        title(tree::short_name(unit), &unit.name, app),
        unit_facts_title(app, unit),
        right,
    );
    if is_orphaned(unit) {
        details(frame, orphan_lines(theme, unit, width), area, app);
        return;
    }
    let card = failure_card(app, lane, unit, width);
    let Some(attached) = attached else {
        let mut lines = card;
        lines.extend(stopped_lines(app, unit, None, width));
        details(frame, lines, area, app);
        return;
    };
    match (&attached.state, &attached.mode) {
        (AttachState::Failed(reason), _) => {
            let mut lines = card;
            lines.extend(stopped_lines(app, unit, Some(reason), width));
            details(frame, lines, area, app);
        }
        // Until the first page arrives the live screen stays, so the pane
        // does not blank out between the two.
        (_, PaneMode::Logs(view)) if !(view.loading && view.lines.is_empty()) => {
            render_logs(theme, view, frame, area);
        }
        (_, mode) => {
            let rows = u16::try_from(card.len()).unwrap_or(u16::MAX);
            let screen_area = if card.is_empty() || rows + 3 > area.height {
                area
            } else {
                let used = rows;
                frame.render_widget(
                    Paragraph::new(card),
                    Rect::new(area.x, area.y, area.width, used),
                );
                // `├─ last output ───┤` across the box, joining its borders.
                divider(app, frame, boxed, area.y + used, "last output");
                Rect::new(
                    area.x,
                    area.y + used + 1,
                    area.width,
                    area.height - used - 1,
                )
            };
            let tail = screen_area != area;
            render_terminal(app, attached, frame, screen_area, tail);
            if let PaneMode::Copy(state) = mode {
                highlight_copy(theme, *state, frame, screen_area);
            }
        }
    }
}

/// The pane's mode at the right of its title: `watching · enter types into
/// it`, `typing · esc returns`, or what the pane shows instead of live output.
fn mode_text(app: &App, attached: &Attached, width: usize) -> Option<Vec<Span<'static>>> {
    let theme = &app.theme;
    let quiet = |text: &str| Some(vec![Span::styled(text.to_owned(), theme.quiet())]);
    match (&attached.mode, &attached.state) {
        (PaneMode::Copy(_), _) => quiet("selecting · v mark · y copy · esc cancel"),
        (PaneMode::Logs(view), _) if view.loading => quiet("stored logs · loading…"),
        (PaneMode::Logs(_), _) => quiet("stored logs · esc returns to live"),
        (_, AttachState::Closed) => quiet("output ended"),
        (_, AttachState::Failed(_)) => None,
        (PaneMode::Live, _) if attached.screen.scroll_offset() > 0 => {
            quiet(&format!("{} lines up", attached.screen.scroll_offset()))
        }
        _ if app.focus == Focus::Terminal => {
            let back = if app.full_screen() {
                "typing · ctrl+] returns"
            } else {
                "typing · esc returns"
            };
            Some(vec![Span::styled(back, theme.focus())])
        }
        // The long form where the mockup at 200 columns has room for it.
        _ if width >= 120 => quiet("watching · enter types into it"),
        _ => quiet("watching · enter types"),
    }
}

/// What a container's pane shows at the right of its title: Docker's logs,
/// how far they are scrolled, or the copy keys.
fn container_mode_text(app: &App, attached: &Attached) -> Option<Vec<Span<'static>>> {
    let quiet = |text: &str| Some(vec![Span::styled(text.to_owned(), app.theme.quiet())]);
    match (&attached.mode, &attached.state) {
        (PaneMode::Copy(_), _) => quiet("selecting · v mark · y copy · esc cancel"),
        (_, AttachState::Opening) => quiet("docker logs · loading…"),
        (_, AttachState::Closed) => quiet("docker logs · ended"),
        (_, AttachState::Failed(_)) => None,
        _ if attached.screen.scroll_offset() > 0 => quiet(&format!(
            "docker logs · {} lines up",
            attached.screen.scroll_offset()
        )),
        _ => quiet("docker logs"),
    }
}

/// `ready · https://api.acme.localhost`, `failed · exit 1`, `❄ frozen · :41000`
/// for the title: the state, then the first URL, else the first port.
fn unit_facts_title(app: &App, unit: &UnitSummary) -> Vec<Span<'static>> {
    let theme = &app.theme;
    let look = unit_look(theme, unit, app.spin);
    let idle = matches!(look.word.as_str(), "frozen" | "throttled");
    let mut facts = vec![Span::styled(
        look.word.clone(),
        theme.tone(if idle { look.word_tone } else { look.tone }),
    )];
    if !look.detail.is_empty() {
        let tone = if look.tone == Tone::Failure {
            Tone::Failure
        } else {
            Tone::Quiet
        };
        facts.push(Span::styled(look.detail.clone(), theme.tone(tone)));
    }
    let address = unit.urls.first().cloned().or_else(|| {
        unit.ports
            .get("http")
            .or_else(|| unit.ports.values().next())
            .map(|port| format!(":{port}"))
    });
    if let Some(address) = address.filter(|_| is_active_state(&unit.state)) {
        facts.push(Span::styled(clean(&address), theme.quiet()));
    }
    let mut facts = dotted(app, facts);
    // `❄ frozen`: the glyph names the idle level, the word stays dim like a
    // healthy state's, since a request thaws the unit.
    if idle {
        facts.insert(
            0,
            Span::styled(format!("{} ", look.glyph), theme.tone(look.tone)),
        );
    }
    facts
}

/// Whether a unit's process runs, so its address answers.
fn is_active_state(state: &str) -> bool {
    matches!(state, "ready" | "running" | "starting")
}

/// `├─ label ──────┤` on row `y` of a box, joining its side borders.
fn divider(app: &App, frame: &mut Frame, boxed: Rect, y: u16, label: &str) {
    if boxed.width < 4 || y >= boxed.bottom().saturating_sub(1) {
        return;
    }
    let ascii = app.theme.glyphs == crate::theme::Glyphs::Ascii;
    let (left, line, right) = if ascii {
        ("+", "-", "+")
    } else {
        ("├", "─", "┤")
    };
    let inner = usize::from(boxed.width) - 2;
    let head = format!("{line} {label} ");
    let rest = inner.saturating_sub(text::width(&head));
    let text = format!("{left}{}{}{right}", clip(&head, inner), line.repeat(rest));
    frame.render_widget(
        Paragraph::new(Line::styled(text, app.theme.rule())),
        Rect::new(boxed.x, y, boxed.width, 1),
    );
}

/// `3s`, `2m`, `1h 4m` for how long something took.
fn span_text(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds / 60 % 60),
    }
}

/// What happened to a failed unit and what to do, as `failed` and `next`
/// fields above its last output; a failed action shows its error instead.
fn failure_card(
    app: &App,
    lane: &LaneSummary,
    unit: &UnitSummary,
    width: usize,
) -> Vec<Line<'static>> {
    let theme = &app.theme;
    if let Some(entry) = target_error(app, &lane.reference, Some(&unit.name)) {
        return error_banner(theme, &entry.what, &entry.worded, width);
    }
    let task = tree::role(unit) != Role::Service && tree::role(unit) != Role::Exec;
    let failed = unit.state == "failed"
        || (unit.state == "exited" && unit.exit_code.is_some_and(|code| code != 0));
    if !failed {
        return Vec::new();
    }
    let red = theme.tone(Tone::Failure);
    let plain = Style::new();
    let mut what = match unit.exit_code {
        Some(code) => devlanes_cli::exit::words(code.into())
            .map_or_else(|| format!("exited with code {code}"), str::to_owned),
        None => "failed".to_owned(),
    };
    if let Some(finished) = unit.finished_at_ms {
        what.push_str(&format!(" {}", super::ago(finished)));
        if let Some(started) = unit.started_at_ms.filter(|s| *s <= finished) {
            what.push_str(&format!(", {} after start", span_text(finished - started)));
        }
    }
    let mut lines = field(theme, "failed", &what, width, red);
    if let Some(reason) = &unit.reason {
        let worded = words::rpc(reason);
        if worded.code.as_deref() != Some("unit_exited") {
            lines.extend(field(theme, "reason", &worded.summary, width, plain));
            if let Some(hint) = &worded.hint {
                lines.extend(field(theme, "run", hint, width, plain));
            }
        }
    }
    let again = if task {
        "r runs it again"
    } else {
        "r restarts it"
    };
    lines.extend(field(theme, "next", again, width, plain));
    lines.extend(field(
        theme,
        "",
        &format!(
            "devlanes logs {} prints the whole log",
            unit_ref(&lane.reference, &unit.name)
        ),
        width,
        theme.quiet(),
    ));
    lines
}

/// Why a unit has no terminal, then its facts.
fn stopped_lines(
    app: &App,
    unit: &UnitSummary,
    attach: Option<&Worded>,
    width: usize,
) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut lines = Vec::new();
    let task = tree::role(unit) != Role::Service && tree::role(unit) != Role::Exec;
    let quiet = matches!(unit.state.as_str(), "idle" | "stopped" | "exited");
    if quiet && !tree::is_problem(unit) {
        let text = match (task, unit.autostart) {
            (true, _) => "Not running. s runs it.",
            (false, true) => "Not running. s starts it.",
            (false, false) => "Not running. Lane starts skip it (autostart is off). s starts it.",
        };
        lines.extend(sentence(text, width, Vec::new(), Style::new()));
        lines.push(Line::from(""));
    } else if let Some(attach) = attach
        && attach.code.as_deref() != Some("not_running")
    {
        lines.extend(sentence(
            &format!("No terminal: {}.", attach.summary.trim_end_matches('.')),
            width,
            Vec::new(),
            theme.quiet(),
        ));
        lines.push(Line::from(""));
    }
    lines.extend(unit_facts(app, unit, width));
    lines
}

/// `url`, `ports`, `tags`, `started by`.
fn unit_facts(app: &App, unit: &UnitSummary, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let plain = Style::new();
    let mut lines = Vec::new();
    if let Some(url) = unit.urls.first() {
        lines.extend(field(theme, "url", url, width, plain));
    }
    if !unit.ports.is_empty() {
        let ports: Vec<String> = unit
            .ports
            .iter()
            .map(|(name, port)| format!("{name} {port}"))
            .collect();
        lines.extend(field(theme, "ports", &ports.join(", "), width, plain));
    }
    if !unit.tags.is_empty() {
        lines.extend(field(theme, "tags", &unit.tags.join(", "), width, plain));
    }
    if started_by_agent(unit) {
        let who = unit
            .actor
            .agent_id
            .as_deref()
            .map_or_else(|| "an agent".to_owned(), |id| format!("agent {id}"));
        lines.extend(field(theme, "started by", &who, width, plain));
    }
    if unit.hidden {
        lines.extend(field(
            theme,
            "hidden",
            "yes, in the config",
            width,
            theme.quiet(),
        ));
    }
    lines
}

/// A process an earlier daemon left running.
fn orphan_lines(theme: &Theme, unit: &UnitSummary, width: usize) -> Vec<Line<'static>> {
    let mut lines = banner(
        theme,
        Glyph::Orphaned,
        Tone::Attention,
        "An earlier daemon started this process and left it running.",
        &[
            "It has no terminal or live output here.".into(),
            "x stops it. r stops it and starts a copy you can watch.".into(),
        ],
        width,
    );
    lines.push(Line::from(""));
    let plain = Style::new();
    if let Some(pid) = unit.pid {
        lines.extend(field(theme, "pid", &pid.to_string(), width, plain));
    }
    let since = |ms: u64| clock(ms);
    let running = match (unit.started_at_ms, unit.orphaned_since_ms) {
        (Some(start), Some(orphaned)) => Some(format!(
            "since {}, orphaned since {}",
            since(start),
            since(orphaned)
        )),
        (None, Some(orphaned)) => Some(format!("orphaned since {}", since(orphaned))),
        (Some(start), None) => Some(format!("since {}", since(start))),
        (None, None) => None,
    };
    if let Some(running) = running {
        lines.extend(field(theme, "running", &running, width, plain));
    }
    if !unit.ports.is_empty() {
        let ports: Vec<String> = unit
            .ports
            .iter()
            .map(|(name, port)| format!("{name} {port}"))
            .collect();
        lines.extend(field(theme, "ports", &ports.join(", "), width, plain));
    }
    lines
}

/// `HH:MM` in UTC for a Unix millisecond time; the TUI has no time zone data.
fn clock(ms: u64) -> String {
    let minutes = (ms / 60_000) % (24 * 60);
    format!("{:02}:{:02} UTC", minutes / 60, minutes % 60)
}

/// `frozen since 10:42 UTC, last activity a request at 10:27 UTC`: when an
/// idle lane reached its level and what last kept it awake.
fn idle_fact(power: &devlanes_proto::LanePower) -> String {
    let mut text = format!("{} since {}", clean(&power.level), clock(power.since_ms));
    let what = power
        .last_activity
        .as_deref()
        .map(|activity| match activity {
            "proxy" => "a request".to_owned(),
            "connection" => "a connection".to_owned(),
            "cpu" => "CPU use".to_owned(),
            "command" => "a command".to_owned(),
            "start" => "a start".to_owned(),
            "stop" => "a stop".to_owned(),
            other => clean(other),
        });
    match (what, power.last_activity_ms) {
        (Some(what), Some(ms)) => {
            text.push_str(&format!(", last activity {what} at {}", clock(ms)));
        }
        (Some(what), None) => text.push_str(&format!(", last activity {what}")),
        (None, Some(ms)) => text.push_str(&format!(", last activity at {}", clock(ms))),
        (None, None) => {}
    }
    text
}

/// A task that has not run: what `task.list` says about it.
fn task_lines(app: &App, lane: &LaneSummary, name: &str, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let plain = Style::new();
    let info = app.task_info(&lane.reference, name);
    let text = match info.and_then(|i| i.last_run.as_ref()) {
        None => "Never ran in this lane. s runs it.",
        Some(run) if run.state == "failed" => "Its last run failed. s runs it again.",
        Some(_) => "s runs it again.",
    };
    let mut lines = Vec::new();
    if let Some(entry) = target_error(app, &lane.reference, Some(name)) {
        lines.extend(error_banner(theme, &entry.what, &entry.worded, width));
        lines.push(Line::from(""));
    }
    lines.extend(sentence(text, width, Vec::new(), plain));
    lines.push(Line::from(""));
    if let Some(run) = info.and_then(|i| i.last_run.as_ref()) {
        let mut last = run.state.clone();
        if let Some(code) = run.exit_code.filter(|code| *code != 0) {
            last.push_str(", ");
            last.push_str(&super::exit_words(code));
        }
        if let Some(finished) = run.finished_at_ms {
            last.push_str(&format!(", {}", super::ago(finished)));
        }
        lines.extend(field(theme, "last run", &last, width, plain));
    }
    if let Some(info) = info {
        if let Some(when) = &info.when {
            let word = match when.as_str() {
                "lane.start" => "at every lane start",
                "lane.create" => "at the lane's first start",
                "lane.stop" => "when the lane stops",
                "lane.clean" => "when the lane is cleaned",
                other => other,
            };
            lines.extend(field(theme, "runs", word, width, plain));
        }
        if let Some(steps) = info.steps.as_ref().filter(|s| !s.is_empty()) {
            lines.extend(field(theme, "steps", &steps.join(", "), width, plain));
        }
        if info.interactive {
            lines.extend(field(
                theme,
                "terminal",
                "interactive: s opens it for typing",
                width,
                plain,
            ));
        }
        if info.confirm {
            lines.extend(field(theme, "confirm", "asks before it runs", width, plain));
        }
    }
    lines
}

/// A container: state, ports, shared, cpu and memory.
fn container_lines(
    app: &App,
    container: &devlanes_proto::ContainerSummary,
    width: usize,
) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let plain = Style::new();
    let mut lines = Vec::new();
    if let Some(state) = &container.state {
        lines.extend(field(theme, "state", state, width, plain));
    }
    if !container.ports.is_empty() {
        let ports: Vec<String> = container
            .ports
            .iter()
            .map(|(name, port)| format!("{name} {port}"))
            .collect();
        lines.extend(field(theme, "ports", &ports.join(", "), width, plain));
    }
    if !container.urls.is_empty() {
        lines.extend(field(
            theme,
            "url",
            &container.urls.join(", "),
            width,
            plain,
        ));
    }
    lines.extend(field(
        theme,
        "scope",
        if container.shared {
            "shared by every lane of the project"
        } else {
            "this lane's own"
        },
        width,
        plain,
    ));
    if let Some(cpu) = container.cpu_percent {
        lines.extend(field(theme, "cpu", &format!("{cpu:.0}%"), width, plain));
    }
    if let Some(memory) = container.memory_bytes {
        lines.extend(field(
            theme,
            "memory",
            &super::format_bytes(memory),
            width,
            plain,
        ));
    }
    lines.push(Line::from(""));
    lines.extend(sentence(
        "Containers start and stop with the lane.",
        width,
        Vec::new(),
        theme.quiet(),
    ));
    lines
}

/// A group or section: one line per unit with state, url, cpu and memory.
fn members_table(
    app: &App,
    lane: &LaneSummary,
    members: &[RowId],
    width: usize,
) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let name_width = members
        .iter()
        .filter_map(|m| match m {
            RowId::Unit(name) => lane
                .units
                .iter()
                .find(|u| u.name == *name)
                .map(|u| text::width(tree::short_name(u))),
            RowId::Task(name) | RowId::Container(name) => Some(text::width(name)),
            _ => None,
        })
        .max()
        .unwrap_or(4)
        .clamp(4, 24);
    let mut lines = vec![fit(
        vec![Span::styled(
            format!(
                "  {:<name_width$}  {:<9} {:>5} {:>6}  url",
                "name", "state", "cpu", "memory"
            ),
            theme.quiet(),
        )],
        width,
    )];
    for member in members {
        let (glyph, tone, name, word, cpu, memory, url, hidden) = match member {
            RowId::Unit(name) => {
                let Some(unit) = lane.units.iter().find(|u| u.name == *name) else {
                    continue;
                };
                let look = unit_look(theme, unit, app.spin);
                (
                    look.glyph,
                    look.tone,
                    clean(tree::short_name(unit)),
                    look.word,
                    unit.cpu_percent,
                    unit.memory_bytes,
                    unit.urls.first().cloned().unwrap_or_default(),
                    unit.hidden,
                )
            }
            RowId::Task(name) => (
                theme.glyph(Glyph::Stopped),
                Tone::Quiet,
                clean(name),
                "not run".into(),
                None,
                None,
                String::new(),
                false,
            ),
            RowId::Container(name) => {
                let Some(container) = app.find_container(lane, name) else {
                    continue;
                };
                let (glyph, tone, word) = container_look(app, Some(lane), &container);
                (
                    glyph,
                    tone,
                    clean(name),
                    word,
                    container.cpu_percent,
                    container.memory_bytes,
                    String::new(),
                    container.hidden,
                )
            }
            _ => continue,
        };
        let cpu = cpu.map_or_else(|| "-".into(), |cpu| format!("{cpu:.0}%"));
        let memory = memory.map_or_else(|| "-".into(), super::format_bytes);
        // Hidden units and containers are dimmed here as in the list.
        let name_style = if hidden { theme.quiet() } else { Style::new() };
        lines.push(fit(
            vec![
                Span::styled(format!("{glyph} "), theme.tone(tone)),
                Span::styled(
                    format!("{:<name_width$}  ", clip(&name, name_width)),
                    name_style,
                ),
                Span::styled(
                    format!("{word:<9}"),
                    theme.tone(if matches!(tone, Tone::Ok | Tone::Frozen) {
                        Tone::Quiet
                    } else {
                        tone
                    }),
                ),
                Span::raw(format!(" {cpu:>5} {memory:>6}  ")),
                Span::styled(clean(&url), theme.quiet()),
            ],
            width,
        ));
    }
    lines
}

/// The whole lane: banners, the start's phases, facts and the project's lanes.
fn lane_lines(app: &App, lane: &LaneSummary, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let plain = Style::new();
    let mut lines = Vec::new();
    let path = clean(&lane.path.display().to_string());
    let missing = lane
        .reason
        .as_ref()
        .is_some_and(|r| r.code == "worktree_missing");
    if missing {
        lines.extend(banner(
            theme,
            Glyph::Attention,
            Tone::Attention,
            &format!("The worktree {path} is gone."),
            &[
                "The lane still holds its ports. C cleans this lane and frees them.".into(),
                "X on the daemon page cleans every lane like this one.".into(),
            ],
            width,
        ));
        lines.push(Line::from(""));
    }
    let entry: Option<&ErrorEntry> = target_error(app, &lane.reference, None);
    if let Some(entry) = entry {
        lines.extend(error_banner(theme, &entry.what, &entry.worded, width));
        lines.push(Line::from(""));
    }
    if let Some(reason) = lane.reason.as_ref().filter(|r| r.code == "config_error") {
        // As the mockup: what failed, then `error` and `next` fields.
        let data = &reason.data;
        let text = |key: &str| data.get(key).and_then(serde_json::Value::as_str).map(clean);
        let number = |key: &str| {
            data.get(key)
                .and_then(serde_json::Value::as_u64)
                .filter(|n| *n > 0)
        };
        lines.extend(sentence(
            &format!("devlanes.yaml in {path} doesn't load"),
            width,
            Vec::new(),
            plain,
        ));
        lines.push(Line::from(""));
        let message = text("message").unwrap_or_else(|| words::rpc(reason).summary);
        lines.extend(field(
            theme,
            "error",
            &message,
            width,
            theme.tone(Tone::Attention),
        ));
        let at = match (number("line"), number("column")) {
            (Some(line), Some(column)) => Some(format!("line {line}, column {column}")),
            (Some(line), None) => Some(format!("line {line}")),
            _ => None,
        };
        if let Some(at) = at {
            lines.extend(field(theme, "", &at, width, theme.quiet()));
        }
        lines.extend(field(
            theme,
            "next",
            "fix the file, then s starts the lane",
            width,
            plain,
        ));
        lines.extend(field(
            theme,
            "",
            "devlanes config check in that folder lists every problem",
            width,
            theme.quiet(),
        ));
        lines.push(Line::from(""));
    } else if let Some(reason) = lane.reason.as_ref().filter(|_| !missing) {
        let worded = words::rpc(reason);
        // The daemon keeps a failed start as the lane's reason; show it once.
        if entry.is_none_or(|entry| entry.worded.code != worded.code) {
            let headline = capitalized(&worded.summary);
            let mut rest: Vec<String> = worded.detail.iter().take(STDERR_TAIL).cloned().collect();
            if let Some(hint) = &worded.hint {
                rest.push(format!("run {hint}"));
            }
            lines.extend(banner(
                theme,
                Glyph::Attention,
                Tone::Attention,
                &headline,
                &rest,
                width,
            ));
            lines.push(Line::from(""));
        }
    }
    lines.extend(phase_lines(app, lane, width));
    if lane.state == "idle"
        && lane.units.iter().all(|u| !crate::app::is_active(&u.state))
        && !missing
    {
        lines.extend(sentence(
            "Not running. s starts the lane with its default profile, p picks profiles.",
            width,
            Vec::new(),
            plain,
        ));
        lines.push(Line::from(""));
    }
    lines.extend(field(theme, "branch", &lane_name(lane), width, plain));
    let worktree = if missing {
        format!("{path} (missing)")
    } else {
        path
    };
    lines.extend(field(theme, "worktree", &worktree, width, plain));
    let profiles = lane_profiles(lane);
    if !profiles.is_empty() {
        lines.extend(field(theme, "profile", &profiles.join(", "), width, plain));
    }
    if let Some(domain) = &lane.domain {
        lines.extend(field(theme, "domain", domain, width, plain));
    }
    if let Some((first, last)) = lane.port_block {
        lines.extend(field(
            theme,
            "ports",
            &format!("{first}-{last}"),
            width,
            plain,
        ));
    }
    if let Some(power) = &lane.power {
        lines.extend(field(theme, "idle", &idle_fact(power), width, plain));
    }
    let counts = tree::counts(lane);
    let services: Vec<&UnitSummary> = lane
        .units
        .iter()
        .filter(|u| tree::role(u) == Role::Service)
        .collect();
    if !services.is_empty() {
        let up = services
            .iter()
            .filter(|u| matches!(u.state.as_str(), "ready" | "running"))
            .count();
        lines.extend(field(
            theme,
            "services",
            &format!("{up} of {} up", services.len()),
            width,
            plain,
        ));
        // Every service that is not up, by name: the lane's to-do list.
        let mut down: Vec<&&UnitSummary> = services
            .iter()
            .filter(|u| !matches!(u.state.as_str(), "ready" | "running"))
            .collect();
        down.sort_by_key(|u| !tree::is_problem(u));
        let room = 8;
        for unit in down.iter().take(room) {
            let look = unit_look(theme, unit, app.spin);
            let mut text = look.word.clone();
            if !look.detail.is_empty() && !look.detail.starts_with(':') {
                text.push_str(&format!(", {}", look.detail));
            }
            lines.push(fit(
                vec![
                    Span::raw(" ".repeat(LABEL)),
                    Span::styled(format!("{} ", look.glyph), theme.tone(look.tone)),
                    Span::raw(format!("{:<16} ", clip(&clean(unit.label()), 16))),
                    Span::styled(text, theme.tone(look.word_tone)),
                ],
                width,
            ));
        }
        if down.len() > room {
            lines.push(Line::styled(
                format!("{}… {} more", " ".repeat(LABEL), down.len() - room),
                theme.quiet(),
            ));
        }
    }
    let tasks: Vec<&UnitSummary> = lane
        .units
        .iter()
        .filter(|u| tree::role(u) == Role::Task)
        .collect();
    let configured = app.task_list(&lane.reference).map_or(0, |t| {
        t.iter()
            .filter(|task| !tasks.iter().any(|u| u.name == task.name))
            .filter(|task| !task.name.starts_with("setup.") && !task.name.starts_with("teardown."))
            .count()
    });
    if !tasks.is_empty() || configured > 0 {
        let done = tasks
            .iter()
            .filter(|u| u.state == "exited" && u.exit_code == Some(0))
            .count();
        let failed = tasks.iter().filter(|u| tree::is_problem(u)).count();
        let running = tasks
            .iter()
            .filter(|u| crate::app::is_active(&u.state))
            .count();
        let not_run = configured + tasks.iter().filter(|u| u.state == "idle").count();
        let mut parts = Vec::new();
        for (count, word) in [
            (running, "running"),
            (done, "done"),
            (failed, "failed"),
            (not_run, "not run"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {word}"));
            }
        }
        lines.extend(field(theme, "tasks", &parts.join(", "), width, plain));
    }
    let setup: Vec<&UnitSummary> = lane
        .units
        .iter()
        .filter(|u| tree::role(u) == Role::Setup)
        .collect();
    if !setup.is_empty() {
        let done = setup
            .iter()
            .filter(|u| u.state == "exited" && u.exit_code == Some(0))
            .count();
        lines.extend(field(
            theme,
            "setup",
            &format!("{done} of {} done", setup.len()),
            width,
            plain,
        ));
    }
    let infra: Vec<String> = lane
        .containers
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| {
            let port = c
                .ports
                .values()
                .next()
                .map(|p| format!(" :{p}"))
                .unwrap_or_default();
            let shared = if c.shared { " (shared)" } else { "" };
            format!("{}{port}{shared}", clean(&c.name))
        })
        .collect();
    if !infra.is_empty() {
        lines.extend(field(theme, "infra", &infra.join(", "), width, plain));
    }
    let _ = counts;
    if lane.config_changed {
        lines.extend(field(
            theme,
            "config",
            "changed since the lane started; r restarts with it",
            width,
            plain,
        ));
    }
    if let Some(next) = lane.moves_to {
        lines.extend(field(
            theme,
            "moves",
            &format!("becomes @{next} at its next start"),
            width,
            plain,
        ));
    }
    if lane.needs_env {
        lines.extend(field(
            theme,
            "env",
            "lost in a daemon restart; s starts the lane with this TUI's environment",
            width,
            plain,
        ));
    }
    for warning in &lane.warnings {
        let message = warning
            .data
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(warning.code.as_str());
        lines.extend(sentence(
            message,
            width,
            vec![Span::styled(
                format!("{} ", theme.glyph(Glyph::Attention)),
                theme.tone(Tone::Attention),
            )],
            theme.tone(Tone::Attention),
        ));
    }
    lines.extend(lanes_table(app, lane, width));
    lines
}

/// The start's phases while it runs or after it failed:
/// `  ✓ infra      postgres, redis up      3s`.
fn phase_lines(app: &App, lane: &LaneSummary, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let Some(phases) = app.phases.get(&lane.reference) else {
        return Vec::new();
    };
    let starting = lane.state == "starting";
    let failed = phases.steps.iter().any(|s| s.state == "failed");
    if phases.steps.is_empty() || !(starting || failed) {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for step in &phases.steps {
        let (glyph, tone) = match step.state.as_str() {
            "done" => (theme.glyph(Glyph::Done), Tone::Ok),
            "failed" => (theme.glyph(Glyph::Failed), Tone::Failure),
            "skipped" => (theme.glyph(Glyph::Stopped), Tone::Quiet),
            _ => (theme.spinner(app.spin), Tone::Busy),
        };
        let detail = match (step.state.as_str(), &step.detail) {
            ("skipped", _) => "skipped".to_owned(),
            (_, Some(detail)) => clean(detail),
            ("done", None) => "done".to_owned(),
            ("failed", None) => "failed".to_owned(),
            _ => "running".to_owned(),
        };
        let seconds = if step.seconds > 0 {
            format!("{}s", step.seconds)
        } else {
            String::new()
        };
        lines.push(spread(
            vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), theme.tone(tone)),
                Span::raw(format!("{:<10} ", clean(&step.phase))),
                Span::styled(detail, theme.quiet()),
            ],
            vec![Span::styled(seconds, theme.quiet())],
            width,
        ));
    }
    if starting {
        let names = |state: &str| -> Vec<String> {
            lane.units
                .iter()
                .filter(|u| u.state == state && tree::role(u) == Role::Service)
                .map(|u| clean(u.label()))
                .collect()
        };
        let plain = Style::new();
        lines.push(Line::from(""));
        let starting = names("starting");
        if !starting.is_empty() {
            lines.extend(field(theme, "starting", &starting.join(", "), width, plain));
        }
        let queued = names("queued");
        if !queued.is_empty() {
            let why = lane
                .units
                .iter()
                .filter_map(|u| u.reason.as_ref())
                .find(|r| r.code == "queued")
                .and_then(|r| r.data.get("window").and_then(serde_json::Value::as_u64))
                .map(|window| format!(": {window} services start at a time"))
                .unwrap_or_default();
            lines.extend(field(
                theme,
                "queued",
                &format!("{}{why}", queued.join(", ")),
                width,
                plain,
            ));
        }
        lines.extend(sentence(
            "x stops the start. Select a unit to watch its output.",
            width,
            Vec::new(),
            theme.quiet(),
        ));
    }
    lines.push(Line::from(""));
    lines
}

/// With several lanes in the project: one row per lane.
fn lanes_table(app: &App, current: &LaneSummary, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let lanes = app.project_lanes();
    if lanes.len() < 2 {
        return Vec::new();
    }
    let mut lines = vec![Line::from("")];
    lines.push(fit(
        vec![Span::styled(
            format!("lanes in {}", clean(&current.project)),
            Style::new().add_modifier(Modifier::BOLD),
        )],
        width,
    ));
    for lane in lanes {
        let marker = if lane.reference == current.reference {
            theme.glyph(Glyph::Marker)
        } else {
            " "
        };
        let name = match lane.index {
            Some(index) => format!("{} @{index}", lane_name(lane)),
            None => format!("{} (no lane)", lane_name(lane)),
        };
        let mut spans = vec![
            Span::styled(format!("{marker} "), theme.focus()),
            Span::raw(format!("{:<24} ", clip(&name, 24))),
        ];
        let mut summary = lane_summary(app, lane);
        if lane.index.is_none() {
            summary = vec![Span::styled(
                "devlanes start in its folder gives it one",
                theme.quiet(),
            )];
        }
        spans.extend(summary);
        lines.push(fit(spans, width));
    }
    lines.push(Line::from(""));
    lines.extend(sentence(
        "1-9 (the lane's place in the bar), [ and ] or a click on a chip switch lanes. devlanes lane new <branch> adds one.",
        width,
        Vec::new(),
        theme.quiet(),
    ));
    lines
}

/// Draws the unit's screen. With `tail`, only the last rows with text fit,
/// so the end of a dead unit's output shows under its card.
fn render_terminal(app: &App, attached: &Attached, frame: &mut Frame, area: Rect, tail: bool) {
    let shown = attached.shown();
    let view = ScreenView::new(shown);
    let mut cursor = Cursor::default();
    if app.focus != Focus::Terminal {
        cursor.hide();
    }
    let inner = area;
    if !tail {
        frame.render_widget(PseudoTerminal::new(&view).cursor(cursor), inner);
        return;
    }
    let rows = shown.size().0.get();
    let full = Rect::new(0, 0, inner.width, rows);
    let mut scratch = Buffer::empty(full);
    PseudoTerminal::new(&view)
        .cursor(cursor)
        .render(full, &mut scratch);
    let used = (0..full.height)
        .rev()
        .find(|&y| (0..full.width).any(|x| !scratch[(x, y)].symbol().trim().is_empty()))
        .map_or(0, |y| y + 1);
    let first = used.saturating_sub(inner.height);
    let buffer = frame.buffer_mut();
    for (dy, y) in (first..used).enumerate() {
        for x in 0..inner.width {
            let target = (inner.x + x, inner.y + dy as u16);
            if target.1 < inner.bottom() {
                buffer[target] = scratch[(x, y)].clone();
            }
        }
    }
}

/// Stored log lines, newest at the bottom, scrolled by `view.offset`.
fn render_logs(theme: &Theme, view: &LogView, frame: &mut Frame, area: Rect) {
    let height = usize::from(area.height);
    if view.lines.is_empty() {
        frame.render_widget(
            Paragraph::new("No stored logs for this unit yet.").style(theme.quiet()),
            area,
        );
        return;
    }
    let end = view.lines.len().saturating_sub(view.offset);
    let start = end.saturating_sub(height);
    // The parser keeps escape sequences and control bytes inside its grid,
    // so the stored colors reach the pane without `clean`.
    let parser = crate::screen::stored_lines(
        view.lines[start..end]
            .iter()
            .map(|line| line.styled.as_deref().unwrap_or(&line.text)),
        area.height,
        area.width,
        start == 0,
    );
    let screen = ScreenView::new(parser.screen());
    let mut cursor = Cursor::default();
    cursor.hide();
    frame.render_widget(PseudoTerminal::new(&screen).cursor(cursor), area);
}

/// Reverses the selected cells and marks the copy cursor.
fn highlight_copy(theme: &Theme, state: CopyState, frame: &mut Frame, area: Rect) {
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
        let cell = &mut buffer[(x, y)];
        cell.set_style(
            theme
                .focus()
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        );
    }
}
