//! The Esc menu, after btop's: the devlanes banner and version over lane
//! lines, then options, help and quit.

use std::cell::RefCell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

use crate::banner::{self, Size};
use crate::theme::{Glyph, Glyphs, Theme};

/// What the menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Options,
    Help,
    Quit,
}

/// The items in order, with their key and label.
pub const ITEMS: [(Item, char, &str); 3] = [
    (Item::Options, 'o', "options"),
    (Item::Help, 'h', "help"),
    (Item::Quit, 'q', "quit"),
];

/// Columns of an item row: marker, key, label and the selection's padding.
const ITEM_WIDTH: u16 = 16;

/// The open menu.
#[derive(Debug, Default)]
pub struct Menu {
    pub index: usize,
    /// The box and the item rows of the last render, for the mouse.
    hits: RefCell<(Option<Rect>, Vec<Rect>)>,
}

/// What a key or click did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Stay,
    Close,
    Pick(Item),
}

impl Menu {
    /// Arrows and j/k move, Enter or an item's letter picks it, Esc closes.
    pub fn key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Action::Pick(Item::Quit),
            KeyCode::Esc => Action::Close,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.index = (self.index + ITEMS.len() - 1) % ITEMS.len();
                Action::Stay
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.index = (self.index + 1) % ITEMS.len();
                Action::Stay
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.index = 0;
                Action::Stay
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.index = ITEMS.len() - 1;
                Action::Stay
            }
            KeyCode::Enter | KeyCode::Char(' ') => Action::Pick(ITEMS[self.index].0),
            KeyCode::Char(c) => ITEMS
                .iter()
                .find(|(_, key, _)| *key == c.to_ascii_lowercase())
                .map_or(Action::Stay, |(item, _, _)| Action::Pick(*item)),
            _ => Action::Stay,
        }
    }

    /// A click on an item picks it; a click outside the box closes the menu;
    /// the wheel moves the selection.
    pub fn mouse(&mut self, mouse: MouseEvent) -> Action {
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hits = self.hits.borrow();
                if let Some(index) = hits.1.iter().position(|rect| rect.contains(position)) {
                    self.index = index;
                    return Action::Pick(ITEMS[index].0);
                }
                match hits.0 {
                    Some(rect) if rect.contains(position) => Action::Stay,
                    _ => Action::Close,
                }
            }
            MouseEventKind::ScrollUp => self.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            MouseEventKind::ScrollDown => {
                self.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            }
            _ => Action::Stay,
        }
    }
}

/// A floating box in the focus color; ASCII glyphs draw it with `+-|`.
pub(crate) fn floating_box(theme: &Theme) -> Block<'static> {
    let set = if theme.glyphs == Glyphs::Ascii {
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
    Block::bordered()
        .border_set(set)
        .border_style(theme.focus())
}

