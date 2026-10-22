//! The daemon page (`D`) as monitor.md draws it: a facts box and a checks
//! box (problems first, each with its fix), then `daemon.log` parsed into
//! entries with level colors, folded repeats, filters and follow.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{draw_box, fit};
use crate::app::App;
use crate::daemonlog::{self, Entry, Level, Shown};
use crate::doctor::{self, Check, CheckState};
use crate::text::{self, clean};
use crate::theme::Tone;

/// Fact labels: `version`, `socket`.
const LABEL: usize = 10;
/// The facts box's width beside the checks.
const FACTS_WIDTH: u16 = 46;
/// Time, level and target before the message: `10:18:08 INFO  daemon::lifecycle    `.
const PREFIX: usize = 36;
/// The same without the target, below 60 columns.
const NARROW_PREFIX: usize = 15;

/// The boxes, top to bottom.
pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let wide = area.width >= 100;
    let facts = fact_lines(app, wide);
    let checks_width = if wide {
        area.width.saturating_sub(FACTS_WIDTH)
    } else {
        area.width
    };
    let check_rows = check_lines(app, usize::from(checks_width.saturating_sub(2)), usize::MAX).0;
    let content = if wide {
        facts.len().max(check_rows.len())
    } else {
        facts.len() + check_rows.len()
    };
    // The boxes take what they need, up to 40% of the page; on short pages
    // up to what leaves the log ten rows. The log gets the rest.
    let height = usize::from(area.height);
    let cap = if height < 30 {
        (height * 2 / 5).max(height.saturating_sub(12))
    } else {
        height * 2 / 5
    }
    .max(5);
    let top = u16::try_from((content + 2).min(cap))
        .unwrap_or(5)
        .min(area.height);
    let top_area = Rect::new(area.x, area.y, area.width, top);
    let room = usize::from(top.saturating_sub(2));
    if wide {
        let facts_area = Rect::new(area.x, area.y, FACTS_WIDTH.min(area.width), top);
        draw_box(app, frame, facts_area, title(" daemon "), Vec::new(), false);
        frame.render_widget(Paragraph::new(facts), inside(facts_area));
        let checks_area = Rect::new(facts_area.right(), area.y, checks_width, top);
        let (lines, right) = check_lines(app, usize::from(checks_width.saturating_sub(2)), room);
        draw_box(app, frame, checks_area, title(" checks "), right, false);
        frame.render_widget(Paragraph::new(lines), inside(checks_area));
    } else {
        // Facts as two lines above the checks, in one box.
        let (lines, right) = check_lines(
            app,
            usize::from(area.width.saturating_sub(2)),
            room.saturating_sub(facts.len()),
        );
        let mut all = facts;
        all.extend(lines);
        draw_box(app, frame, top_area, title(" daemon "), right, false);
        frame.render_widget(Paragraph::new(all), inside(top_area));
    }
    let log = Rect::new(area.x, area.y + top, area.width, area.height - top);
    if log.height >= 3 {
        render_log(app, frame, log);
    }
}

fn title(text: &str) -> Vec<Span<'static>> {
    vec![Span::styled(
        text.to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    )]
}

