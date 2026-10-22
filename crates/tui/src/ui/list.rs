//! The list box: one lane's whole-lane row, its sections and their units,
//! as the navigation.md mockups draw them. Normal states (ready, running,
//! stopped) are a glyph only; words are for what needs a look or is moving.

use devlanes_proto::{LaneSummary, UnitSummary};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{draw_box, fit, spans_width, spread, target_error, task_look, unit_look};
use crate::app::{App, Focus, View};
use crate::text::{self, clean};
use crate::theme::{Glyph, Tone};
use crate::tree::{self, ContainerTone, Row, RowId, Section};

/// Unit states the glyph says on its own.
const SILENT: [&str; 5] = ["ready", "running", "stopped", "idle", "not run"];

/// The list box and its rows.
pub fn render(app: &App, frame: &mut Frame, boxed: Rect, area: Rect) {
    let theme = &app.theme;
    let width = usize::from(area.width);
    let focused = app.focus == Focus::List && app.view == View::Project;
    let rows = app.rows();
    let Some(lane) = app.selected_lane() else {
        draw_box(
            app,
            frame,
            boxed,
            vec![Span::styled(" loading… ", theme.quiet())],
            Vec::new(),
            false,
        );
        return;
    };
    // Title: `main @0` as the mockups draw it (the lane's state glyph is on
    // its chip), and the filter while one is set. The lane has no row:
    // selecting it (k from the top, a click on the title) highlights the
    // title and shows its status in the pane.
    let mut name = vec![
        Span::raw(" "),
        Span::styled(lane_name(lane), Style::new().add_modifier(Modifier::BOLD)),
    ];
    if let Some(index) = lane.index {
        name.push(Span::raw(format!(" @{index}")));
    }
    // A lane on its way somewhere says so; settled states need no word.
    let word = if app.is_cleaning(&lane.reference) {
        Some("cleaning…")
    } else {
        match lane.state.as_str() {
            "starting" => Some("starting"),
            "stopping" => Some("stopping"),
            _ => None,
        }
    };
    if let Some(word) = word {
        name.push(Span::styled(format!(" · {word}"), theme.tone(Tone::Busy)));
    }
    name.push(Span::raw(" "));
    if app.selected_id() == Some(&RowId::WholeLane) {
        name = name
            .into_iter()
            .map(|s| {
                let style = if focused {
                    theme.selected(s.style)
                } else {
                    s.style
                };
                Span::styled(s.content, style.add_modifier(Modifier::BOLD))
            })
            .collect();
    }
    let title_width = super::spans_width(&name);
    let mut title = name;
    if let Some(filter) = &app.filter {
        title.push(Span::styled("· filter: ", theme.quiet()));
        title.push(Span::raw(clean(&filter.text)));
        if filter.editing {
            title.push(Span::styled("▏", theme.focus()));
        }
        title.push(Span::raw(" "));
    }
    draw_box(app, frame, boxed, title, Vec::new(), false);
    super::hit(
        app,
        Rect::new(boxed.x + 1, boxed.y, title_width as u16, 1),
        crate::app::Click::StartLane(lane.reference.clone()),
    );

    let height = usize::from(area.height);
    let selected = app.selected_row();
    let mut lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(app.list_offset)
        .take(height)
        .map(|(index, row)| {
            let line = row_line(app, lane, row, width);
            let chosen = Some(index) == selected;
            if chosen && focused {
                let mut spans: Vec<Span> = line
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, theme.selected(s.style)))
                    .collect();
                let pad = width.saturating_sub(spans_width(&spans));
                spans.push(Span::styled(" ".repeat(pad), theme.selection()));
                Line::from(spans)
            } else if chosen {
                // Typing: the row keeps a marker in column 0 and a bold name.
                let mut spans = line.spans;
                if let Some(first) = spans.first_mut() {
                    let rest: String = first.content.chars().skip(1).collect();
                    let style = first.style;
                    spans.splice(
                        0..1,
                        [
                            Span::styled(theme.glyph(Glyph::Marker), theme.focus()),
                            Span::styled(rest, style),
                        ],
                    );
                }
                Line::from(spans).patch_style(Style::new().add_modifier(Modifier::BOLD))
            } else {
                line
            }
        })
        .collect();
    if rows.len() == 1 && lane.units.is_empty() && lane.state == "no-config" {
        lines.push(Line::from(""));
        for text in text::wrap(
            "services, tasks and infra show once devlanes.yaml loads",
            width - 2,
        ) {
            lines.push(Line::styled(format!(" {text}"), theme.quiet()));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// `feat/checkout`, or `detached HEAD`.
pub fn lane_name(lane: &LaneSummary) -> String {
    lane.branch
        .as_deref()
        .map_or_else(|| "detached HEAD".to_owned(), clean)
}

/// `left` and `right` in one row with a cell of margin on the right, as in
/// `│ ● whole lane                 running · 33 up │`.
fn row(left: Vec<Span<'static>>, mut right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    // Details on the right drop from the end (`· pid 48211`) before the
    // name is cut; the first part (the state) stays.
    while right.len() > 2 && spans_width(&left) + spans_width(&right) + 2 > width {
        right.truncate(right.len() - 2);
    }
    if right.is_empty() {
        return fit(left, width);
    }
    right.push(Span::raw(" "));
    spread(left, right, width)
}

/// `a · b · c` from parts.
fn joined(parts: Vec<Span<'static>>, quiet: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for part in parts {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", quiet));
        }
        spans.push(part);
    }
    spans
}

