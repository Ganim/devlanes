//! Client-side terminal emulation for the one visible unit.
//!
//! tui-term 0.3 depends on vt100 0.16 while devlanes uses 0.18, so its
//! built-in `Screen` impl does not apply. [`ScreenView`] implements tui-term's
//! traits on newtypes instead.

use std::num::NonZeroU16;

use ratatui::{
    buffer::Cell as BufferCell,
    style::{Color, Modifier, Style},
};

/// Lines kept client-side; older history comes from the daemon's log store.
pub const SCROLLBACK_LINES: usize = 2000;

/// The parser for the attached unit plus the user's scroll position.
pub struct TerminalScreen {
    parser: vt100::Parser,
}

impl TerminalScreen {
    /// Creates an empty screen; zero sizes are clamped to one cell.
    pub fn new(rows: u16, columns: u16) -> Self {
        Self {
            parser: vt100::Parser::new(non_zero(rows), non_zero(columns), SCROLLBACK_LINES),
        }
    }

    /// Feeds raw output from the attach stream.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resizes the grid to the pane.
    pub fn resize(&mut self, rows: u16, columns: u16) {
        self.parser
            .screen_mut()
            .set_size(non_zero(rows), non_zero(columns));
    }

    /// Current `(rows, columns)`.
    pub fn size(&self) -> (u16, u16) {
        let (rows, columns) = self.parser.screen().size();
        (rows.get(), columns.get())
    }

    /// The vt100 screen, for input modes and text extraction.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Lines scrolled back from the live view.
    pub fn scroll_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// Scrolls by `delta` lines (positive is older) and reports whether the
    /// request went past the oldest line held client-side.
    pub fn scroll_by(&mut self, delta: isize) -> bool {
        let current = self.scroll_offset();
        let wanted = current.saturating_add_signed(delta);
        self.parser.screen_mut().set_scrollback(wanted);
        delta > 0 && self.scroll_offset() < wanted
    }

    /// Returns to the live view.
    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Plain text of the visible grid between two cells, inclusive of the start.
    pub fn text_between(&self, start: (u16, u16), end: (u16, u16)) -> String {
        let (first, last) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.parser
            .screen()
            .contents_between(first.0, first.1, last.0, last.1.saturating_add(1))
    }
}

/// vt100 rejects zero dimensions; a hidden pane still keeps a 1x1 parser.
fn non_zero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

/// One borrowed vt100 cell, so tui-term's `Cell` trait can be implemented locally.
pub struct CellView<'a>(&'a vt100::Cell);

/// Visible cells of a vt100 screen, gathered once per frame as references.
pub struct ScreenView<'a> {
    screen: &'a vt100::Screen,
    columns: u16,
    cells: Vec<Option<CellView<'a>>>,
}

impl<'a> ScreenView<'a> {
    /// Collects references to the visible cells; no cell contents are copied.
    pub fn new(screen: &'a vt100::Screen) -> Self {
        let (rows, columns) = screen.size();
        let (rows, columns) = (rows.get(), columns.get());
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(columns));
        for row in 0..rows {
            for column in 0..columns {
                cells.push(screen.cell(row, column).map(CellView));
            }
        }
        Self {
            screen,
            columns,
            cells,
        }
    }
}

impl<'a> tui_term::widget::Screen for ScreenView<'a> {
    type C = CellView<'a>;

    /// Looks up a visible cell in the gathered grid.
    fn cell(&self, row: u16, col: u16) -> Option<&Self::C> {
        if col >= self.columns {
            return None;
        }
        let index = usize::from(row) * usize::from(self.columns) + usize::from(col);
        self.cells.get(index)?.as_ref()
    }

    /// Hidden while scrolled back, since the cursor belongs to the live screen.
    fn hide_cursor(&self) -> bool {
        self.screen.hide_cursor() || self.screen.scrollback() > 0
    }

    /// The cursor shifted by the scrollback offset, as tui-term expects.
    fn cursor_position(&self) -> (u16, u16) {
        let (row, col) = self.screen.cursor_position();
        let offset = u16::try_from(self.screen.scrollback()).unwrap_or(u16::MAX);
        (row.saturating_add(offset), col)
    }
}

impl tui_term::widget::Cell for CellView<'_> {
    /// Mirrors vt100's notion of an empty cell.
    fn has_contents(&self) -> bool {
        self.0.has_contents()
    }

    /// Copies symbol and style into the ratatui buffer.
    fn apply(&self, cell: &mut BufferCell) {
        apply_cell(self.0, cell);
    }
}

/// Writes one vt100 cell into a ratatui buffer cell.
pub fn apply_cell(source: &vt100::Cell, target: &mut BufferCell) {
    if source.has_contents() {
        target.set_symbol(source.contents());
    }
    let mut modifier = Modifier::empty();
    if source.bold() {
        modifier |= Modifier::BOLD;
    }
    if source.dim() {
        modifier |= Modifier::DIM;
    }
    if source.italic() {
        modifier |= Modifier::ITALIC;
    }
    if source.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if source.inverse() {
        modifier |= Modifier::REVERSED;
    }
    target.set_style(
        Style::reset()
            .fg(map_color(source.fgcolor()))
            .bg(map_color(source.bgcolor()))
            .add_modifier(modifier),
    );
}

/// vt100 colors map one to one onto ratatui colors.
fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
