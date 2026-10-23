//! The screen model of a unit whose output renders its terminal (see
//! [`devlanes_core::Unit::renders_screen`]), such as a Windows pseudo console.
//!
//! Such output places text with absolute cursor moves for the terminal's
//! size, so replaying it at another size, as [`crate::screen::Screen`] does
//! for other units, scatters it. Here the history holds the rows that left
//! the top of the grid as text lines instead, and a redraw (see
//! [`Rendered::frame`]) replays those plus the grid's own rows, joined where a
//! row wrapped, at the viewer's size.

use std::{collections::VecDeque, num::NonZeroU16};

use bytes::Bytes;
use devlanes_core::TerminalSize;
use vt100::capture::{BasicFormattedCaptureRange, BasicFormattedCaptureState, RowContents};

/// Rows that left the grid kept for redraws, as text: enough for several
/// screens of colored log lines at any width.
const HISTORY: usize = 128 * 1024;
/// How far into trimmed history a line start is looked for.
const LINE_SEARCH: usize = 16 * 1024;
/// Erase in display, mode 3: clears the scrollback.
const CLEAR_SCROLLBACK: &[u8] = b"\x1b[3J";

/// The grid of a rendered terminal plus the rows that left it.
pub(crate) struct Rendered {
    parser: vt100::Parser<Rows>,
    /// The rows that left the grid as text lines, starting at a line start
    /// once trimmed.
    history: VecDeque<u8>,
    /// The last bytes fed, for a [`CLEAR_SCROLLBACK`] split across two chunks.
    seam: Vec<u8>,
}

/// Collects the rows that leave the top of the grid as text lines.
#[derive(Default)]
struct Rows {
    /// Rows collected since [`Rendered::take_rows`] last took them.
    text: String,
    /// Attributes and whether a line break is owed before the next row.
    state: BasicFormattedCaptureState,
}

impl vt100::Callbacks for Rows {
    /// Keeps a row of the main screen that scrolled off, joined to the one
    /// before when that one wrapped.
    fn on_scroll(&mut self, contents: RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen {
            // Writing to a `String` cannot fail.
            let _ = contents.write_formatted_basic(&mut self.text, &mut self.state);
        }
    }
}

impl Rendered {
    /// An empty grid of `size`.
    pub(crate) fn new(size: TerminalSize) -> Self {
        Self {
            // No scrollback: every row that leaves the grid goes to the history.
            parser: vt100::Parser::new_with_callbacks(
                cells(size.rows),
                cells(size.columns),
                0,
                Rows::default(),
            ),
            history: VecDeque::new(),
            seam: Vec::new(),
        }
    }

    /// Feeds output. A size marker (`ESC[8;<rows>;<columns>t`, see
    /// [`devlanes_core::Unit::renders_screen`]) resizes the grid where it
    /// stands, since what follows it is drawn at that size.
    pub(crate) fn process(&mut self, mut bytes: &[u8]) {
        while let Some((start, end, size)) = size_marker(bytes) {
            self.process_drawn(&bytes[..start]);
            self.resize(size);
            bytes = &bytes[end..];
        }
        self.process_drawn(bytes);
    }

    /// Feeds output drawn at the grid's size. The console host passes a clear
    /// of the scrollback on as [`CLEAR_SCROLLBACK`] (`cls`, `Clear-Host` and
    /// Git's `clear` all send it, while its own startup sends only `ESC[2J`);
    /// the rows that left the grid before it are dropped then, as a terminal
    /// drops its scrollback.
    fn process_drawn(&mut self, bytes: &[u8]) {
        match self.clear_end(bytes) {
            Some(end) => {
                self.parser.process(&bytes[..end]);
                let rows = self.parser.callbacks_mut();
                rows.text.clear();
                rows.state = BasicFormattedCaptureState::default();
                self.history.clear();
                self.parser.process(&bytes[end..]);
            }
            None => self.parser.process(bytes),
        }
        self.take_rows();
        // Keep the bytes a clear split across two chunks could start with.
        let keep = CLEAR_SCROLLBACK.len() - 1;
        self.seam
            .extend_from_slice(&bytes[bytes.len().saturating_sub(keep)..]);
        self.seam.drain(..self.seam.len().saturating_sub(keep));
    }

