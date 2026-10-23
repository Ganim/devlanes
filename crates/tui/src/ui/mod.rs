//! Rendering. Reads [`App`] and draws; the only state it writes is the map of
//! clickable chrome ([`App::hits`]), so hit tests match what was drawn.
//!
//! Every string from the daemon is cleaned and fitted to its widget before it
//! reaches the buffer (see [`crate::text`]), so long or hostile text cannot
//! spill into another widget or move the real terminal's cursor.

pub(crate) mod daemon;
mod dialog;
mod list;
mod monitor;
mod pane;

use crossterm::event::{KeyCode, KeyModifiers};
use devlanes_proto::{LaneSummary, ORPHANED, UnitSummary};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::{
    App, AttachState, Click, Connection, Focus, MIN_HEIGHT, MIN_WIDTH, PaneMode, Target, View,
};
use crate::text::{self, clean, clip};
use crate::theme::{Glyph, Theme, Tier, Tone};
use crate::tree::{self, Role, RowId};

pub use list::lane_summary;
pub use monitor::format_bytes;

/// Draws one frame.
pub fn render(app: &App, frame: &mut Frame) {
    app.hits.borrow_mut().clear();
    *app.dialog_area.borrow_mut() = None;
    let size = frame.area();
    let Some(layout) = app.areas_sized(size.width, size.height) else {
        render_too_small(frame, size);
        return;
    };
    render_header(app, frame, layout.header);
    match app.view {
        View::Project => {
            if app.lanes.is_empty() {
                render_welcome(app, frame, layout.body);
            } else {
                if let Some(bar) = layout.lane_bar {
                    render_lane_bar(app, frame, bar);
                }
                if let (Some(boxed), Some(rows)) = (layout.list_box, layout.list) {
                    list::render(app, frame, boxed, rows);
                }
                pane::render(app, frame, layout.pane_box, layout.pane_inner);
            }
        }
        View::Monitor => monitor::render(app, frame, layout.body),
        View::Daemon => daemon::render(app, frame, layout.body),
    }
    if let Some(strip) = layout.strip {
        render_strip(app, frame, strip);
    }
    render_footer(app, frame, layout.footer);
    if let Some(dialog) = &app.dialog {
        dialog::render(app, dialog, frame, size);
    }
}

/// Draws a box with `left` in its top border and `right` at the end of it,
/// as the navigation.md mockups draw the list and the pane:
/// `┌ main @0 ───┐`. The border takes the focus color when `accent`.
pub(crate) fn draw_box(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    accent: bool,
) {
    use ratatui::{symbols::border, widgets::Block};
    let theme = &app.theme;
    let set = if theme.glyphs == crate::theme::Glyphs::Ascii {
        border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        }
    } else {
        border::PLAIN
    };
    let style = if accent { theme.focus() } else { theme.rule() };
    // Titles are drawn over the border's cells; without their own color
    // they would take the border's gray.
    let own = |spans: Vec<Span<'static>>| -> Vec<Span<'static>> {
        spans
            .into_iter()
            .map(|span| {
                let style = if span.style.fg.is_none() {
                    span.style.fg(ratatui::style::Color::Reset)
                } else {
                    span.style
                };
                Span::styled(span.content, style)
            })
            .collect()
    };
    let (left, right) = (own(left), own(right));
    // The right title drops before the left one is cut, and the left one
    // never runs into the corner.
    let room = usize::from(area.width).saturating_sub(4);
    let right_width = spans_width(&right);
    let fits_right = right_width > 0 && spans_width(&left) + right_width + 2 <= room;
    let left = fit(left, room);
    let mut block = Block::bordered()
        .border_set(set)
        .border_style(style)
        .title_top(left);
    // The right title drops before the left one is cut.
    if fits_right {
        block = block.title_top(Line::from(right).right_aligned());
    }
    frame.render_widget(block, area);
}

/// Records a clickable rectangle for the mouse.
fn hit(app: &App, rect: Rect, click: Click) {
    if rect.width > 0 && rect.height > 0 {
        app.hits.borrow_mut().push((rect, click));
    }
}

/// Cuts styled spans to `width` cells, ending in `…` where text was cut.
pub(crate) fn fit(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
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

/// Spans on the left and right of one row of `width` cells; the left side
/// is cut first.
pub(crate) fn spread(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let right_width: usize = right.iter().map(|s| text::width(&s.content)).sum();
    if right_width + 1 >= width {
        return fit(left, width);
    }
    let left = fit(left, width - right_width - 1);
    let gap = width - left.width() - right_width;
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// Total width of spans.
pub(crate) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| text::width(&s.content)).sum()
}

/// Shown instead of the layout below the minimum size.
fn render_too_small(frame: &mut Frame, area: Rect) {
    let width = usize::from(area.width);
    let text = vec![
        Line::from(clip(
            &format!("terminal is {}x{}", area.width, area.height),
            width,
        )),
        Line::from(clip(
            &format!("devlanes needs {MIN_WIDTH}x{MIN_HEIGHT} or larger"),
            width,
        ))
        .style(Style::new().add_modifier(Modifier::DIM)),
    ];
    let top = area.height.saturating_sub(2) / 2;
    let area = Rect::new(area.x, area.y + top, area.width, area.height - top);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}