/// One row, starting at column 0 (the marker column).
fn row_line(app: &App, lane: &LaneSummary, item: &Row, width: usize) -> Line<'static> {
    let theme = &app.theme;
    // Headers: the arrow at two cells per depth, after the marker column.
    let indent = " ".repeat(1 + 2 * usize::from(item.depth));
    let header = |label: String, right: Vec<Span<'static>>| {
        let arrow = if item.folded == Some(true) {
            theme.glyph(Glyph::Folded)
        } else {
            theme.glyph(Glyph::Open)
        };
        let left = vec![
            Span::raw(indent.clone()),
            Span::styled(format!("{arrow} "), theme.quiet()),
            Span::raw(label),
        ];
        row(left, right, width)
    };
    match &item.id {
        RowId::Gap(_) | RowId::WholeLane => Line::from(""),
        RowId::Section(section) => {
            let summary = match section {
                Section::Infra if item.folded != Some(true) => Vec::new(),
                _ if item.folded == Some(true) => members_summary(app, lane, &item.members),
                _ => vec![Span::styled(item.members.len().to_string(), theme.quiet())],
            };
            header(section.name().into(), summary)
        }
        RowId::Group(_, name) => header(clean(name), members_summary(app, lane, &item.members)),
        RowId::Unit(name) => match lane.units.iter().find(|u| u.name == *name) {
            Some(unit) => unit_line(app, lane, unit, item, width),
            None => Line::from(format!("{indent}{}", clean(name))),
        },
        RowId::Task(name) => {
            let info = app.task_info(&lane.reference, name);
            let look = task_look(theme, info, app.spin);
            let mut right = vec![Span::styled(look.word.clone(), theme.tone(look.word_tone))];
            if !look.detail.is_empty() && look.tone == Tone::Failure {
                right.push(Span::styled(look.detail.clone(), theme.tone(Tone::Failure)));
            }
            if info.is_some_and(|i| i.runs_at_start()) {
                right.push(Span::styled("runs at start", theme.quiet()));
            }
            let quiet = look.tone == Tone::Quiet;
            let left = leaf(app, item, None, (look.glyph, look.tone), clean(name), quiet);
            row(left, joined(right, theme.quiet()), width)
        }
        RowId::Container(name) => {
            let Some(container) = app.find_container(lane, name) else {
                return Line::from(format!("{indent}{}", clean(name)));
            };
            let (glyph, tone, word) = container_look(app, &container);
            let mut right = Vec::new();
            if tone != Tone::Ok {
                right.push(Span::styled(word, theme.tone(tone)));
            }
            // Hidden in `infra.hidden`: listed, dimmed, as hidden services.
            let left = leaf(
                app,
                item,
                container.color.as_deref(),
                (glyph, tone),
                clean(name),
                container.hidden,
            );
            row(left, joined(right, theme.quiet()), width)
        }
    }
}