    /// Where the last [`CLEAR_SCROLLBACK`] in `bytes` ends, also one that
    /// began at the end of the chunk before.
    fn clear_end(&self, bytes: &[u8]) -> Option<usize> {
        let len = CLEAR_SCROLLBACK.len();
        if let Some(start) = bytes.windows(len).rposition(|w| w == CLEAR_SCROLLBACK) {
            return Some(start + len);
        }
        let mut joined = self.seam.clone();
        joined.extend_from_slice(&bytes[..bytes.len().min(len - 1)]);
        let start = joined.windows(len).position(|w| w == CLEAR_SCROLLBACK)?;
        Some(start + len - self.seam.len())
    }

    /// Moves the rows that left the grid into the history, with the line
    /// ends a terminal needs.
    fn take_rows(&mut self) {
        let rows = std::mem::take(&mut self.parser.callbacks_mut().text);
        self.history.extend(crlf(&rows));
        if self.history.len() > HISTORY {
            self.history.drain(..self.history.len() - HISTORY);
            // A redraw starting mid-line would show a fragment as a whole line.
            if let Some(end) = self
                .history
                .iter()
                .take(LINE_SEARCH)
                .position(|&byte| byte == b'\n')
            {
                self.history.drain(..=end);
            }
        }
    }

    /// Resizes the grid the way the console host resizes its buffer, so its
    /// repaint that follows lands on the same rows. On the main screen the
    /// grid's lines are re-wrapped at the new width and keep their place from
    /// the top; rows that no longer fit push the top ones into the history,
    /// and a taller grid gets its new rows at the bottom. Nothing comes back
    /// from the history: the console host keeps no rows above its window. A
    /// program on the alternate screen redraws itself, so that grid is only
    /// cut or extended.
    fn resize(&mut self, size: TerminalSize) {
        if self.parser.screen().alternate_screen() {
            // The scrollback holds no rows; this only forgets how many left,
            // so vt100 adds no blank rows at the top for them.
            self.parser.screen_mut().clear_scrollback();
            self.parser.set_size(cells(size.rows), cells(size.columns));
        } else {
            let replay = self.reflowed(size);
            let rows = std::mem::take(self.parser.callbacks_mut());
            self.parser =
                vt100::Parser::new_with_callbacks(cells(size.rows), cells(size.columns), 0, rows);
            self.parser.process(&replay);
        }
        self.take_rows();
    }