/// The look of a unit: glyph, its tone, the state word, the word's tone and
/// the detail column.
pub(crate) struct Look {
    pub glyph: &'static str,
    pub tone: Tone,
    pub word: String,
    pub word_tone: Tone,
    pub detail: String,
}

/// How a unit's state is drawn, by its role (visual.md's glyph table).
pub(crate) fn unit_look(theme: &Theme, unit: &UnitSummary, spin: u64) -> Look {
    let task = matches!(tree::role(unit), Role::Task | Role::Setup | Role::Teardown);
    let exit = unit.exit_code.map(|code| format!("exit {code}"));
    let look = |glyph, tone, word: &str, word_tone, detail: Option<String>| Look {
        glyph,
        tone,
        word: word.into(),
        word_tone,
        detail: detail.unwrap_or_default(),
    };
    let spinner = theme.spinner(spin);
    match (unit.state.as_str(), task) {
        // A ready check passed (`●`), or the process merely runs (`◉`): the
        // shapes differ so 16 colors and color-blind eyes tell them apart.
        ("ready" | "running", false) => look(
            theme.glyph(if unit.shown_state() == "ready" {
                Glyph::Ready
            } else {
                Glyph::Running
            }),
            Tone::Ok,
            unit.shown_state(),
            Tone::Quiet,
            None,
        ),
        ("running", true) => look(spinner, Tone::Busy, "running", Tone::Busy, None),
        ("starting", _) => look(spinner, Tone::Busy, "starting", Tone::Busy, None),
        ("stopping", _) => look(spinner, Tone::Quiet, "stopping", Tone::Quiet, None),
        ("queued", _) => look(
            theme.glyph(Glyph::Queued),
            Tone::Quiet,
            "queued",
            Tone::Quiet,
            None,
        ),
        ("failed", _) => look(
            theme.glyph(Glyph::Failed),
            Tone::Failure,
            "failed",
            Tone::Failure,
            exit,
        ),
        ("exited", true) if unit.exit_code == Some(0) => look(
            theme.glyph(Glyph::Done),
            Tone::Ok,
            "done",
            Tone::Quiet,
            unit.finished_at_ms.map(ago),
        ),
        ("exited", true) if unit.exit_code.is_some() => look(
            theme.glyph(Glyph::Failed),
            Tone::Failure,
            "failed",
            Tone::Failure,
            exit,
        ),
        ("exited", true) => look(
            theme.glyph(Glyph::Exited),
            Tone::Quiet,
            "ended",
            Tone::Quiet,
            None,
        ),
        ("exited", false) => look(
            theme.glyph(Glyph::Exited),
            Tone::Quiet,
            "exited",
            Tone::Quiet,
            exit,
        ),
        (ORPHANED, _) => look(
            theme.glyph(Glyph::Orphaned),
            Tone::Attention,
            "orphaned",
            Tone::Attention,
            unit.pid.map(|pid| format!("pid {pid}")),
        ),
        (_, true) => look(
            theme.glyph(Glyph::Stopped),
            Tone::Quiet,
            "not run",
            Tone::Quiet,
            None,
        ),
        (_, false) => look(
            theme.glyph(Glyph::Stopped),
            Tone::Quiet,
            "stopped",
            Tone::Quiet,
            None,
        ),
    }
}

/// A task that is not a unit now, from its last stored run in `task.list`.
pub(crate) fn task_look(theme: &Theme, info: Option<&crate::api::TaskInfo>, spin: u64) -> Look {
    let run = info.and_then(|i| i.last_run.as_ref());
    let ago = run
        .and_then(|r| r.finished_at_ms)
        .map(ago)
        .unwrap_or_default();
    let look = |glyph, tone, word: &str, word_tone, detail: String| Look {
        glyph,
        tone,
        word: word.into(),
        word_tone,
        detail,
    };
    match run.map(|r| r.state.as_str()) {
        Some("running") => look(
            theme.spinner(spin),
            Tone::Busy,
            "running",
            Tone::Busy,
            String::new(),
        ),
        Some("done") => look(theme.glyph(Glyph::Done), Tone::Ok, "done", Tone::Quiet, ago),
        Some("failed") => {
            let exit = run
                .and_then(|r| r.exit_code)
                .map(|code| format!("exit {code}"))
                .unwrap_or_default();
            look(
                theme.glyph(Glyph::Failed),
                Tone::Failure,
                "failed",
                Tone::Failure,
                exit,
            )
        }
        Some(_) => look(
            theme.glyph(Glyph::Exited),
            Tone::Quiet,
            "ended",
            Tone::Quiet,
            ago,
        ),
        None => look(
            theme.glyph(Glyph::Stopped),
            Tone::Quiet,
            "not run",
            Tone::Quiet,
            String::new(),
        ),
    }
}

/// `3m ago` for a Unix millisecond time.
pub(crate) fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The state word with its detail, for panes and tables (`failed  exit 1`).
pub fn unit_state(unit: &UnitSummary) -> String {
    let look = unit_look(&Theme::default(), unit, 0);
    if look.detail.is_empty() || look.detail.starts_with(':') {
        look.word
    } else {
        format!("{}  {}", look.word, look.detail)
    }
}