fn inside(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// `/Users/acme/.devlanes` as `~/.devlanes` when `/Users/acme` is the
/// user's home (`App::user_home`); see [`super::home_short`].
fn home_short(app: &App, path: &str) -> String {
    super::home_short(app.user_home.as_deref(), path)
}

/// The facts: version and pid, uptime, socket, managed processes, clients,
/// state. Below 100 columns, two lines.
fn fact_lines(app: &App, wide: bool) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let view = &app.daemon;
    let width = if wide {
        usize::from(FACTS_WIDTH - 2)
    } else {
        usize::from(app.size.0.saturating_sub(2))
    };
    let line = |label: &str, value: String, style: Style| {
        fit(
            vec![
                Span::styled(format!("{label:<LABEL$}"), theme.quiet()),
                Span::styled(clean(&value), style),
            ],
            width,
        )
    };
    let mut lines = Vec::new();
    if let Some(busy) = view.busy {
        lines.push(line("daemon", format!("{busy}…"), theme.tone(Tone::Busy)));
    }
    if view.stopped {
        lines.push(line(
            "daemon",
            "Stopped. S starts it.".into(),
            theme.quiet(),
        ));
        return lines;
    }
    let status = view.status.as_ref();
    let doctor = view.doctor.as_ref();
    if status.is_none() && doctor.is_none() {
        lines.push(Line::styled("loading…", theme.quiet()));
        return lines;
    }
    let source = status.or(doctor).cloned().unwrap_or_default();
    let version = format!(
        "{} · pid {}",
        doctor::fact(&source, "version"),
        doctor::fact(&source, "pid")
    );
    let uptime = status
        .and_then(|s| s.get("uptime_ms"))
        .and_then(serde_json::Value::as_u64)
        .map(doctor::uptime);
    let socket = home_short(app, &doctor::fact(&source, "socket"));
    let mut managed = Vec::new();
    if let Some(processes) = view.managed_processes() {
        managed.push(format!("{processes} processes"));
    }
    // Counts the daemon did not send are left out, not shown as unknown.
    let count = |key: &str, one: &str, many: &str| match doctor?
        .get(key)
        .and_then(serde_json::Value::as_u64)?
    {
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    managed.extend(count("projects", "project", "projects"));
    let clients = count("subscribers", "event subscriber", "event subscribers");
    let plain = Style::new();
    if !wide {
        let mut first = version;
        if let Some(uptime) = &uptime {
            first.push_str(&format!(" · up {uptime}"));
        }
        if let Some(processes) = view.managed_processes() {
            first.push_str(&format!(" · {processes} managed"));
        }
        lines.push(line("daemon", first, plain));
        lines.push(line("socket", socket, plain));
        return lines;
    }
    lines.push(line("version", version, plain));
    if let Some(uptime) = uptime {
        lines.push(line("uptime", uptime, plain));
    }
    lines.push(line("socket", socket, plain));
    if !managed.is_empty() {
        lines.push(line("managed", managed.join(" · "), plain));
    }
    if let Some(clients) = clients {
        lines.push(line("clients", clients, plain));
    }
    if let Some(doctor) = doctor
        && doctor.get("schema_version").is_some()
    {
        let state = format!(
            "schema {} · {}",
            doctor::fact(doctor, "schema_version"),
            home_short(app, &doctor::fact(doctor, "db_path"))
        );
        lines.push(line("state", state, plain));
    }
    lines
}

/// The checks in `room` rows of `width`: failures, then warnings, each with
/// its fix, then one `ok` line naming the passing groups (`i` lists them
/// one per line). Returns the rows and the box title's right side.
fn check_lines(app: &App, width: usize, room: usize) -> (Vec<Line<'static>>, Vec<Span<'static>>) {
    let theme = &app.theme;
    let Some(doctor) = app.daemon.doctor.as_ref() else {
        return (Vec::new(), Vec::new());
    };
    let mut checks = doctor::checks(doctor);
    // A newer release reads like a warning with its upgrade command as the fix.
    if let Some((_, text)) = app.release_badge() {
        let (text, command) = text.split_once(": ").unwrap_or((text.as_str(), ""));
        checks.push(Check {
            group: "update",
            state: CheckState::Warn,
            text: format!("{text} · u hides it"),
            fix: (!command.is_empty()).then(|| command.to_owned()),
        });
    }
    checks.sort_by_key(|check| match check.state {
        CheckState::Fail => 0,
        CheckState::Warn => 1,
        CheckState::Ok => 2,
    });
    let fails = checks
        .iter()
        .filter(|c| c.state == CheckState::Fail)
        .count();
    // The header badge counts an update apart from issues; so does the title.
    let update = checks.iter().any(|c| c.group == "update");
    let warns = checks
        .iter()
        .filter(|c| c.state == CheckState::Warn && c.group != "update")
        .count();
    let label = checks
        .iter()
        .map(|c| text::width(c.group))
        .max()
        .unwrap_or(0)
        + 6;
    let row = |check: &Check, text: &str, first: bool| -> Vec<Line<'static>> {
        let tone = match check.state {
            CheckState::Ok => Tone::Ok,
            CheckState::Warn => Tone::Attention,
            CheckState::Fail => Tone::Failure,
        };
        let head = if first {
            format!("{:<4} {}", check.state.word(), check.group)
        } else {
            String::new()
        };
        let value_width = width.saturating_sub(label).max(1);
        text::wrap(&clean(text), value_width)
            .into_iter()
            .enumerate()
            .map(|(index, part)| {
                let head = if index == 0 { head.as_str() } else { "" };
                fit(
                    vec![
                        Span::styled(format!("{head:<label$}"), theme.tone(tone)),
                        Span::styled(part, if first { Style::new() } else { theme.quiet() }),
                    ],
                    width,
                )
            })
            .collect()
    };
    // Each check as a block of rows, so a cut never splits one.
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    let mut passing = Vec::new();
    for check in &checks {
        if check.state == CheckState::Ok && !app.daemon.all_checks {
            if !passing.contains(&check.group) {
                passing.push(check.group);
            }
            continue;
        }
        let mut block = row(check, &check.text, true);
        if let Some(fix) = &check.fix {
            block.extend(row(check, &format!("fix: {fix}"), false));
        }
        blocks.push(block);
    }
    if !passing.is_empty() {
        let ok = Check {
            group: "",
            state: CheckState::Ok,
            text: String::new(),
            fix: None,
        };
        blocks.push(row(&ok, &passing.join(" · "), true));
    }
    let mut lines = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let left = blocks.len() - index;
        // Leave a row for `… n more` unless this is the last block.
        let reserve = usize::from(left > 1);
        if lines.len() + block.len() + reserve > room {
            lines.push(Line::styled(
                format!("… {left} more · i shows all"),
                theme.quiet(),
            ));
            break;
        }
        lines.extend(block.iter().cloned());
    }
    let mut parts = Vec::new();
    if fails > 0 {
        parts.push(Span::styled(
            format!("{fails} fail"),
            theme.tone(Tone::Failure),
        ));
    }
    if warns > 0 {
        parts.push(Span::styled(
            format!("{warns} warn"),
            theme.tone(Tone::Attention),
        ));
    }
    if update {
        parts.push(Span::styled("update", theme.tone(Tone::Attention)));
    }
    if parts.is_empty() {
        parts.push(Span::styled("all ok", theme.tone(Tone::Ok)));
    }
    let toggle = if app.daemon.all_checks {
        "i problems"
    } else {
        "i all"
    };
    let mut right = vec![Span::raw(" ")];
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            right.push(Span::styled(" · ", theme.quiet()));
        }
        right.push(part);
    }
    right.push(Span::styled(format!(" · {toggle} "), theme.quiet()));
    (lines, right)
}