/// A unit, task or container: the glyph, then the name at its depth in the
/// family's color (dimmed when it does not run or is hidden).
fn leaf(
    app: &App,
    item: &Row,
    color: Option<&str>,
    (glyph, tone): (&'static str, Tone),
    name: String,
    quiet: bool,
) -> Vec<Span<'static>> {
    let theme = &app.theme;
    let mut style = color
        .and_then(|c| theme.identity(c))
        .map_or_else(Style::new, |color| Style::new().fg(color));
    if quiet {
        style = style.patch(theme.quiet());
    }
    vec![
        Span::raw(" ".repeat(1 + 2 * usize::from(item.depth))),
        Span::styled(format!("{glyph} "), theme.tone(tone)),
        Span::styled(name, style),
    ]
}

/// A container's glyph, tone and state word.
pub(crate) fn container_look(
    app: &App,
    container: &devlanes_proto::ContainerSummary,
) -> (&'static str, Tone, String) {
    let theme = &app.theme;
    match tree::container_tone(container) {
        ContainerTone::Up => (theme.glyph(Glyph::Ready), Tone::Ok, "up".into()),
        ContainerTone::Busy => (
            theme.spinner(app.spin),
            Tone::Busy,
            container.state.clone().unwrap_or_default(),
        ),
        ContainerTone::Failed => (
            theme.glyph(Glyph::Failed),
            Tone::Failure,
            clean(container.state.as_deref().unwrap_or("failed")),
        ),
        ContainerTone::Down => (
            theme.glyph(Glyph::Stopped),
            Tone::Quiet,
            clean(container.state.as_deref().unwrap_or("down")),
        ),
    }
}

/// `   ● api-auth`, `   ✕ wrk-export      failed · exit 1`.
fn unit_line(
    app: &App,
    lane: &LaneSummary,
    unit: &UnitSummary,
    item: &Row,
    width: usize,
) -> Line<'static> {
    let theme = &app.theme;
    let look = unit_look(theme, unit, app.spin);
    let role = tree::role(unit);
    // Tasks and setup steps keep the default color: they don't run long.
    let color = match role {
        tree::Role::Service | tree::Role::Exec => unit.color.as_deref(),
        _ => None,
    };
    let task = matches!(
        role,
        tree::Role::Task | tree::Role::Setup | tree::Role::Teardown
    );
    let mut right = Vec::new();
    match target_error(app, &lane.reference, Some(&unit.name)) {
        // A failed action says more than the state.
        Some(entry) => right.push(Span::styled(
            format!("{} failed", clean(entry.verb())),
            theme.tone(Tone::Failure),
        )),
        None => {
            // Services in a normal state are their glyph alone; tasks keep
            // `done`, as the tasks section reads as a checklist.
            if task || !SILENT.contains(&look.word.as_str()) {
                right.push(Span::styled(look.word.clone(), theme.tone(look.word_tone)));
            }
            let detail_tone = if look.tone == Tone::Failure {
                Tone::Failure
            } else {
                Tone::Quiet
            };
            if !look.detail.is_empty() && (!task || look.tone == Tone::Failure) {
                right.push(Span::styled(look.detail.clone(), theme.tone(detail_tone)));
            }
            if task
                && app
                    .task_info(&lane.reference, &unit.name)
                    .is_some_and(|i| i.runs_at_start())
            {
                right.push(Span::styled("runs at start", theme.quiet()));
            }
        }
    }
    // The name takes the family color; a unit that does not run, or a
    // hidden one, is dimmed.
    let quiet = look.tone == Tone::Quiet || unit.hidden;
    let left = leaf(
        app,
        item,
        color,
        (look.glyph, look.tone),
        clean(unit.label()),
        quiet,
    );
    row(left, joined(right, theme.quiet()), width)
}