/// The glyph and tone for a lane in tabs and chips.
pub(crate) fn lane_glyph(theme: &Theme, lane: &LaneSummary, spin: u64) -> (&'static str, Tone) {
    let missing = lane
        .reason
        .as_ref()
        .is_some_and(|reason| reason.code == "worktree_missing");
    match lane.state.as_str() {
        _ if missing => (theme.glyph(Glyph::Attention), Tone::Attention),
        "running" => (theme.glyph(Glyph::Ready), Tone::Ok),
        "starting" | "stopping" => (theme.spinner(spin), Tone::Busy),
        "failed" => (theme.glyph(Glyph::Failed), Tone::Failure),
        "no-config" => (theme.glyph(Glyph::Attention), Tone::Attention),
        _ => (theme.glyph(Glyph::Stopped), Tone::Quiet),
    }
}

/// How bad a lane state is, for a project tab's glyph: the worst lane wins.
fn severity(lane: &LaneSummary) -> u8 {
    match lane_glyph(&Theme::default(), lane, 0).1 {
        Tone::Failure => 4,
        Tone::Attention => 3,
        Tone::Busy => 2,
        Tone::Ok => 1,
        Tone::Quiet => 0,
    }
}

/// ` devlanes   1 acme ●   2 shop ·` on the left, the message, then
/// `M monitor   D daemon` on the right.
fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let width = usize::from(area.width);
    // The view buttons, right-aligned.
    let badge = daemon_badge(app);
    // Below 100 columns the badge's words would crowd out the message, so
    // the buttons of other views drop their labels; their keys stay.
    let compact = badge.is_some() && width < 100;
    let mut buttons: Vec<(Vec<Span<'static>>, KeyCode)> = Vec::new();
    for (key, label, view) in [
        ('M', "monitor", View::Monitor),
        ('D', "daemon", View::Daemon),
    ] {
        let active = app.view == view;
        let label_style = if active {
            theme
                .focus()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme.quiet()
        };
        let mut spans = vec![Span::styled(key.to_string(), theme.key())];
        if active || !compact {
            spans.push(Span::styled(format!(" {label}"), label_style));
        }
        // `D daemon · 1 issue`, `D daemon · update`: a count alone said nothing.
        if view == View::Daemon
            && let Some((text, tone)) = &badge
        {
            spans.push(Span::styled(format!(" · {text}"), theme.tone(*tone)));
        }
        buttons.push((spans, KeyCode::Char(key)));
    }
    let buttons_width: usize = buttons.iter().map(|(s, _)| spans_width(s)).sum::<usize>() + 3 + 1;
    let mut x = area.right().saturating_sub(buttons_width as u16);
    let mut right: Vec<Span> = Vec::new();
    for (index, (spans, key)) in buttons.into_iter().enumerate() {
        if index > 0 {
            right.push(Span::raw("   "));
            x += 3;
        }
        let w = spans_width(&spans) as u16;
        hit(
            app,
            Rect::new(x, area.y, w, 1),
            Click::Key(key, KeyModifiers::SHIFT),
        );
        x += w;
        right.extend(spans);
    }
    right.push(Span::raw(" "));

    // Brand, letter by letter from yellow to orange, and project tabs.
    let mut left = vec![Span::raw(" ")];
    let brand = "devlanes";
    left.extend(
        brand.chars().enumerate().map(|(index, letter)| {
            Span::styled(letter.to_string(), theme.brand(index, brand.len()))
        }),
    );
    let mut used = 9;
    let projects = app.projects();
    // One project needs no tabs: its name, then its lanes on this same row.
    let single = projects.len() <= 1;
    let current = app.project().map(str::to_owned);
    let tabs: Vec<(String, Vec<Span<'static>>)> = projects
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let lanes: Vec<&LaneSummary> =
                app.lanes.iter().filter(|l| l.project == *name).collect();
            let worst = lanes.iter().max_by_key(|lane| severity(lane)).copied();
            let (glyph, tone) =
                worst.map_or(("", Tone::Quiet), |lane| lane_glyph(theme, lane, app.spin));
            let active = app.view == View::Project && current.as_deref() == Some(name.as_str());
            let number = if index < 9 && !single {
                format!("{} ", index + 1)
            } else {
                String::new()
            };
            let glyph = if needs_glyph(theme) && !glyph.is_empty() {
                format!("{glyph} ")
            } else {
                String::new()
            };
            // The active tab is one yellow block; the others' names take the
            // color of their worst lane.
            let spans = if single {
                vec![Span::styled(
                    format!(" {}{glyph} ", clean(name)),
                    Style::new().add_modifier(Modifier::BOLD),
                )]
            } else if active {
                vec![Span::styled(
                    format!(" {number}{} {glyph}", clean(name)),
                    theme.tab(),
                )]
            } else {
                vec![
                    Span::raw(" "),
                    Span::styled(number, theme.quiet()),
                    Span::styled(clean(name), state_style(theme, tone)),
                    Span::raw(format!(" {glyph}")),
                ]
            };
            (name.clone(), spans)
        })
        .collect();
    // The message takes what is left; tabs come first and scroll to keep the
    // shown project in view.
    let message = header_message(app);
    let room = width.saturating_sub(buttons_width + used);
    // Each tab pads itself; one space separates them.
    let tab_widths: Vec<usize> = tabs.iter().map(|(_, s)| spans_width(s) + 1).collect();
    let active = current
        .as_ref()
        .and_then(|c| projects.iter().position(|p| p == c))
        .unwrap_or(0);
    let mut first = 0;
    while first < active && tab_widths[first..=active].iter().sum::<usize>() + 2 > room {
        first += 1;
    }
    if first > 0 {
        left.push(Span::styled(" ‹", theme.quiet()));
        used += 2;
    }
    for (index, (name, spans)) in tabs.into_iter().enumerate().skip(first) {
        let w = tab_widths[index];
        if used + w + 2 > width.saturating_sub(buttons_width) {
            left.push(Span::styled(" ›", theme.quiet()));
            used += 2;
            break;
        }
        left.push(Span::raw(" "));
        hit(
            app,
            Rect::new(area.x + used as u16 + 1, area.y, (w - 1) as u16, 1),
            Click::Project(name),
        );
        left.extend(spans);
        used += w;
    }
    if single && app.view == View::Project && app.project_lanes().len() > 1 {
        // A message (a start's progress, an error) keeps up to 32 columns;
        // lanes past that fold into `+N more`.
        let message_room = message
            .as_ref()
            .map_or(0, |(spans, _)| spans_width(spans).min(32) + 4);
        let budget = width.saturating_sub(buttons_width + 2 + message_room);
        let (chips, after) = lane_chips(app, area, used, budget);
        left.extend(chips);
        used = after;
    }
    // The message, right-aligned before the buttons.
    let free = width.saturating_sub(used + buttons_width + 4);
    let mut middle = Vec::new();
    if let Some((spans, click)) = message
        && free > 4
    {
        let line = fit(spans, free);
        let w = line.width();
        middle.push(Span::raw(" ".repeat(free - w + 2)));
        if let Some(click) = click {
            let x = area.x + (used + 2 + free - w) as u16;
            hit(app, Rect::new(x, area.y, w as u16, 1), click);
        }
        middle.extend(line.spans);
        middle.push(Span::raw("  "));
    } else {
        middle.push(Span::raw(" ".repeat(free + 4)));
    }
    let mut spans = left;
    spans.extend(middle);
    spans.extend(right);
    frame.render_widget(Paragraph::new(fit(spans, width)), area);
}