/// The palette color of the unit `lane:unit` names, when the TUI knows it.
fn unit_style(app: &App, reference: &str) -> Style {
    let Some((lane, unit)) = reference.rsplit_once(':') else {
        return Style::new();
    };
    app.lanes
        .iter()
        .find(|l| l.reference == lane)
        .and_then(|l| l.units.iter().find(|u| u.name == unit))
        .and_then(|u| u.color.as_deref())
        .and_then(|color| app.theme.identity(color))
        .map_or_else(Style::new, |color| Style::new().fg(color))
}

/// `spans` with the `/` filter's words marked wherever they occur, like a
/// search hit.
fn mark(spans: Vec<Span<'static>>, filter: &str, hit: Style) -> Vec<Span<'static>> {
    if filter.trim().is_empty() {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let ranges = daemonlog::matches_in(&span.content, filter);
        if ranges.is_empty() {
            out.push(span);
            continue;
        }
        let text = span.content.to_string();
        let mut at = 0;
        for range in ranges {
            if range.start > at {
                out.push(Span::styled(text[at..range.start].to_owned(), span.style));
            }
            out.push(Span::styled(
                text[range.clone()].to_owned(),
                span.style.patch(hit),
            ));
            at = range.end;
        }
        if at < text.len() {
            out.push(Span::styled(text[at..].to_owned(), span.style));
        }
    }
    out
}

/// One shown entry as rows of `width`: time, level and target, then the
/// message and fields wrapped under the message column, the repeat count
/// right-aligned on the first row, continuation lines below.
fn entry_rows(app: &App, entry: &Entry, shown: &Shown, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let quiet_line = matches!(entry.level, Level::Debug | Level::Trace);
    let base = if quiet_line {
        theme.quiet()
    } else {
        Style::new()
    };
    let level_style = match entry.level {
        Level::Error => theme.tone(Tone::Failure).add_modifier(Modifier::BOLD),
        Level::Warn => theme.tone(Tone::Attention),
        Level::Info => Style::new(),
        Level::Debug | Level::Trace => theme.quiet(),
    };
    let narrow = width < 60;
    let prefix = if narrow { NARROW_PREFIX } else { PREFIX };
    let time = entry
        .time
        .as_deref()
        .map(|t| daemonlog::local(t, &app.time_zone).1)
        .unwrap_or_default();
    let mut head = vec![
        Span::styled(format!("{:<8} ", clip(&time, 8)), theme.quiet()),
        Span::styled(entry.level.tag().to_owned(), level_style),
        Span::raw(" "),
    ];
    // `/` matches show like search hits in the target, message and fields.
    let filter = app.daemon.filters.text.as_str();
    let hit = theme
        .tone(Tone::Attention)
        .add_modifier(Modifier::REVERSED | Modifier::BOLD);
    if !narrow {
        head.extend(mark(
            vec![Span::styled(
                format!("{:<20} ", daemonlog::clip_left(&entry.target, 20)),
                base,
            )],
            filter,
            hit,
        ));
    }
    // Words joined by spaces, each a group of styled spans: a field's key
    // and the first word of its value stay together.
    let mut tokens: Vec<Vec<Span<'static>>> = entry
        .message
        .split_whitespace()
        .map(|word| vec![Span::styled(word.to_owned(), base)])
        .collect();
    let failing = matches!(entry.level, Level::Error | Level::Warn);
    for (index, (name, value)) in entry.fields.iter().enumerate() {
        let value_style = match name.as_str() {
            "unit" => unit_style(app, value),
            "error" if failing => theme.tone(Tone::Failure),
            _ => base,
        };
        // Two spaces between the message and its fields.
        let gap = if index == 0 && !tokens.is_empty() {
            " "
        } else {
            ""
        };
        let value = clean(value);
        let mut words = value.split_whitespace();
        tokens.push(vec![
            Span::styled(format!("{gap}{name}="), theme.quiet()),
            Span::styled(words.next().unwrap_or_default().to_owned(), value_style),
        ]);
        tokens.extend(words.map(|word| vec![Span::styled(word.to_owned(), value_style)]));
    }
    let tokens: Vec<Vec<Span<'static>>> = tokens
        .into_iter()
        .map(|token| mark(token, filter, hit))
        .collect();
    let tail = (shown.count > 1).then(|| {
        let since = shown
            .since
            .as_deref()
            .map(|t| daemonlog::local(t, &app.time_zone).1)
            .unwrap_or_default();
        format!("×{} since {}", shown.count, clip(&since, 5))
    });
    let message_width = width.saturating_sub(prefix).max(8);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for token in tokens {
        for (piece_index, piece) in split_long(token, message_width).into_iter().enumerate() {
            let w = super::spans_width(&piece);
            // Pieces of one long word follow each other without a space.
            let space = usize::from(used > 0 && piece_index == 0);
            let limit = if rows.len() == 1 {
                message_width.saturating_sub(tail.as_ref().map_or(0, |t| text::width(t) + 2))
            } else {
                message_width
            };
            let mut piece = piece;
            if used > 0 && used + space + w > limit {
                rows.push(Vec::new());
                used = 0;
                // A field's two-space gap is only for after the message.
                if let Some(first) = piece.first_mut()
                    && first.content.starts_with(' ')
                {
                    first.content = first.content.trim_start().to_owned().into();
                }
            } else if space == 1
                && let Some(row) = rows.last_mut()
            {
                row.push(Span::raw(" "));
                used += 1;
            }
            used += super::spans_width(&piece);
            if let Some(row) = rows.last_mut() {
                row.extend(piece);
            }
        }
    }
    let mut lines = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let mut spans = if index == 0 {
            head.clone()
        } else {
            vec![Span::raw(" ".repeat(prefix))]
        };
        spans.extend(row);
        if index == 0
            && let Some(tail) = &tail
        {
            let used = super::spans_width(&spans);
            let pad = width.saturating_sub(used + text::width(tail)).max(1);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(tail.clone(), theme.quiet()));
        }
        lines.push(fit(spans, width));
    }
    for more in &entry.more {
        for part in text::wrap(&clean(more), message_width) {
            let mut spans = vec![Span::raw(" ".repeat(prefix))];
            spans.extend(mark(vec![Span::styled(part, base)], filter, hit));
            lines.push(fit(spans, width));
        }
    }
    lines
}

