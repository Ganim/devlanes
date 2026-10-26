//! What the shell shows once the TUI closed: the devlanes banner, the lane
//! that was on screen and whether the daemon still runs, with the command
//! that stops the lane while it runs.

use std::{
    fmt::Write as _,
    io::{self, Write as _},
    time::Duration,
};

use crossterm::{
    Command,
    style::{Attribute, SetAttribute, SetForegroundColor},
};
use devlanes_proto::LaneSummary;
use ratatui::{
    backend::IntoCrossterm,
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    api::{ApiError, DaemonApi},
    banner,
    theme::{Theme, Tone},
    tree,
};

/// How long the daemon gets to answer `status` before the summary says it
/// did not.
const STATUS_TIMEOUT: Duration = Duration::from_secs(1);
/// Columns before the banner and each line.
const INDENT: u16 = 2;
/// The banner's tallest font is too much for a shell: the medium and small
/// ones fit in this many rows.
const BANNER_ROWS: u16 = 5;
/// Columns of a line's label, the gap after it included.
const LABEL: usize = 10;

/// The daemon as it answered after the TUI closed.
#[derive(Debug, Clone)]
pub enum Daemon {
    /// It answered with every lane it knows.
    Running(Vec<LaneSummary>),
    /// Nothing listens on its socket any more.
    Stopped,
    /// It is there but `status` failed or took too long.
    Unanswered,
}

impl Daemon {
    /// Asks `api` for every lane, within [`STATUS_TIMEOUT`].
    pub async fn ask(api: &dyn DaemonApi) -> Self {
        match tokio::time::timeout(STATUS_TIMEOUT, api.status()).await {
            Ok(Ok(lanes)) => Self::Running(lanes),
            Ok(Err(ApiError::Disconnected(_))) => Self::Stopped,
            Ok(Err(_)) | Err(_) => Self::Unanswered,
        }
    }
}

/// Asks the daemon how things stand and prints the summary on stdout, for
/// `lane`, the lane the TUI showed last. Runs after the terminal left TUI
/// mode; a failed write is dropped, as the TUI already closed.
pub async fn print(api: &dyn DaemonApi, theme: &Theme, lane: Option<&str>) {
    let daemon = Daemon::ask(api).await;
    let width = crossterm::terminal::size().map_or(80, |(width, _)| width);
    let text = render(theme, width, lane, &daemon);
    let mut stdout = io::stdout();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}

/// The summary for a terminal `width` columns wide, with SGR sequences for
/// its colors and a blank line around it.
pub fn render(theme: &Theme, width: u16, lane: Option<&str>, daemon: &Daemon) -> String {
    let size = banner::Size::pick(theme, width.saturating_sub(INDENT), BANNER_ROWS);
    let (banner_width, banner_rows) = size.dimensions();
    let lines = lines(theme, lane, daemon);
    let text_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let content = u16::try_from(text_width)
        .unwrap_or(u16::MAX)
        .max(banner_width);
    let height = banner_rows + 1 + u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, INDENT.saturating_add(content), height);
    let mut buf = Buffer::empty(area);
    banner::render(
        &mut buf,
        Rect::new(INDENT, 0, banner_width, banner_rows),
        theme,
        size,
    );
    for (y, line) in (banner_rows + 1..).zip(&lines) {
        buf.set_line(INDENT, y, line, content);
    }
    format!("\n{}\n", ansi(&buf))
}

/// The lane, daemon and stop lines; the lane's state only when the daemon
/// still knows it.
fn lines(theme: &Theme, lane: Option<&str>, daemon: &Daemon) -> Vec<Line<'static>> {
    let label = |text: &str| Span::styled(format!("{text:<LABEL$}"), theme.quiet());
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let lanes = match daemon {
        Daemon::Running(lanes) => lanes.as_slice(),
        Daemon::Stopped | Daemon::Unanswered => &[],
    };
    let known = lane.and_then(|reference| lanes.iter().find(|l| l.reference == reference));
    let mut out = Vec::new();
    if let Some(reference) = lane {
        let mut spans = vec![label("Lane"), Span::styled(reference.to_owned(), bold)];
        if let Some(known) = known {
            let (state, tone) = lane_state(known);
            spans.push(Span::raw("  "));
            spans.push(Span::styled(state, theme.tone(tone)));
        }
        out.push(Line::from(spans));
    }
    let mut spans = vec![label("Daemon")];
    match daemon {
        Daemon::Running(lanes) => {
            let live = lanes.iter().filter(|l| tree::lane_is_live(l)).count();
            let running = match live {
                0 => ", no lanes running".to_owned(),
                1 => ", 1 lane running".to_owned(),
                n => format!(", {n} lanes running"),
            };
            spans.push(Span::styled("running", theme.tone(Tone::Ok)));
            spans.push(Span::styled(running, theme.quiet()));
        }
        Daemon::Stopped => spans.push(Span::styled("not running", theme.quiet())),
        Daemon::Unanswered => {
            spans.push(Span::styled("not answering", theme.tone(Tone::Attention)));
        }
    }
    out.push(Line::from(spans));
    if let Some(known) = known.filter(|l| tree::lane_is_live(l)) {
        out.push(Line::from(vec![
            label("Stop"),
            Span::styled(format!("devlanes stop --lane {}", known.reference), bold),
        ]));
    }
    out
}