/// A rectangle of `width` x `height` centered in `area`, cut to fit.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Draws the menu over `area`, with this TUI's `version` under the banner.
pub fn render(theme: &Theme, menu: &Menu, version: &str, frame: &mut Frame, area: Rect) {
    // Room for the border, the version, the items and blank rows around them.
    let size = Size::pick(
        theme,
        area.width.saturating_sub(12),
        area.height.saturating_sub(12),
    );
    let (art_width, art_height) = size.dimensions();
    let spacious = area.height >= art_height + 12;
    // Banner, blank, version, blank, then the items with blank rows between
    // them when there is room.
    let item_rows: u16 = if spacious { 5 } else { 3 };
    let inner_height = 1 + art_height + 2 + u16::from(spacious) + item_rows + 1;
    let width = art_width.max(ITEM_WIDTH + 8) + 12;
    let outer = centered(area, width, inner_height + 2);
    let inner = Rect::new(
        outer.x + 1,
        outer.y + 1,
        outer.width.saturating_sub(2),
        outer.height.saturating_sub(2),
    );
    frame.render_widget(Clear, outer);
    frame.render_widget(
        floating_box(theme)
            .title_bottom(Line::from(Span::styled(" esc closes ", theme.quiet())).right_aligned()),
        outer,
    );
    // The blank row above the banner goes first when the box is cut.
    let mut y = inner.y + u16::from(inner.height >= inner_height);
    let art = Rect::new(
        inner.x + inner.width.saturating_sub(art_width) / 2,
        y,
        art_width.min(inner.width),
        art_height,
    );
    y += art_height + 1;
    let version = format!("v{version}");
    let version_width = u16::try_from(version.len()).unwrap_or(0);
    let version_rect = Rect::new(
        inner.x + inner.width.saturating_sub(version_width) / 2,
        y.min(inner.bottom().saturating_sub(1)),
        version_width.min(inner.width),
        1,
    );
    y += 1 + u16::from(spacious);
    let item_x = inner.x + inner.width.saturating_sub(ITEM_WIDTH) / 2;
    let items: Vec<Rect> = (0..ITEMS.len())
        .map(|index| {
            let step = if spacious { 2 } else { 1 };
            let row = y + step * u16::try_from(index).unwrap_or(0);
            Rect::new(item_x, row, ITEM_WIDTH.min(inner.width), 1).intersection(inner)
        })
        .collect();
    let mut keep = vec![art, version_rect];
    keep.extend(items.iter().copied());
    banner::backdrop(frame.buffer_mut(), inner, theme, &keep);
    banner::render(frame.buffer_mut(), art.intersection(inner), theme, size);
    frame.render_widget(
        Paragraph::new(Span::styled(version, theme.quiet())),
        version_rect,
    );
    for (index, ((_, key, label), rect)) in ITEMS.iter().zip(&items).enumerate() {
        let selected = index == menu.index;
        let marker = if selected {
            theme.glyph(Glyph::Marker)
        } else {
            " "
        };
        let mut line = Line::from(vec![
            Span::styled(marker, theme.focus()),
            Span::raw("  "),
            Span::styled(key.to_string(), theme.key()),
            Span::raw("  "),
            Span::raw(*label),
        ]);
        let pad = usize::from(rect.width).saturating_sub(line.width());
        line.spans.push(Span::raw(" ".repeat(pad)));
        if selected {
            line = line.patch_style(theme.selection());
        }
        frame.render_widget(Paragraph::new(line).style(Style::new()), *rect);
    }
    *menu.hits.borrow_mut() = (Some(outer), items);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(menu: &mut Menu, code: KeyCode) -> Action {
        menu.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn keys_move_and_pick() {
        let mut menu = Menu::default();
        assert_eq!(
            press(&mut menu, KeyCode::Enter),
            Action::Pick(Item::Options)
        );
        assert_eq!(press(&mut menu, KeyCode::Down), Action::Stay);
        assert_eq!(press(&mut menu, KeyCode::Enter), Action::Pick(Item::Help));
        // Up from the top wraps to quit.
        press(&mut menu, KeyCode::Up);
        press(&mut menu, KeyCode::Up);
        assert_eq!(menu.index, 2);
        assert_eq!(
            press(&mut menu, KeyCode::Char('o')),
            Action::Pick(Item::Options)
        );
        assert_eq!(
            press(&mut menu, KeyCode::Char('h')),
            Action::Pick(Item::Help)
        );
        assert_eq!(
            press(&mut menu, KeyCode::Char('q')),
            Action::Pick(Item::Quit)
        );
        assert_eq!(press(&mut menu, KeyCode::Char('x')), Action::Stay);
        assert_eq!(press(&mut menu, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn esc_opens_the_menu_and_its_items_lead_on() {
        use crate::app::{App, Dialog, Effect, Filter, Msg};
        use crate::settings::{Reply, Request};
        let key = |code| Msg::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let mut app = App::new(100, 30);
        // A kept filter is cancelled first.
        app.filter = Some(Filter {
            text: "api".into(),
            editing: false,
        });
        app.update(key(KeyCode::Esc));
        assert!(app.filter.is_none() && app.dialog.is_none());
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.dialog, Some(Dialog::Menu(_))));
        app.update(key(KeyCode::Esc));
        assert!(app.dialog.is_none());

        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Char('h')));
        assert!(matches!(app.dialog, Some(Dialog::Help)));
        app.update(key(KeyCode::Char('x')));

        app.update(key(KeyCode::Esc));
        let effects = app.update(key(KeyCode::Char('o')));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Settings(Request::Load)))
        );
        app.update(Msg::Settings(Reply::Loaded {
            path: "config.yaml".into(),
            source: Ok("# mine\n".into()),
            error: None,
        }));
        let Some(Dialog::Settings(settings)) = app.dialog.as_mut() else {
            panic!("no options page");
        };
        settings.index = settings.categories[0]
            .items
            .iter()
            .position(|item| item.key == "tui.glyphs")
            .expect("tui.glyphs");
        // Glyphs preview in the running TUI while they are edited.
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Left));
        app.update(key(KeyCode::Enter));
        assert_eq!(app.theme.glyphs, Glyphs::Ascii);
        // Esc with an unsaved change asks first; s saves it.
        app.update(key(KeyCode::Esc));
        assert!(
            matches!(&app.dialog, Some(Dialog::Settings(settings)) if settings.prompt.is_some())
        );
        let effects = app.update(key(KeyCode::Char('s')));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Settings(Request::Save { changes })
                if changes.iter().any(|(key, _)| key == "tui.glyphs")
        )));
        // The page went back to the menu, and Esc goes from there out.
        assert!(matches!(app.dialog, Some(Dialog::Menu(_))));
        app.update(key(KeyCode::Esc));
        assert!(app.dialog.is_none());

        // Quit takes the path q takes: it quits or asks first.
        app.update(key(KeyCode::Esc));
        let effects = app.update(key(KeyCode::Char('q')));
        assert!(
            effects.iter().any(|effect| matches!(effect, Effect::Quit))
                || matches!(app.dialog, Some(Dialog::Confirm { .. } | Dialog::Quit(_))),
            "{effects:?}"
        );
    }

    #[test]
    fn the_menu_fits_every_size() {
        // DOS Rebel, Coder Mini, Pagga, then the plain word.
        for (width, height, banner) in [
            (200, 60, "░░███"),
            (80, 24, "▄████"),
            (56, 16, "░█▀▄"),
            (40, 8, "devlanes"),
        ] {
            let theme = Theme::default();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("terminal");
            terminal
                .draw(|frame| render(&theme, &Menu::default(), "1.2.3", frame, frame.area()))
                .expect("draw");
            let text = terminal.backend().to_string();
            for wanted in [banner, "options", "help", "quit"] {
                assert!(text.contains(wanted), "{width}x{height}: {wanted}\n{text}");
            }
        }
    }

    #[test]
    fn clicks_pick_items_and_outside_closes() {
        let theme = Theme::default();
        let menu = RefCell::new(Menu::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render(&theme, &menu.borrow(), "1.2.3", frame, frame.area()))
            .expect("draw");
        let mut menu = menu.into_inner();
        let quit = menu.hits.borrow().1[2];
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            menu.mouse(click(quit.x + 3, quit.y)),
            Action::Pick(Item::Quit)
        );
        assert_eq!(menu.index, 2);
        let outer = menu.hits.borrow().0.expect("box");
        assert_eq!(menu.mouse(click(outer.x + 1, outer.y + 1)), Action::Stay);
        assert_eq!(menu.mouse(click(0, 0)), Action::Close);
    }
}