/// Failing and warning doctor checks, plus a daemon that runs another
/// version or a newer release: the count and tone of the daemon button's `!`.
fn daemon_badge(app: &App) -> Option<(String, Tone)> {
    let mut fail = 0;
    let mut warn = 0;
    if let Some(doctor) = &app.daemon.doctor {
        for check in crate::doctor::checks(doctor) {
            match check.state {
                crate::doctor::CheckState::Fail => fail += 1,
                crate::doctor::CheckState::Warn => warn += 1,
                crate::doctor::CheckState::Ok => {}
            }
        }
    }
    let update = app.release_badge().is_some();
    let issues = fail + warn;
    let mut text = match issues {
        0 if !update => return None,
        0 => "update".to_owned(),
        1 => "1 issue".to_owned(),
        n => format!("{n} issues"),
    };
    if update && issues > 0 {
        text.push_str(", update");
    }
    let tone = if fail > 0 {
        Tone::Failure
    } else {
        Tone::Attention
    };
    Some((text, tone))
}

/// The one message the top bar shows, the first that applies.
fn header_message(app: &App) -> Option<(Vec<Span<'static>>, Option<Click>)> {
    let theme = &app.theme;
    let plain = |text: String, tone: Tone| Some((vec![Span::styled(text, theme.tone(tone))], None));
    if let Some(busy) = app.daemon.busy {
        let busy = if busy == "collecting lanes" {
            "cleaning lanes with missing worktrees"
        } else {
            busy
        };
        return plain(format!("daemon {busy}…"), Tone::Busy);
    }
    if app.daemon.stopped {
        return plain(
            "daemon stopped: S on the daemon page starts it".into(),
            Tone::Quiet,
        );
    }
    match app.connection {
        Connection::Reconnecting { .. } => {
            return plain("daemon not responding, retrying…".into(), Tone::Failure);
        }
        Connection::Connecting if app.start_line().is_none() => {
            return plain("connecting to the daemon…".into(), Tone::Busy);
        }
        _ => {}
    }
    if let Some(notice) = &app.notice
        && notice.error
    {
        let text = format!("{} {}", theme.glyph(Glyph::Failed), notice.text);
        return plain(text, Tone::Failure);
    }
    if let Some((line, ended)) = app.start_line() {
        let tone = match ended {
            None => Tone::Busy,
            Some(true) => Tone::Ok,
            Some(false) => Tone::Failure,
        };
        let click = app
            .start_run
            .as_ref()
            .and_then(|run| run.lanes.first())
            .map(|lane| Click::StartLane(lane.clone()));
        return Some((vec![Span::styled(line, theme.tone(tone))], click));
    }
    if let Some(notice) = &app.notice {
        return plain(notice.text.clone(), Tone::Busy);
    }
    // A start this TUI did not send: the first starting lane of the project.
    if let Some(lane) = app
        .project_lanes()
        .into_iter()
        .find(|l| l.state == "starting")
    {
        let counts = tree::counts(lane);
        let what = lane.branch.as_deref().unwrap_or(&lane.reference);
        let phase = app
            .phases
            .get(&lane.reference)
            .and_then(|phases| phases.steps.iter().rev().find(|s| s.state == "started"))
            .map(|step| step.phase.clone())
            .unwrap_or_else(|| "services".into());
        let text = if phase == "services" {
            format!(
                "starting {}: {} of {} ready",
                clean(what),
                counts.up,
                counts.total
            )
        } else {
            format!("starting {}: {}", clean(what), clean(&phase))
        };
        return Some((
            vec![Span::styled(text, theme.tone(Tone::Busy))],
            Some(Click::StartLane(lane.reference.clone())),
        ));
    }
    // Problems in the project, clickable.
    let mut failed = 0;
    let mut orphaned = 0;
    for lane in app.project_lanes() {
        let counts = tree::counts(lane);
        failed += counts.failed;
        orphaned += counts.orphaned;
    }
    let mut spans = Vec::new();
    if failed > 0 {
        spans.push(Span::styled(
            format!("{} {failed} failed", theme.glyph(Glyph::Failed)),
            theme.tone(Tone::Failure),
        ));
    }
    if orphaned > 0 {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{} {orphaned} orphaned", theme.glyph(Glyph::Orphaned)),
            theme.tone(Tone::Attention),
        ));
    }
    (!spans.is_empty()).then_some((spans, Some(Click::FirstProblem)))
}