/// A word wider than `width` cut into pieces of at most `width` columns,
/// each keeping the styles of its characters.
fn split_long(token: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    if super::spans_width(&token) <= width {
        return vec![token];
    }
    let mut pieces: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for span in token {
        let mut current = String::new();
        for c in span.content.chars() {
            let w = text::width(c.encode_utf8(&mut [0; 4]));
            if used + w > width && used > 0 {
                if !current.is_empty()
                    && let Some(piece) = pieces.last_mut()
                {
                    piece.push(Span::styled(std::mem::take(&mut current), span.style));
                }
                pieces.push(Vec::new());
                used = 0;
            }
            current.push(c);
            used += w;
        }
        if !current.is_empty()
            && let Some(piece) = pieces.last_mut()
        {
            piece.push(Span::styled(current, span.style));
        }
    }
    pieces
}

/// `text` cut to `width` characters.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// `daemon.log`: the shown entries ending `scroll` entries up from the
/// newest, with day separators; the title says what is shown.
fn render_log(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let view = &app.daemon;
    let (entries, shown) = app.daemon_shown();
    let inner = inside(area);
    let width = usize::from(inner.width).max(1);
    let height = usize::from(inner.height);

    // The title's parts with how much they matter (0 most); the least
    // important go first when the title does not fit.
    let mut state: Vec<(u8, String)> = Vec::new();
    if view.copy.is_some() {
        state.push((0, "selecting · v mark · y copy · Y as shown".into()));
    } else if view.paused {
        state.push((0, format!("paused · {} new · G follows", view.new_lines)));
    } else {
        state.push((0, "following".into()));
    }
    state.push((2, view.filters.floor.word().into()));
    let folding = if view.filters.unfolded {
        "repeats shown"
    } else {
        "repeats folded"
    };
    state.push((3, folding.into()));
    if view.filter_editing || !view.filters.text.is_empty() {
        let cursor = if view.filter_editing { "▏" } else { "" };
        state.push((
            1,
            format!(
                "/{}{cursor} {} of {}",
                clean(&view.filters.text),
                shown.len(),
                entries.len()
            ),
        ));
    }
    match view.filters.targets.len() {
        0 => {}
        1 => state.push((1, "1 target".into())),
        n => state.push((1, format!("{n} targets"))),
    }
    state.push((4, format!("{} lines", view.log.len())));
    let joined = |state: &[(u8, String)]| {
        state
            .iter()
            .map(|(_, part)| part.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    // ` daemon.log `, the corners and the rule between take 18 columns.
    let room = usize::from(area.width).saturating_sub(18);
    while text::width(&joined(&state)) > room && state.len() > 1 {
        let least = state
            .iter()
            .enumerate()
            .max_by_key(|(_, (rank, _))| *rank)
            .map_or(0, |(index, _)| index);
        state.remove(least);
    }
    let right = vec![Span::styled(format!(" {} ", joined(&state)), theme.quiet())];
    draw_box(app, frame, area, title(" daemon.log "), right, false);

    if let Some(error) = &view.log_error {
        let lines: Vec<Line> = text::wrap(&format!("Can't read daemon.log: {error}"), width)
            .into_iter()
            .map(|line| Line::styled(line, theme.tone(Tone::Failure)))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if shown.is_empty() {
        let text = if view.log.is_empty() {
            "Empty so far."
        } else {
            "Nothing matches the filters. w, / and d change them."
        };
        frame.render_widget(Paragraph::new(text).style(theme.quiet()), inner);
        return;
    }
    let end = shown.len().saturating_sub(view.scroll).max(1);
    let selected = view.copy.map(|copy| copy.range());
    let date = |row: &Shown| {
        entries[row.entry]
            .time
            .as_deref()
            .map(|t| daemonlog::local(t, &app.time_zone).0)
            .unwrap_or_default()
    };
    // From the newest shown entry up, until the box is full.
    let mut rows: Vec<Line> = Vec::new();
    for index in (0..end).rev() {
        let row = &shown[index];
        let mut lines = entry_rows(app, &entries[row.entry], row, width);
        if selected.is_some_and(|(first, last)| (first..=last).contains(&index)) {
            lines = lines
                .into_iter()
                .map(|line| {
                    let pad = width.saturating_sub(line.width());
                    let mut line = line.patch_style(theme.selection());
                    line.spans
                        .push(Span::styled(" ".repeat(pad), theme.selection()));
                    line
                })
                .collect();
        }
        // A separator where the day changes from the entry before.
        let day = date(row);
        let previous = index.checked_sub(1).map(|i| date(&shown[i]));
        if !day.is_empty() && previous.as_deref() != Some(day.as_str()) {
            let label = format!("── {day} ");
            let rule = "─".repeat(width.saturating_sub(text::width(&label)));
            lines.insert(0, Line::styled(format!("{label}{rule}"), theme.rule()));
        }
        lines.extend(rows);
        rows = lines;
        if rows.len() >= height {
            break;
        }
    }
    let start = rows.len().saturating_sub(height);
    frame.render_widget(Paragraph::new(rows.split_off(start)), inner);
}