    /// The bytes that draw the grid's lines, joined where a row wrapped, into
    /// an empty grid of `size`, with the cursor on the same character and the
    /// program's attributes and modes. Blank lines at the bottom, below the
    /// cursor, are left out, so they push nothing into the history.
    fn reflowed(&self, size: TerminalSize) -> Vec<u8> {
        let screen = self.parser.screen();
        let grid = self.size();
        let (cursor_row, cursor_column) = screen.cursor_position();
        // Each line as its first row and its width in cells.
        let mut lines: Vec<(u16, usize)> = Vec::new();
        let mut first = 0;
        for row in 0..grid.rows {
            if !screen.row_wrapped(row) || row + 1 == grid.rows {
                let width = usize::from(row - first) * usize::from(grid.columns)
                    + usize::from(written_end(screen, row));
                lines.push((first, width));
                first = row + 1;
            }
        }
        let cursor_line = lines
            .iter()
            .rposition(|&(first, _)| first <= cursor_row)
            .unwrap_or(0);
        let last = lines
            .iter()
            .rposition(|&(_, width)| width > 0)
            .map_or(cursor_line, |last| last.max(cursor_line));
        let mut text = String::new();
        // Writing to a `String` cannot fail.
        let _ = screen.write_contents_formatted_basic(
            &mut text,
            BasicFormattedCaptureRange::Full(&mut BasicFormattedCaptureState::default()),
        );
        let columns = usize::from(size.columns.max(1));
        let rows_of = |width: usize| width.div_ceil(columns).max(1);
        let mut bytes = Vec::new();
        for (i, line) in text.split('\n').take(last + 1).enumerate() {
            if i > 0 {
                bytes.extend_from_slice(b"\r\n");
            }
            bytes.extend_from_slice(line.as_bytes());
        }
        let fed: usize = lines[..=last]
            .iter()
            .map(|&(_, width)| rows_of(width))
            .sum();
        let scrolled = fed.saturating_sub(usize::from(size.rows));
        let (first, _) = lines[cursor_line];
        let offset = usize::from(cursor_row - first) * usize::from(grid.columns)
            + usize::from(cursor_column);
        let above: usize = lines[..cursor_line]
            .iter()
            .map(|&(_, width)| rows_of(width))
            .sum();
        let row = (above + offset / columns)
            .saturating_sub(scrolled)
            .min(usize::from(size.rows.max(1) - 1));
        bytes.extend_from_slice(format!("\x1b[{};{}H", row + 1, offset % columns + 1).as_bytes());
        bytes.extend_from_slice(screen.attributes_formatted().as_bytes());
        bytes.extend_from_slice(screen.input_mode_formatted().as_bytes());
        if screen.hide_cursor() {
            bytes.extend_from_slice(b"\x1b[?25l");
        }
        bytes
    }

    /// The grid's size.
    fn size(&self) -> TerminalSize {
        let (rows, columns) = self.parser.screen().size();
        TerminalSize {
            columns: columns.get(),
            rows: rows.get(),
        }
    }

    /// The model as the console host drew it.
    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// A whole redraw for a viewer of `size` (the grid's size when `None`).
    pub(crate) fn snapshot(&self, size: Option<TerminalSize>) -> Bytes {
        self.frame(size).render(None).0
    }

    /// What a redraw for a viewer of `size` (the grid's size when `None`)
    /// needs: a copy, so that the replay can run after the screen lock is
    /// released.
    pub(crate) fn frame(&self, size: Option<TerminalSize>) -> Frame {
        let screen = self.parser.screen();
        let size = size.unwrap_or_else(|| self.size());
        let drawn = screen
            .alternate_screen()
            .then(|| screen.contents_formatted());
        let mut lines = Vec::new();
        if drawn.is_none() {
            let grid = self.grid_lines(size);
            lines.reserve_exact(self.history.len() + grid.len());
            lines.extend(&self.history);
            lines.extend(grid);
        }
        Frame {
            drawn,
            lines,
            size,
            hide_cursor: screen.hide_cursor(),
            input_modes: screen.input_mode_formatted(),
        }
    }

    /// The grid's rows as text lines that continue the history, for a viewer
    /// of `size`. Blank rows under the cursor are left out: a console host's
    /// repaint after a resize leaves them, and replayed they would push lines
    /// off the top. A viewer as wide as the grid wraps the cursor's line as
    /// the grid did, so the cursor keeps its column there; for any other
    /// viewer the cursor moves on from the row's last cell as far as it is
    /// from it in the grid, which keeps a prompt's trailing space (the console
    /// host sends a cursor move for it, not a cell).
    fn grid_lines(&self, size: TerminalSize) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut state = self.parser.callbacks().state.clone();
        let mut text = String::new();
        // Writing to a `String` cannot fail.
        let _ = screen.write_contents_formatted_basic(
            &mut text,
            BasicFormattedCaptureRange::Full(&mut state),
        );
        let lines = text.trim_end_matches('\n');
        // Each blank row at the bottom adds one line break.
        let blank = text.len() - lines.len();
        let grid = self.size();
        let (row, column) = screen.cursor_position();
        let below = usize::from(grid.rows.saturating_sub(row + 1));
        let mut bytes = crlf(lines);
        bytes.extend_from_slice(&b"\r\n".repeat(blank.saturating_sub(below)));
        bytes.extend_from_slice(b"\x1b[m");
        if blank >= below {
            if size.columns == grid.columns {
                bytes.extend_from_slice(format!("\x1b[{}G", column + 1).as_bytes());
            } else {
                let gap = column.saturating_sub(written_end(screen, row));
                if gap > 0 {
                    bytes.extend_from_slice(format!("\x1b[{gap}C").as_bytes());
                }
            }
        }
        bytes
    }
}