/// ` lanes  1 main  2 feat/checkout 5/9` and the switch hint, under the
/// project tabs when there are several projects.
fn render_lane_bar(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let width = usize::from(area.width);
    let hint = vec![
        Span::styled("1-9", theme.key()),
        Span::styled(" or ", theme.quiet()),
        Span::styled("[ ]", theme.key()),
        Span::styled(" switch lanes ", theme.quiet()),
    ];
    let budget = width.saturating_sub(spans_width(&hint) + 2);
    let mut spans = vec![Span::styled(" lanes ", theme.quiet())];
    spans.extend(lane_chips(app, area, 7, budget).0);
    frame.render_widget(Paragraph::new(spread(spans, hint, width)), area);
}

/// The project's lane chips from column `used` of `area`, up to column
/// `budget`: `1 main  2 feat/checkout`, the shown lane highlighted, `+N more`
/// for those that do not fit. Like the project tabs, they scroll (`‹`) to
/// keep the shown lane in view. Registers each chip's click; returns the
/// spans and the column after them.
fn lane_chips(
    app: &App,
    area: Rect,
    mut used: usize,
    budget: usize,
) -> (Vec<Span<'static>>, usize) {
    let theme = &app.theme;
    let lanes = app.project_lanes();
    let current = app.selected.as_ref().map(|s| s.lane.as_str());
    let chips: Vec<Vec<Span<'static>>> = lanes
        .iter()
        .enumerate()
        .map(|(index, lane)| lane_chip(app, lane, &lanes, index))
        .collect();
    // Each chip pads itself with a space on both sides, so the highlight is
    // a block; that padding is also the gap between chips.
    let widths: Vec<usize> = chips.iter().map(|chip| spans_width(chip) + 2).collect();
    let active = lanes
        .iter()
        .position(|lane| current == Some(lane.reference.as_str()))
        .unwrap_or(0);
    let mut first = 0;
    let room = budget.saturating_sub(used + 2);
    while first < active && widths[first..=active].iter().sum::<usize>() > room {
        first += 1;
    }
    let mut spans = Vec::new();
    if first > 0 {
        spans.push(Span::styled(" ‹", theme.quiet()));
        used += 2;
    }
    for (index, (lane, chip)) in lanes.iter().zip(chips).enumerate().skip(first) {
        let w = widths[index];
        let more = lanes.len() - index;
        // A chip with lanes after it leaves room for ` +N more`; those up to
        // the shown lane fit already, the scroll above saw to that.
        let reserve = if more > 1 && index > active {
            format!(" +{} more", more - 1).len()
        } else {
            0
        };
        // The shown lane always draws; a message beside the chips gives way.
        if used + w + reserve > budget && index != active {
            let text = format!(" +{more} more");
            hit(
                app,
                Rect::new(area.x + used as u16 + 1, area.y, (text.len() - 1) as u16, 1),
                Click::Key(KeyCode::Char(']'), KeyModifiers::NONE),
            );
            used += text.len();
            spans.push(Span::styled(text, theme.quiet()));
            break;
        }
        hit(
            app,
            Rect::new(area.x + used as u16, area.y, w as u16, 1),
            Click::Lane(lane.reference.clone()),
        );
        let mut chip_spans = vec![Span::raw(" ")];
        chip_spans.extend(chip);
        chip_spans.push(Span::raw(" "));
        if current == Some(lane.reference.as_str()) {
            chip_spans = chip_spans
                .into_iter()
                .map(|span| {
                    let style = theme.selected(span.style).add_modifier(Modifier::BOLD);
                    Span::styled(span.content, style)
                })
                .collect();
        }
        spans.extend(chip_spans);
        used += w;
    }
    (spans, used)
}

