//! The welcome shown once per devlanes home: the banner, what devlanes does,
//! the first keys to learn and a link to the docs.

use std::cell::RefCell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::banner::{self, Size};
use crate::menu::{centered, floating_box};
use crate::theme::{Glyph, Theme};

/// Where the docs live; `o` or a click opens it.
pub const DOCS_URL: &str = "https://devlanes.run/docs";

/// The dismissal key, stored with the release notices. The first-run hint
/// strip used the same key, so whoever hid it does not get this either.
pub const KEY: &str = "tui-first-run";

const TAGLINE: &str = "Run a dev environment per git worktree, side by side.";

const FEATURES: [&str; 4] = [
    "a lane per git worktree, with its own ports, hostnames and data",
    "services, tasks and containers from devlanes.yaml",
    "a live terminal and stored logs for every service",
    "everything keeps running after this window closes",
];

const KEYS: [(&str, &str); 5] = [
    ("j k", "move through lanes and units"),
    ("enter", "type into a service; esc leaves"),
    ("tab", "next project"),
    ("M  D", "monitor, daemon log"),
    ("?", "every key, any time"),
];

/// Columns of the key column in the key list.
const KEY_WIDTH: usize = 8;

/// The open welcome.
#[derive(Debug, Default)]
pub struct Welcome {
    /// The box and the docs link of the last render, for the mouse.
    hits: RefCell<(Option<Rect>, Option<Rect>)>,
}

/// What a key or click did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Stay,
    Close,
    OpenDocs,
    Quit,
}

impl Welcome {
    /// Enter, Esc or Space closes; `o` opens the docs; `q` and Ctrl+C quit
    /// as they do everywhere else.
    pub fn key(&self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Action::Quit,
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('o') => Action::OpenDocs,
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ') => Action::Close,
            _ => Action::Stay,
        }
    }

    /// A click on the link opens the docs; a click outside the box closes it.
    pub fn mouse(&self, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Stay;
        }
        let position = Position::new(mouse.column, mouse.row);
        let hits = self.hits.borrow();
        if hits.1.is_some_and(|link| link.contains(position)) {
            return Action::OpenDocs;
        }
        match hits.0 {
            Some(rect) if rect.contains(position) => Action::Stay,
            _ => Action::Close,
        }
    }
}

/// The text under the banner, and the row and column of the docs URL in it.
fn text(theme: &Theme) -> (Vec<Line<'static>>, usize, usize) {
    let mut lines = vec![
        Line::styled(TAGLINE, Style::new().add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];
    let bullet = theme.glyph(Glyph::Folded);
    for feature in FEATURES {
        lines.push(Line::from(vec![
            Span::styled(format!("{bullet} "), theme.focus()),
            Span::raw(feature),
        ]));
    }
    lines.push(Line::from(""));
    for (key, label) in KEYS {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:<KEY_WIDTH$}"), theme.key()),
            Span::styled(label, theme.quiet()),
        ]));
    }
    lines.push(Line::from(""));
    let label = format!("{:<KEY_WIDTH$}", "docs");
    let column = label.len();
    lines.push(Line::from(vec![
        Span::styled(label, theme.key()),
        Span::styled(DOCS_URL, theme.focus().add_modifier(Modifier::UNDERLINED)),
        Span::styled("  o opens it", theme.quiet()),
    ]));
    let row = lines.len() - 1;
    (lines, row, column)
}