/// A rendered screen copied for one redraw (see [`Rendered::frame`]).
pub(crate) struct Frame {
    /// The alternate screen as the program drew it; `None` on the main screen.
    drawn: Option<String>,
    /// The history and the grid as text lines, replayed at `size`.
    lines: Vec<u8>,
    size: TerminalSize,
    hide_cursor: bool,
    input_modes: String,
}

impl Frame {
    /// The bytes that bring a viewer showing `shown` (what the last redraw
    /// left; a whole screen is drawn when `None` or of another size) to this
    /// frame, and the screen they leave it showing. Only the rows that differ
    /// are drawn again, so a viewer sees no flicker. CPU work; blocking.
    pub(crate) fn render(self, shown: Option<&vt100::Screen>) -> (Bytes, Option<vt100::Screen>) {
        let mut bytes = Vec::new();
        let screen = match self.drawn {
            Some(drawn) => {
                bytes.extend_from_slice(b"\x1b[H\x1b[2J");
                bytes.extend_from_slice(drawn.as_bytes());
                None
            }
            None => {
                let mut replay =
                    vt100::Parser::new(cells(self.size.rows), cells(self.size.columns), 0);
                replay.process(&self.lines);
                let screen = replay.screen();
                match shown.filter(|shown| shown.size() == screen.size()) {
                    Some(shown) => bytes.extend_from_slice(screen.contents_diff(shown).as_bytes()),
                    None => {
                        bytes.extend_from_slice(b"\x1b[H\x1b[2J");
                        bytes.extend_from_slice(screen.contents_formatted().as_bytes());
                    }
                }
                // The sequence that hid the cursor is not part of the lines.
                bytes.extend_from_slice(if self.hide_cursor {
                    b"\x1b[?25l"
                } else {
                    b"\x1b[?25h"
                });
                Some(screen.clone())
            }
        };
        bytes.extend_from_slice(self.input_modes.as_bytes());
        (Bytes::from(bytes), screen)
    }
}

/// The columns of `row` up to its last cell that a capture writes: one with
/// text or with attributes of its own.
fn written_end(screen: &vt100::Screen, row: u16) -> u16 {
    let fresh = vt100::Parser::new(NonZeroU16::MIN, NonZeroU16::MIN, 0);
    let blank = fresh.screen().cell(0, 0);
    let columns = screen.size().1.get();
    (0..columns)
        .rev()
        .find(|&column| screen.cell(row, column) != blank)
        .map_or(0, |column| column + 1)
}

/// The first `ESC[8;<rows>;<columns>t` in `bytes`: where it starts and ends,
/// and the size it names.
fn size_marker(bytes: &[u8]) -> Option<(usize, usize, TerminalSize)> {
    const START: &[u8] = b"\x1b[8;";
    let mut from = 0;
    while let Some(at) = bytes[from..].windows(START.len()).position(|w| w == START) {
        let start = from + at;
        let params = &bytes[start + START.len()..];
        let end = params
            .iter()
            .position(|&byte| !(byte.is_ascii_digit() || byte == b';'));
        if let Some(end) = end
            && params[end] == b't'
            && let Some((rows, columns)) = std::str::from_utf8(&params[..end])
                .ok()
                .and_then(|text| text.split_once(';'))
            && let (Ok(rows), Ok(columns)) = (rows.parse(), columns.parse())
        {
            let size = TerminalSize { columns, rows };
            return Some((start, start + START.len() + end + 1, size));
        }
        from = start + 1;
    }
    None
}