/// What a lane is called in the chips: an extra lane's name, else its
/// branch; `@N` only when another lane of the project has the same name.
pub(crate) fn lane_label(lane: &LaneSummary, lanes: &[&LaneSummary]) -> (String, Option<String>) {
    let label = |lane: &LaneSummary| {
        lane.name
            .clone()
            .or_else(|| lane.branch.clone())
            .map_or_else(|| "detached".to_owned(), |name| clean(&name))
    };
    let own = label(lane);
    let shared = lanes
        .iter()
        .any(|other| other.reference != lane.reference && label(other) == own);
    let number = lane
        .index
        .filter(|_| shared)
        .map(|index| format!("@{index}"));
    (own, number)
}

/// The color of a lane or project name for its state: green when running,
/// yellow while it starts or needs a look, red when it failed, dim when idle.
fn state_style(theme: &Theme, tone: Tone) -> Style {
    match tone {
        // Busy is default text elsewhere (a spinner shows motion); a name has
        // no spinner, so it takes the attention color.
        Tone::Busy => theme.tone(Tone::Attention),
        tone => theme.tone(tone),
    }
}

/// Without colors the state needs its glyph, since the name cannot carry it.
fn needs_glyph(theme: &Theme) -> bool {
    theme.tier == Tier::Plain
}

/// One chip: its key (1-9, then 0), the name colored by state, `@N` when the
/// name is shared, the ready count while starting.
fn lane_chip(
    app: &App,
    lane: &LaneSummary,
    lanes: &[&LaneSummary],
    index: usize,
) -> Vec<Span<'static>> {
    let theme = &app.theme;
    let (label, number) = lane_label(lane, lanes);
    let (glyph, tone) = lane_glyph(theme, lane, app.spin);
    let mut spans = Vec::new();
    if index < 10 {
        spans.push(Span::styled(
            format!("{} ", (index + 1) % 10),
            theme.quiet(),
        ));
    }
    spans.push(Span::styled(label, state_style(theme, tone)));
    if let Some(number) = number {
        spans.push(Span::styled(number, theme.quiet()));
    }
    if needs_glyph(theme) {
        spans.push(Span::raw(format!(" {glyph}")));
    }
    if lane.state == "starting" {
        let counts = tree::counts(lane);
        spans.push(Span::styled(
            format!(" {}/{}", counts.up, counts.total),
            state_style(theme, Tone::Busy),
        ));
    }
    spans
}