/// The lane's state word, with how many of its services and containers are
/// up while it runs, and its tone.
fn lane_state(lane: &LaneSummary) -> (String, Tone) {
    let counts = tree::counts(lane);
    let tone = match lane.state.as_str() {
        "running" => Tone::Ok,
        "failed" => Tone::Failure,
        "starting" | "stopping" => Tone::Busy,
        _ => Tone::Quiet,
    };
    let mut text = lane.state.clone();
    if tree::lane_is_live(lane) && counts.total > 0 {
        let _ = write!(text, ", {}/{} up", counts.up, counts.total);
    }
    if counts.failed > 0 {
        let _ = write!(text, ", {} failed", counts.failed);
    }
    (text, tone)
}

/// `buf` as text, one line per row with trailing blanks cut, its colors and
/// bold or dim as SGR sequences.
fn ansi(buf: &Buffer) -> String {
    let plain = Cell::EMPTY.style();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let row: Vec<&Cell> = (0..buf.area.width).map(|x| &buf[(x, y)]).collect();
        let end = row
            .iter()
            .rposition(|cell| cell.symbol() != " ")
            .map_or(0, |last| last + 1);
        let mut current = plain;
        // A wide character covers the cells after it, which hold a blank.
        let mut covered = 0;
        for cell in &row[..end] {
            if covered > 0 {
                covered -= 1;
                continue;
            }
            let style = cell.style();
            if style != current {
                sgr(&mut out, style);
                current = style;
            }
            out.push_str(cell.symbol());
            covered = cell.symbol().width().saturating_sub(1);
        }
        if current != plain {
            let _ = SetAttribute(Attribute::Reset).write_ansi(&mut out);
        }
        out.push('\n');
    }
    out
}

/// Writes the sequences that switch to `style` from whatever came before:
/// a reset, then its color and modifiers. The summary uses only these.
fn sgr(out: &mut String, style: Style) {
    let _ = SetAttribute(Attribute::Reset).write_ansi(out);
    if let Some(color) = style.fg.filter(|color| *color != Color::Reset) {
        let _ = SetForegroundColor(color.into_crossterm()).write_ansi(out);
    }
    for (modifier, attribute) in [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
    ] {
        if style.add_modifier.contains(modifier) {
            let _ = SetAttribute(attribute).write_ansi(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Glyphs, Tier};

    fn theme(tier: Tier, glyphs: Glyphs) -> Theme {
        Theme { tier, glyphs }
    }

    fn lane(reference: &str, state: &str) -> LaneSummary {
        LaneSummary {
            reference: reference.into(),
            project: "acme".into(),
            index: Some(0),
            state: state.into(),
            ..LaneSummary::default()
        }
    }

    /// The summary without its SGR sequences, trailing blank lines cut.
    fn text(theme: &Theme, width: u16, lane: Option<&str>, daemon: &Daemon) -> Vec<String> {
        let rendered = render(theme, width, lane, daemon);
        let plain = strip_ansi_escapes::strip_str(&rendered);
        plain.trim_end().lines().map(str::to_owned).collect()
    }

    #[test]
    fn a_running_lane_gets_its_state_and_the_stop_command() {
        let daemon = Daemon::Running(vec![lane("acme@0", "running"), lane("acme@1", "idle")]);
        let rows = text(
            &theme(Tier::Ansi, Glyphs::Unicode),
            42,
            Some("acme@0"),
            &daemon,
        );
        assert_eq!(
            rows,
            [
                "",
                "  ▀▀▀▀▀   ░█▀▄░█▀▀░█░█░█░░░█▀█░█▀█░█▀▀░█▀▀",
                "   ▀▀▀▀▀  ░█░█░█▀▀░▀▄▀░█░░░█▀█░█░█░█▀▀░▀▀█",
                "    ▀▀▀▀▀ ░▀▀░░▀▀▀░░▀░░▀▀▀░▀░▀░▀░▀░▀▀▀░▀▀▀",
                "",
                "  Lane      acme@0  running",
                "  Daemon    running, 1 lane running",
                "  Stop      devlanes stop --lane acme@0",
            ]
        );
    }

    #[test]
    fn a_stopped_daemon_leaves_the_lane_without_state() {
        let rows = text(
            &theme(Tier::Plain, Glyphs::Ascii),
            80,
            Some("acme@0"),
            &Daemon::Stopped,
        );
        assert_eq!(
            rows,
            [
                "",
                "  devlanes",
                "",
                "  Lane      acme@0",
                "  Daemon    not running",
            ]
        );
    }

    #[test]
    fn an_idle_lane_has_no_stop_command() {
        let daemon = Daemon::Running(vec![lane("acme@0", "idle")]);
        let rows = text(
            &theme(Tier::Ansi, Glyphs::Ascii),
            80,
            Some("acme@0"),
            &daemon,
        );
        assert_eq!(
            rows[3..],
            [
                "  Lane      acme@0  idle",
                "  Daemon    running, no lanes running"
            ]
        );
        let rows = text(&theme(Tier::Ansi, Glyphs::Ascii), 80, None, &daemon);
        assert_eq!(rows[3..], ["  Daemon    running, no lanes running"]);
    }

    #[test]
    fn colors_end_with_a_reset() {
        let rendered = render(
            &theme(Tier::Ansi, Glyphs::Unicode),
            80,
            Some("acme@0"),
            &Daemon::Unanswered,
        );
        assert!(rendered.contains("not answering"), "{rendered:?}");
        for line in rendered.lines().filter(|line| line.contains('\x1b')) {
            assert!(line.ends_with("\x1b[0m"), "{line:?}");
        }
    }
}