/// Counts of the members under a header: `4 up`, `7 running · 1 failed`.
/// Healthy counts are dim; problems keep their color.
fn members_summary(app: &App, lane: &LaneSummary, members: &[RowId]) -> Vec<Span<'static>> {
    let theme = &app.theme;
    let mut counts: Vec<(String, Tone, usize)> = Vec::new();
    let mut add = |word: String, tone: Tone| match counts.iter_mut().find(|(w, _, _)| *w == word) {
        Some(entry) => entry.2 += 1,
        None => counts.push((word, tone, 1)),
    };
    for member in members {
        match member {
            RowId::Unit(name) => {
                if let Some(unit) = lane.units.iter().find(|u| u.name == *name) {
                    let look = unit_look(theme, unit, 0);
                    add(look.word, look.tone);
                }
            }
            RowId::Task(name) => {
                let look = task_look(theme, app.task_info(&lane.reference, name), 0);
                add(look.word, look.tone);
            }
            RowId::Container(name) => {
                if let Some(container) = app.find_container(lane, name) {
                    let (_, tone, word) = container_look(app, &container);
                    add(word, tone);
                }
            }
            _ => {}
        }
    }
    // Healthy and quiet first, problems last where the eye ends.
    counts.sort_by_key(|(_, tone, _)| match tone {
        Tone::Ok => 0,
        Tone::Busy => 1,
        Tone::Quiet => 2,
        Tone::Attention => 3,
        Tone::Failure => 4,
    });
    let parts = counts
        .into_iter()
        .map(|(word, tone, count)| {
            let style = match tone {
                Tone::Failure | Tone::Attention => theme.tone(tone),
                _ => theme.quiet(),
            };
            Span::styled(format!("{count} {word}"), style)
        })
        .collect();
    joined(parts, theme.quiet())
}

/// The lane's summary: `running · 33 up`, `1 failed · 32 up`, `starting ·
/// 5 of 9 ready`, with badges for cleaning, config and failed actions.
pub fn lane_summary(app: &App, lane: &LaneSummary) -> Vec<Span<'static>> {
    let theme = &app.theme;
    let counts = tree::counts(lane);
    let mut parts: Vec<Span<'static>> = Vec::new();
    let mut push = |text: String, style: Style| parts.push(Span::styled(text, style));
    if app.is_cleaning(&lane.reference) {
        push("cleaning…".into(), theme.tone(Tone::Busy));
    }
    if let Some(entry) = target_error(app, &lane.reference, None) {
        push(
            format!("{} failed", clean(entry.verb())),
            theme.tone(Tone::Failure),
        );
    }
    let missing = lane
        .reason
        .as_ref()
        .is_some_and(|r| r.code == "worktree_missing");
    match lane.state.as_str() {
        _ if missing => push("worktree missing".into(), theme.tone(Tone::Attention)),
        "running" | "failed" => {
            if counts.failed > 0 {
                push(
                    format!("{} failed", counts.failed),
                    theme.tone(Tone::Failure),
                );
            } else if lane.state == "failed" {
                push("start failed".into(), theme.tone(Tone::Failure));
            } else {
                push("running".into(), theme.quiet());
            }
            // Orphans have their own row and the top bar's counter.
            if counts.starting > 0 {
                push(
                    format!("{} starting", counts.starting),
                    theme.tone(Tone::Busy),
                );
            }
            push(format!("{} up", counts.up), theme.quiet());
        }
        "starting" => {
            push("starting".into(), theme.tone(Tone::Busy));
            push(
                format!("{} of {} ready", counts.up, counts.total),
                theme.quiet(),
            );
        }
        "stopping" => push("stopping".into(), theme.quiet()),
        "idle" => push("not running".into(), theme.quiet()),
        "unassigned" => push("unassigned".into(), theme.tone(Tone::Attention)),
        // A reason means the file is there but does not load.
        "no-config" if lane.reason.is_some() => {
            push("config has an error".into(), theme.tone(Tone::Attention));
        }
        "no-config" => push("no devlanes.yaml".into(), theme.tone(Tone::Attention)),
        other => push(clean(other), theme.quiet()),
    }
    if lane.config_changed {
        push("config changed".into(), theme.quiet());
    }
    if let Some(next) = lane.moves_to {
        push(format!("becomes @{next} on next start"), theme.quiet());
    }
    joined(parts, theme.quiet())
}