/// No projects: what devlanes is and how to start.
fn render_welcome(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let mut lines = vec![Line::from("")];
    if !app.loaded {
        lines.push(Line::styled(" loading…", theme.quiet()));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let mut add = |text: &str, style: Style| {
        for line in text::wrap(text, width) {
            lines.push(Line::styled(format!(" {line}"), style));
        }
    };
    add(
        "No projects yet.",
        Style::new().add_modifier(Modifier::BOLD),
    );
    add("", Style::new());
    add(
        "devlanes runs the services, tasks and containers a project lists in devlanes.yaml, one lane per git worktree.",
        Style::new(),
    );
    add("", Style::new());
    add(
        "  1  open a terminal in a repository that has a devlanes.yaml",
        Style::new(),
    );
    add("  2  run devlanes start", Style::new());
    add("", Style::new());
    add(
        "The project appears here as a tab while it starts. This window can stay open.",
        theme.quiet(),
    );
    add(
        "Writing a new devlanes.yaml? devlanes config check validates it before the first start.",
        theme.quiet(),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// The first-run strip above the footer.
fn render_strip(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let pairs = [
        ("j k", "move"),
        ("enter", "types into a service"),
        ("tab", "next project"),
        ("?", "all keys"),
        ("u", "hides this"),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in pairs {
        spans.push(Span::styled(key, theme.key()));
        spans.push(Span::styled(format!(" {label}  "), theme.quiet()));
    }
    frame.render_widget(Paragraph::new(fit(spans, usize::from(area.width))), area);
}

/// A footer hint: the key as shown, its label, and the key a click presses.
type Hint = (&'static str, &'static str, KeyCode);

/// The hints that apply to the selection and mode, most important first.
fn hints(app: &App) -> Vec<Hint> {
    let ch = KeyCode::Char;
    match app.view {
        View::Monitor => {
            let swap = if app.monitor.memory_first {
                ("c", "cpu graph", ch('c'))
            } else {
                ("m", "memory graph", ch('m'))
            };
            return vec![
                ("j k", "move", ch('j')),
                ("enter", "open unit", KeyCode::Enter),
                ("< >", "sort", ch('>')),
                ("i", "reverse", ch('i')),
                ("/", "filter", ch('/')),
                ("e", "flat", ch('e')),
                swap,
                ("s x r", "start stop restart", ch('r')),
                ("esc", "back", KeyCode::Esc),
            ];
        }
        View::Daemon if app.daemon.copy.is_some() => {
            return vec![
                ("h j k l", "move", KeyCode::Null),
                ("v", "mark", ch('v')),
                ("y", "copy", ch('y')),
                ("Y", "copy whole entries", ch('Y')),
                ("esc", "cancel", KeyCode::Esc),
            ];
        }
        View::Daemon if app.daemon.filter_editing => {
            return vec![
                ("type", "to filter", KeyCode::Null),
                ("enter", "keep", KeyCode::Enter),
                ("esc", "clear", KeyCode::Esc),
            ];
        }
        View::Daemon => {
            let follow = if app.daemon.paused {
                ("G", "follow", ch('G'))
            } else {
                ("F", "pause", ch('F'))
            };
            let fold = if app.daemon.filters.unfolded {
                ("d", "fold", ch('d'))
            } else {
                ("d", "unfold", ch('d'))
            };
            let mut hints = vec![
                ("j k", "scroll", ch('k')),
                follow,
                ("w", "level", ch('w')),
                ("/", "filter", ch('/')),
                ("T", "targets", ch('T')),
                ("c", "copy", ch('c')),
                ("i", "checks", ch('i')),
                fold,
                ("R", "restart", ch('R')),
                ("S", "stop", ch('S')),
            ];
            // Stopped, starting it is what matters: first, and S no longer stops.
            if app.daemon.stopped {
                hints.pop();
                hints.insert(0, ("S", "start daemon", ch('S')));
            }
            return hints;
        }
        View::Project => {}
    }
    if app.filter.as_ref().is_some_and(|f| f.editing) {
        return vec![
            ("type", "to filter", KeyCode::Null),
            ("enter", "keep", KeyCode::Enter),
            ("esc", "clear", KeyCode::Esc),
        ];
    }
    let mode = app.attached.as_ref().map(|a| &a.mode);
    match mode {
        Some(PaneMode::Copy(_)) => {
            return vec![
                ("h j k l", "move", KeyCode::Null),
                ("v", "mark", ch('v')),
                ("y", "copy", ch('y')),
                ("esc", "cancel", KeyCode::Esc),
            ];
        }
        Some(PaneMode::Logs(_)) => {
            return vec![
                ("pgup pgdn", "scroll", KeyCode::PageUp),
                ("esc", "back to live", KeyCode::Esc),
                ("c", "select text", ch('c')),
            ];
        }
        _ => {}
    }
    if app
        .attached
        .as_ref()
        .is_some_and(|a| a.screen.scroll_offset() > 0)
    {
        return vec![
            ("pgup pgdn", "scroll", KeyCode::PageUp),
            ("esc", "back to live", KeyCode::Esc),
            ("c", "select text", ch('c')),
        ];
    }
    let Some(lane) = app.selected_lane() else {
        return Vec::new();
    };
    let filter = ("/", "filter", ch('/'));
    let switch = ("[ ]", "switch lane", ch(']'));
    match app.selected_id() {
        Some(RowId::WholeLane) => {
            let missing = lane
                .reason
                .as_ref()
                .is_some_and(|r| r.code == "worktree_missing");
            if missing {
                return vec![("C", "clean lane", ch('C')), switch];
            }
            let live = tree::lane_is_live(lane);
            let mut hints = if live {
                vec![("x", "stop lane", ch('x')), ("r", "restart lane", ch('r'))]
            } else {
                vec![("s", "start lane", ch('s')), ("S", "start all", ch('S'))]
            };
            hints.extend([
                ("p", "start with profiles", ch('p')),
                ("C", "clean lane", ch('C')),
                switch,
                filter,
            ]);
            hints
        }
        Some(RowId::Section(_) | RowId::Group(..)) => vec![
            ("enter", "fold", KeyCode::Enter),
            ("s", "start all", ch('s')),
            ("x", "stop all", ch('x')),
            ("r", "restart all", ch('r')),
            ("p", "profiles", ch('p')),
            filter,
        ],
        Some(RowId::Container(_) | RowId::Gap(_)) => vec![filter, ("n", "next problem", ch('n'))],
        Some(RowId::Task(_)) => vec![("s", "run", ch('s')), filter],
        Some(RowId::Unit(_)) => {
            let Some(unit) = app.selected_unit() else {
                return vec![filter];
            };
            let task = tree::role(unit) != Role::Service && tree::role(unit) != Role::Exec;
            match unit.state.as_str() {
                ORPHANED => vec![("x", "end it", ch('x')), ("n", "next problem", ch('n'))],
                "failed" | "exited" if !task || unit.exit_code != Some(0) => vec![
                    ("r", if task { "run again" } else { "restart" }, ch('r')),
                    ("n", "next problem", ch('n')),
                    ("pgup", "scroll back", KeyCode::PageUp),
                    ("c", "select text", ch('c')),
                    filter,
                ],
                state if crate::app::is_active(state) && task => vec![
                    ("enter", "type into it", KeyCode::Enter),
                    ("x", "stop", ch('x')),
                    ("c", "select text", ch('c')),
                    ("z", "zoom", ch('z')),
                ],
                state if crate::app::is_active(state) => vec![
                    ("enter", "type into it", KeyCode::Enter),
                    ("x", "stop", ch('x')),
                    ("r", "restart", ch('r')),
                    ("o", "open in browser", ch('o')),
                    ("c", "select text", ch('c')),
                    ("z", "zoom", ch('z')),
                    filter,
                ],
                _ if task => vec![("s", "run", ch('s')), filter],
                _ => vec![("s", "start", ch('s')), filter],
            }
        }
        None => Vec::new(),
    }
}

/// The footer: hints for the selection, `? all keys  q quit` on the right.
/// While typing, one banner says how to get back.
fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let width = usize::from(area.width);
    if app.focus == Focus::Terminal && app.view == View::Project {
        let label = app
            .attached_unit()
            .map(|u| clean(u.label()))
            .or_else(|| app.attached.as_ref().map(|a| clean(&a.unit)))
            .unwrap_or_default();
        let back = if app.full_screen() {
            "ctrl+] returns to the list"
        } else {
            "esc or ctrl+] returns to the list"
        };
        let live = app
            .attached
            .as_ref()
            .is_some_and(|a| a.state == AttachState::Live);
        // ` TYPING into Gateway API · keys go to the program · esc or ctrl+] returns to the list`
        let spans = vec![
            Span::styled(" TYPING", theme.focus().add_modifier(Modifier::BOLD)),
            Span::raw(format!(" into {label}")),
            Span::styled(
                if live {
                    " · keys go to the program · "
                } else {
                    " · the program is not live · "
                },
                theme.quiet(),
            ),
            Span::styled(back, theme.key()),
        ];
        frame.render_widget(Paragraph::new(fit(spans, width)), area);
        return;
    }
    let fixed: [Hint; 2] = [
        ("?", "all keys", KeyCode::Char('?')),
        ("q", "quit", KeyCode::Char('q')),
    ];
    // `key label` and three spaces, as the mockups' footer.
    let size = |(key, label, _): &Hint| text::width(key) + text::width(label) + 4;
    let fixed_width: usize = fixed.iter().map(size).sum();
    let budget = width.saturating_sub(1 + fixed_width);
    let mut used = 0;
    let mut shown = Vec::new();
    for hint in hints(app) {
        if used + size(&hint) > budget {
            break;
        }
        used += size(&hint);
        shown.push(hint);
    }
    let mut left = vec![Span::raw(" ")];
    let mut x = area.x + 1;
    let record = |spans: &mut Vec<Span<'static>>, x: &mut u16, (key, label, code): Hint| {
        let w = (text::width(key) + 1 + text::width(label)) as u16;
        if code != KeyCode::Null {
            let modifiers = if matches!(code, KeyCode::Char(c) if c.is_ascii_uppercase()) {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            hit(
                app,
                Rect::new(*x, area.y, w, 1),
                Click::Key(code, modifiers),
            );
        }
        spans.push(Span::styled(key, theme.key()));
        spans.push(Span::styled(format!(" {label}   "), theme.quiet()));
        *x += w + 3;
    };
    for hint in shown {
        record(&mut left, &mut x, hint);
    }
    let mut right = Vec::new();
    let mut rx = area.right().saturating_sub(fixed_width as u16);
    for hint in fixed {
        record(&mut right, &mut rx, hint);
    }
    frame.render_widget(Paragraph::new(spread(left, right, width)), area);
}

/// The failed action on `target`, if any.
pub(crate) fn target_error<'a>(
    app: &'a App,
    lane: &str,
    unit: Option<&str>,
) -> Option<&'a crate::app::ErrorEntry> {
    let target = match unit {
        Some(unit) => Target::Unit {
            lane: lane.into(),
            unit: unit.into(),
        },
        None => Target::Lane(lane.into()),
    };
    app.target_error(&target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(index: u32, branch: &str, name: Option<&str>) -> LaneSummary {
        LaneSummary {
            reference: format!("acme@{index}"),
            project: "acme".into(),
            index: Some(index),
            branch: Some(branch.into()),
            name: name.map(str::to_owned),
            ..LaneSummary::default()
        }
    }

    #[test]
    fn lanes_go_by_branch_and_show_their_number_only_when_it_is_shared() {
        let main = lane(0, "main", None);
        let feat = lane(1, "feat/x", None);
        let again = lane(2, "main", None);
        // An extra lane of main's folder: same branch, its own name.
        let review = lane(3, "main", Some("review"));
        let all = [&main, &feat, &again, &review];
        assert_eq!(lane_label(&feat, &all), ("feat/x".into(), None));
        assert_eq!(lane_label(&main, &all), ("main".into(), Some("@0".into())));
        assert_eq!(lane_label(&again, &all), ("main".into(), Some("@2".into())));
        assert_eq!(lane_label(&review, &all), ("review".into(), None));
        // Alone, main needs no number.
        assert_eq!(
            lane_label(&main, &[&main, &feat, &review]),
            ("main".into(), None)
        );
    }
}