/// Draws the welcome over `area`.
pub fn render(theme: &Theme, welcome: &Welcome, frame: &mut Frame, area: Rect) {
    let (lines, link_row, link_column) = text(theme);
    let text_width = u16::try_from(lines.iter().map(Line::width).max().unwrap_or(0)).unwrap_or(0);
    let text_height = u16::try_from(lines.len()).unwrap_or(0);
    // The border, a blank row above and below the banner and one at the bottom.
    let size = Size::pick(
        theme,
        area.width.saturating_sub(8),
        area.height.saturating_sub(text_height + 5),
    );
    let (art_width, art_height) = size.dimensions();
    let inner_height = 1 + art_height + 1 + text_height + 1;
    let outer = centered(area, art_width.max(text_width) + 8, inner_height + 2);
    let inner = Rect::new(
        outer.x + 1,
        outer.y + 1,
        outer.width.saturating_sub(2),
        outer.height.saturating_sub(2),
    );
    frame.render_widget(Clear, outer);
    frame.render_widget(
        floating_box(theme).title_bottom(
            Line::from(Span::styled(" enter or esc to start ", theme.quiet())).right_aligned(),
        ),
        outer,
    );
    // The blank row above the banner goes first when the box is cut.
    let top = inner.y + u16::from(inner.height >= inner_height);
    let art = Rect::new(
        inner.x + inner.width.saturating_sub(art_width) / 2,
        top,
        art_width.min(inner.width),
        art_height,
    )
    .intersection(inner);
    // Lane lines behind the banner only, as on the website.
    let band = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        top - inner.y + art_height + 1,
    )
    .intersection(inner);
    banner::backdrop(frame.buffer_mut(), band, theme, &[art]);
    banner::render(frame.buffer_mut(), art, theme, size);
    let body = Rect::new(
        inner.x + inner.width.saturating_sub(text_width) / 2,
        top + art_height + 1,
        text_width.min(inner.width),
        inner.bottom().saturating_sub(top + art_height + 1),
    )
    .intersection(inner);
    frame.render_widget(Paragraph::new(lines), body);
    let link = Rect::new(
        body.x + u16::try_from(link_column).unwrap_or(0),
        body.y + u16::try_from(link_row).unwrap_or(0),
        u16::try_from(DOCS_URL.len()).unwrap_or(0),
        1,
    );
    // A link cut by a small terminal is not offered for clicks.
    let shown =
        body.contains(link.as_position()) && body.contains(Position::new(link.right() - 1, link.y));
    *welcome.hits.borrow_mut() = (Some(outer), shown.then_some(link));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(width: u16, height: u16) -> (String, Welcome) {
        let theme = Theme::default();
        let welcome = Welcome::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("terminal");
        terminal
            .draw(|frame| render(&theme, &welcome, frame, frame.area()))
            .expect("draw");
        (terminal.backend().to_string(), welcome)
    }

    #[test]
    fn keys_close_open_the_docs_or_quit() {
        let welcome = Welcome::default();
        let press = |code| welcome.key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(press(KeyCode::Enter), Action::Close);
        assert_eq!(press(KeyCode::Esc), Action::Close);
        assert_eq!(press(KeyCode::Char(' ')), Action::Close);
        assert_eq!(press(KeyCode::Char('o')), Action::OpenDocs);
        assert_eq!(press(KeyCode::Char('q')), Action::Quit);
        assert_eq!(press(KeyCode::Char('j')), Action::Stay);
        assert_eq!(
            welcome.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn the_welcome_fits_and_keeps_the_link_clickable() {
        // DOS Rebel, then Coder Mini.
        for (width, height, banner) in [(200, 60, "░░███"), (80, 24, "▄████")] {
            let (text, welcome) = draw(width, height);
            for wanted in [
                banner,
                TAGLINE,
                FEATURES[0],
                "type into a service",
                DOCS_URL,
            ] {
                assert!(text.contains(wanted), "{width}x{height}: {wanted}\n{text}");
            }
            let link = welcome.hits.borrow().1.expect("link");
            let click = |column, row| MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            assert_eq!(welcome.mouse(click(link.x, link.y)), Action::OpenDocs);
            assert_eq!(welcome.mouse(click(0, 0)), Action::Close);
        }
    }

    #[test]
    fn a_cut_link_is_not_clickable() {
        let (_, welcome) = draw(56, 16);
        assert!(welcome.hits.borrow().1.is_none());
    }
}