/// `text` with `\r\n` line ends; captured rows end lines with `\n` alone.
fn crlf(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        if byte == b'\n' {
            bytes.push(b'\r');
        }
        bytes.push(byte);
    }
    bytes
}

/// vt100 rejects zero-sized screens; a client reporting 0 rows still gets one.
fn cells(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bytes` drawn by a fresh terminal of `columns`x`rows`.
    fn draw(bytes: &[u8], columns: u16, rows: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(cells(rows), cells(columns), 0);
        parser.process(bytes);
        parser
    }

    /// Line `i`: 50 columns, wider than the test grids.
    fn line(i: usize) -> String {
        format!("L{i:02} {}", "x".repeat(46))
    }

    /// A 40x8 grid fed lines 1 to 20 the way Windows' inbox console host sends
    /// them: the first screen as written, then, at the bottom, the first 40
    /// columns, a line break, and the rest written from the last column of the
    /// row above, so the terminal marks that row as wrapped.
    fn rendered() -> Rendered {
        let mut screen = Rendered::new(TerminalSize {
            columns: 40,
            rows: 8,
        });
        for i in 1..=20 {
            let text = line(i);
            if i <= 3 {
                screen.process(format!("{text}\r\n").as_bytes());
            } else {
                let (head, _) = text.split_at(40);
                let rest = &text[39..];
                screen.process(format!("{head}\r\n\x1b[7;40H{rest}\r\n").as_bytes());
            }
        }
        screen
    }

    /// A redraw for a viewer of `size`, from scratch.
    fn redraw(screen: &Rendered, size: Option<TerminalSize>) -> Bytes {
        screen.snapshot(size)
    }

    /// The rows of a viewer of `columns`x`rows` after `bytes`, without
    /// trailing blanks.
    fn rows_after(bytes: &[u8], columns: u16, rows: u16) -> Vec<String> {
        draw(bytes, columns, rows)
            .screen()
            .rows(0, columns)
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    /// Whole lines at the viewer's size, not scattered by cursor moves meant
    /// for the console's size.
    #[test]
    fn a_rendered_screen_is_rewrapped_for_another_size() {
        let screen = rendered();
        let size = TerminalSize {
            columns: 60,
            rows: 10,
        };
        let shown = draw(&redraw(&screen, Some(size)), 60, 10);
        let rows: Vec<String> = shown.screen().rows(0, 60).collect();
        assert_eq!(rows[9], "");
        for (row, i) in rows[..9].iter().zip(12..) {
            assert_eq!(row, &line(i));
        }
    }

    /// After a resize the console host repaints its window from the top and
    /// leaves blank rows under the cursor; the redraw still fills the screen
    /// from the lines above, and the line cut by the window's top edge is
    /// joined to its start.
    #[test]
    fn a_repaint_after_a_resize_fills_the_screen() {
        let mut screen = rendered();
        let size = TerminalSize {
            columns: 60,
            rows: 8,
        };
        screen.resize(size);
        // The window kept its top row, the end of line 17.
        let mut repaint = format!("\x1b[?25l\x1b[H{}\x1b[K\r\n", &line(17)[40..]);
        for i in 18..=20 {
            repaint.push_str(&format!("{}\x1b[K\r\n", line(i)));
        }
        repaint.push_str("\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[5;1H\x1b[?25h");
        screen.process(repaint.as_bytes());
        let shown = draw(&redraw(&screen, None), 60, 8);
        let rows: Vec<String> = shown.screen().rows(0, 60).collect();
        assert_eq!(rows[7], "");
        for (row, i) in rows[..7].iter().zip(14..) {
            assert_eq!(row, &line(i));
        }
        assert_eq!(shown.screen().cursor_position(), (7, 0));
    }

    /// A viewer as wide as the grid keeps the cursor's column, so the cursor
    /// of a prompt stays after it; a viewer of another width keeps the space
    /// after the prompt.
    #[test]
    fn a_prompt_keeps_its_cursor() {
        let mut screen = Rendered::new(TerminalSize {
            columns: 40,
            rows: 4,
        });
        // The console host sends a prompt's trailing space as a cursor move.
        screen.process(b"one\r\ntwo\r\n>\x1b[3;3H");
        let shown = draw(&redraw(&screen, None), 40, 4);
        assert_eq!(
            rows_after(&redraw(&screen, None), 40, 4),
            ["one", "two", ">", ""]
        );
        assert_eq!(shown.screen().cursor_position(), (2, 2));
        let size = TerminalSize {
            columns: 30,
            rows: 4,
        };
        let bytes = redraw(&screen, Some(size));
        assert_eq!(rows_after(&bytes, 30, 4), ["one", "two", ">", ""]);
        assert_eq!(draw(&bytes, 30, 4).screen().cursor_position(), (2, 2));
    }

    /// A redraw after another one changes only what differs, and leaves the
    /// viewer with the same screen as a whole redraw would.
    #[test]
    fn a_redraw_after_another_draws_the_difference() {
        let mut screen = rendered();
        let size = Some(TerminalSize {
            columns: 60,
            rows: 10,
        });
        let (first, shown) = screen.frame(size).render(None);
        screen.process(b"L21 new\r\n");
        let (diff, _) = screen.frame(size).render(shown.as_ref());
        let whole = redraw(&screen, size);
        assert!(diff.len() < whole.len() / 2, "{diff:?}");
        let mut viewer = first.to_vec();
        viewer.extend_from_slice(&diff);
        assert_eq!(rows_after(&viewer, 60, 10), rows_after(&whole, 60, 10));
        assert_eq!(rows_after(&viewer, 60, 10)[8], "L21 new");
    }

    /// A clear of the scrollback (`cls` in a pseudo console) drops the rows
    /// that left the grid before it, also when it arrives in two chunks; the
    /// console host's `ESC[2J` at startup does not.
    #[test]
    fn a_clear_of_the_scrollback_drops_the_history() {
        let size = TerminalSize {
            columns: 40,
            rows: 4,
        };
        let mut screen = Rendered::new(size);
        screen.process(b"\x1b[?25l\x1b[2J\x1b[m\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
        assert_eq!(rows_after(&redraw(&screen, Some(size)), 40, 4)[0], "three");
        let taller = TerminalSize {
            columns: 40,
            rows: 8,
        };
        assert_eq!(rows_after(&redraw(&screen, Some(taller)), 40, 8)[0], "one");
        screen.process(b"\x1b[H\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[H\x1b[");
        screen.process(b"3Jsix\r\n");
        let rows = rows_after(&redraw(&screen, Some(taller)), 40, 8);
        assert_eq!(rows[..2], ["six", ""]);
    }

    /// A taller grid gets its new rows at the bottom, as the console host's
    /// own buffer does, so its repaint lands on the same rows.
    #[test]
    fn a_taller_grid_grows_at_the_bottom() {
        let mut screen = rendered();
        let (row, _) = screen.screen().cursor_position();
        screen.resize(TerminalSize {
            columns: 40,
            rows: 12,
        });
        assert_eq!(screen.screen().cursor_position().0, row);
        // No blank rows between the history and the grid's rows.
        let rows = rows_after(&redraw(&screen, None), 40, 12);
        assert!(rows[..11].iter().all(|row| !row.is_empty()), "{rows:?}");
        assert_eq!(rows[11], "");
    }

    /// Line `i` of the reflow test: 180 columns, like a dev server's.
    fn wide(i: usize) -> String {
        format!("P{i:05} {}END", "abcdefghij".repeat(18)[..170].to_owned())
    }

    /// The console host's repaint after a resize, as the inbox one sends it:
    /// from the top, every line whole (the terminal wraps it), then the blank
    /// rows below, then the cursor.
    fn repaint(first: usize, last: usize, blank: usize, cursor_row: usize) -> String {
        let mut bytes = String::from("\x1b[?25l\x1b[H");
        for i in first..=last {
            bytes.push_str(&format!("{}\r\n", wide(i)));
        }
        bytes.push_str(&"\x1b[K\r\n".repeat(blank));
        bytes.push_str(&format!("\x1b[K\x1b[{cursor_row};1H\x1b[?25h"));
        bytes
    }

    /// Every service line once and in order, wrapped at `columns`, in the
    /// rows of a viewer of `columns`x`rows` after `bytes`.
    fn lines_in_order(bytes: &[u8], columns: u16, rows: u16) -> Vec<usize> {
        let shown = rows_after(bytes, columns, rows);
        let numbers: Vec<usize> = shown
            .iter()
            .filter(|row| row.starts_with('P'))
            .filter_map(|row| row.get(1..6)?.parse().ok())
            .collect();
        for pair in numbers.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "{shown:#?}");
        }
        numbers
    }

    /// Narrowing and widening the console again and again, as zooming a pane
    /// does: the console host re-wraps its lines and repaints from the top,
    /// and the lines it pushed out of its window stay in the history, so a
    /// redraw never skips or repeats one.
    #[test]
    fn resizes_rewrap_like_the_console_host() {
        let mut screen = Rendered::new(TerminalSize {
            columns: 200,
            rows: 45,
        });
        for i in 1..=150 {
            screen.process(format!("{}\r\n", wide(i)).as_bytes());
        }
        let narrow = TerminalSize {
            columns: 152,
            rows: 45,
        };
        let wide_size = TerminalSize {
            columns: 200,
            rows: 45,
        };
        for _ in 0..3 {
            // The runner marks where each repaint at a new size begins.
            let marker = |size: TerminalSize| format!("\x1b[8;{};{}t", size.rows, size.columns);
            // 22 lines of two rows each, then the cursor's row.
            screen.process(format!("{}{}", marker(narrow), repaint(129, 150, 0, 45)).as_bytes());
            assert_eq!(screen.size(), narrow);
            let shown = lines_in_order(&redraw(&screen, None), 152, 45);
            assert_eq!(shown.last(), Some(&150));
            assert_eq!(shown.len(), 22);
            screen.process(marker(wide_size).as_bytes());
            screen.process(repaint(129, 150, 22, 23).as_bytes());
            let shown = lines_in_order(&redraw(&screen, None), 200, 45);
            assert_eq!(shown.last(), Some(&150));
            assert_eq!(shown.len(), 44, "the history fills the screen");
        }
    }

    /// The alternate screen is sent as the program drew it.
    #[test]
    fn the_alternate_screen_is_sent_as_drawn() {
        let mut screen = Rendered::new(TerminalSize {
            columns: 40,
            rows: 5,
        });
        screen.process(b"before\r\n\x1b[?1049h\x1b[Hmenu");
        let shown = draw(&redraw(&screen, None), 40, 5);
        assert_eq!(shown.screen().contents(), "menu");
    }

    /// Trimmed history starts at a line, never in the middle of one.
    #[test]
    fn history_is_trimmed_at_a_line_start() {
        let mut screen = Rendered::new(TerminalSize {
            columns: 1000,
            rows: 2,
        });
        let line = format!("{}\r\n", "z".repeat(998));
        for _ in 0..(HISTORY / line.len() + 10) {
            screen.process(line.as_bytes());
        }
        assert!(screen.history.len() <= HISTORY);
        let start: Vec<u8> = screen.history.iter().take(line.len()).copied().collect();
        assert_eq!(start, line.as_bytes());
    }
}
