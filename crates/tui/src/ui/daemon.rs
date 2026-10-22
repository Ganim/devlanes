//! The daemon view (`D`): facts and checks, then the daemon's log.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{fit, spread};
use crate::app::App;
use crate::doctor;
use crate::text::{self, clean};
use crate::theme::{Glyph, Tone};

const LABEL: usize = 10;
const CHECK_LABEL: usize = 13;

/// Facts and checks on top, the log below.
pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let lines = daemon_lines(app, width);
    let wanted = u16::try_from(lines.len() + 1).unwrap_or(u16::MAX);
    let top = if area.height < 12 {
        (area.height / 2).max(3).min(area.height)
    } else {
        wanted.min(area.height - 7)
    };
    let [info_area, log_area] =
        Layout::vertical([Constraint::Length(top), Constraint::Min(0)]).areas(area);
    let [header, info] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(info_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(app.theme.glyph(Glyph::Marker), app.theme.focus()),
            Span::styled("daemon", Style::new().add_modifier(Modifier::BOLD)),
        ])),
        header,
    );
    let info = Rect::new(
        info.x + 1,
        info.y,
        info.width.saturating_sub(1),
        info.height,
    );
    let mut lines = lines;
    let height = usize::from(info.height);
    if height > 0 && lines.len() > height {
        lines.truncate(height - 1);
        lines.push(Line::styled(
            "… more: devlanes doctor lists every check",
            app.theme.quiet(),
        ));
    }
    frame.render_widget(Paragraph::new(lines), info);
    if log_area.height >= 3 {
        render_log(app, frame, log_area);
    }
}

fn field(app: &App, label: &str, value: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    labeled(app, label, LABEL, value, width, style)
}

fn labeled(
    app: &App,
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
                    Span::styled(format!("{label:<column$}"), app.theme.quiet()),
                    Span::styled(line, style),
                ],
                width,
            )
        })
        .collect()
}

/// Facts and checks, wrapped to `width`.
fn daemon_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let view = &app.daemon;
    let plain = Style::new();
    let mut lines = Vec::new();
    if let Some(busy) = view.busy {
        lines.extend(field(
            app,
            "daemon",
            &format!("{busy}…"),
            width,
            theme.tone(Tone::Busy),
        ));
    }
    if view.stopped {
        lines.extend(field(
            app,
            "daemon",
            "Stopped. S starts it.",
            width,
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
    let mut version = format!(
        "{}  pid {}",
        doctor::fact(&source, "version"),
        doctor::fact(&source, "pid")
    );
    if let Some(ms) = status
        .and_then(|s| s.get("uptime_ms"))
        .and_then(serde_json::Value::as_u64)
    {
        version.push_str(&format!("  up {}", doctor::uptime(ms)));
    }
    lines.extend(field(app, "version", &version, width, plain));
    if let Some((_, text)) = app.release_badge() {
        lines.extend(field(
            app,
            "update",
            &format!("{text}  u hides"),
            width,
            theme.tone(Tone::Attention),
        ));
    }
    lines.extend(field(
        app,
        "socket",
        &doctor::fact(&source, "socket"),
        width,
        plain,
    ));
    let mut counts = Vec::new();
    if let Some(processes) = view.managed_processes() {
        counts.push(format!("{processes} processes"));
    }
    if let Some(doctor) = doctor {
        let count =
            |key: &str, one: &str, many: &str| match doctor.get(key).and_then(|v| v.as_u64()) {
                Some(1) => format!("1 {one}"),
                _ => format!("{} {many}", doctor::fact(doctor, key)),
            };
        counts.push(count("projects", "project", "projects"));
        counts.push(count("subscribers", "client", "clients"));
    }
    if !counts.is_empty() {
        lines.extend(field(app, "running", &counts.join("  "), width, plain));
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
            doctor::CheckState::Ok => theme.tone(Tone::Ok),
            doctor::CheckState::Warn => theme.tone(Tone::Attention),
            doctor::CheckState::Fail => theme.tone(Tone::Failure),
        };
        let label = format!("{:<4} {}", check.state.word(), check.group);
        let mut first = labeled(app, &label, CHECK_LABEL, &check.text, width, plain);
        if let Some(line) = first.first_mut()
            && let Some(span) = line.spans.first_mut()
        {
            *span = Span::styled(span.content.clone(), style);
        }
        lines.extend(first);
        if let Some(fix) = &check.fix {
            lines.extend(labeled(
                app,
                "",
                CHECK_LABEL,
                &format!("fix  {fix}"),
                width,
                theme.quiet(),
            ));
        }
    }
    lines
}

/// The tail of the daemon's log, wrapped; follows the file unless scrolled up.
fn render_log(app: &App, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let view = &app.daemon;
    let state = if view.copy.is_some() {
        "selecting  v mark  y copy  esc cancel".to_owned()
    } else if view.scroll == 0 {
        "live".to_owned()
    } else {
        format!("{} lines up  G jumps to newest", view.scroll)
    };
    let [header, inner] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(spread(
            vec![
                Span::raw(" "),
                Span::styled("daemon.log", Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {state}"), theme.quiet()),
            ],
            Vec::new(),
            usize::from(area.width),
        )),
        header,
    );
    let inner = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );
    let width = usize::from(inner.width).max(1);
    let height = usize::from(inner.height);
    if let Some(error) = &view.log_error {
        let lines: Vec<Line> = text::wrap(&format!("Can't read daemon.log: {error}"), width)
            .into_iter()
            .map(|line| Line::styled(line, theme.tone(Tone::Failure)))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if view.log.is_empty() {
        frame.render_widget(Paragraph::new("Empty so far.").style(theme.quiet()), inner);
        return;
    }
    let end = view.log.len().saturating_sub(view.scroll);
    let mut rows: Vec<(usize, String)> = Vec::new();
    for (index, line) in view.log.range(..end).enumerate().rev() {
        let mut wrapped: Vec<(usize, String)> = text::wrap(&clean(line), width)
            .into_iter()
            .map(|row| (index, row))
            .collect();
        wrapped.extend(rows);
        rows = wrapped;
        if rows.len() >= height {
            break;
        }
    }
    let start = rows.len().saturating_sub(height);
    let selected = view.copy.map(|copy| copy.range());
    let lines: Vec<Line> = rows
        .into_iter()
        .skip(start)
        .map(|(index, row)| match selected {
            Some((first, last)) if (first..=last).contains(&index) => {
                let pad = width.saturating_sub(text::width(&row));
                Line::styled(format!("{row}{}", " ".repeat(pad)), theme.selection())
            }
            _ => Line::from(row),
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
