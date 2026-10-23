//! A unit's screen model and the recent output it was built from.
//!
//! A terminal grid keeps every line wrapped at the width it was printed at, so
//! after a resize, or for a viewer of another size, the grid alone shows long
//! lines cut at the old width and leaves extra rows empty. Programs on the
//! main screen (dev servers, loggers, pipe units) get their snapshot by
//! replaying the recent output at the viewer's size instead, the way a fresh
//! terminal of that size would have drawn it. Programs on the alternate screen
//! (editors, pagers) redraw themselves after a resize, so their grid is sent.
//!
//! Output that renders a screen (see [`devlanes_core::Unit::renders_screen`])
//! places text with absolute cursor moves for the terminal's size, so replaying
//! it at another size scatters it. For such units the history holds the rows
//! that left the top of the grid as text lines instead, and a redraw (see
//! [`Screen::frame`]) replays those plus the grid's own rows, joined where a
//! row wrapped.

use std::{collections::VecDeque, num::NonZeroU16};

use bytes::Bytes;
use devlanes_core::TerminalSize;
use vt100::capture::{BasicFormattedCaptureRange, BasicFormattedCaptureState, RowContents};

/// Rows kept above the grid; deep history comes from log segments.
const SCROLLBACK: usize = 200;
/// Recent output kept for replay: enough for several screens of colored log
/// lines at any width.
const HISTORY: usize = 128 * 1024;
/// How far into trimmed history a line start is looked for.
const LINE_SEARCH: usize = 16 * 1024;
/// Erase in display, mode 3: clears the scrollback.
const CLEAR_SCROLLBACK: &[u8] = b"\x1b[3J";

/// The screen model plus the output it was fed, for replay at other sizes.
pub(crate) struct Screen {
    parser: vt100::Parser<Rows>,
    /// The last [`HISTORY`] bytes fed to `parser`, or for rendered output the
    /// rows that left the grid as text lines, starting at a line start once
    /// trimmed.
    history: VecDeque<u8>,
    /// The last bytes of rendered output, for a [`CLEAR_SCROLLBACK`] split
    /// across two chunks.
    seam: Vec<u8>,
}

/// Collects the rows that leave the top of a rendered screen as text lines.
#[derive(Default)]
struct Rows {
    /// Set when the output renders a screen; otherwise rows are not kept.
    enabled: bool,
    /// Rows collected since the last [`Screen::process`] took them.
    text: String,
    /// Attributes and whether a line break is owed before the next row.
    state: BasicFormattedCaptureState,
}

impl vt100::Callbacks for Rows {
    /// Keeps a row of the main screen that scrolled off, joined to the one
    /// before when that one wrapped.
    fn on_scroll(&mut self, contents: RowContents<'_>, alternate_screen: bool) {
        if self.enabled && !alternate_screen {
            // Writing to a `String` cannot fail.
            let _ = contents.write_formatted_basic(&mut self.text, &mut self.state);
        }
    }
}

impl Screen {
    /// An empty screen of `size`. With `rendered`, the output is a rendering
    /// of the terminal and rows that scroll off are kept as text lines.
    pub(crate) fn new(size: TerminalSize, rendered: bool) -> Self {
        let rows = Rows {
            enabled: rendered,
            ..Rows::default()
        };
        // Rendered output keeps no rows in the grid's scrollback: every row
        // that leaves the grid goes to the history instead.
        let scrollback = if rendered { 0 } else { SCROLLBACK };
        Self {
            parser: vt100::Parser::new_with_callbacks(
                cells(size.rows),
                cells(size.columns),
                scrollback,
                rows,
            ),
            history: VecDeque::new(),
            seam: Vec::new(),
        }
    }

    /// Whether the output renders a screen.
    pub(crate) fn rendered(&self) -> bool {
        self.parser.callbacks().enabled
    }

    /// Feeds output to the model and the replay history.
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        if self.rendered() {
            self.process_rendered(bytes);
        } else {
            self.parser.process(bytes);
            self.history.extend(bytes);
            self.trim();
        }
    }

    /// Feeds rendered output. The console host passes a clear of the
    /// scrollback on as [`CLEAR_SCROLLBACK`] (`cls`, `Clear-Host` and Git's
    /// `clear` all send it, while its own startup sends only `ESC[2J`); the
    /// rows that left the grid before it are dropped then, as a terminal drops
    /// its scrollback.
    fn process_rendered(&mut self, bytes: &[u8]) {
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

    /// Moves the rows that left a rendered grid into the history, with the
    /// line ends a terminal needs.
    fn take_rows(&mut self) {
        let rows = std::mem::take(&mut self.parser.callbacks_mut().text);
        self.history.extend(crlf(&rows));
        self.trim();
    }

    /// Bounds the history to [`HISTORY`] bytes.
    fn trim(&mut self) {
        if self.history.len() > HISTORY {
            self.history.drain(..self.history.len() - HISTORY);
            // A replay starting mid-line would show a fragment as a whole line.
            // The search is bounded: output redrawn with `\r` alone has no line
            // ends, and scanning all of it for every chunk would add up.
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

    /// Resizes the grid, as the PTY was.
    pub(crate) fn resize(&mut self, size: TerminalSize) {
        if self.rendered() {
            self.resize_rendered(size);
        } else {
            self.parser
                .screen_mut()
                .set_size(cells(size.rows), cells(size.columns));
        }
    }

    /// Resizes a rendered grid. Rows a shorter grid pushes off the top join the
    /// history. A taller grid gets its new rows at the bottom, where the
    /// console host adds them: vt100 would otherwise add one blank row at the
    /// top for every row that ever left the grid, as a terminal pulls rows back
    /// from its scrollback.
    fn resize_rendered(&mut self, size: TerminalSize) {
        // The scrollback holds no rows; this only forgets how many left.
        self.parser.screen_mut().clear_scrollback();
        self.parser.set_size(cells(size.rows), cells(size.columns));
        self.take_rows();
    }

    /// The grid's size.
    fn size(&self) -> TerminalSize {
        let (rows, columns) = self.parser.screen().size();
        TerminalSize {
            columns: columns.get(),
            rows: rows.get(),
        }
    }

    /// The model as the program drew it.
    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Clear, home and the screen as a viewer of `size` should see it (the
    /// grid's size when `None`), plus the program's input modes.
    pub(crate) fn snapshot(&self, size: Option<TerminalSize>) -> Bytes {
        let screen = self.parser.screen();
        let mut bytes = b"\x1b[H\x1b[2J".to_vec();
        if screen.alternate_screen() {
            bytes.extend_from_slice(screen.contents_formatted().as_bytes());
        } else {
            let size = size.unwrap_or_else(|| self.size());
            let mut replay = vt100::Parser::new(cells(size.rows), cells(size.columns), 0);
            let (front, back) = self.history.as_slices();
            replay.process(front);
            replay.process(back);
            bytes.extend_from_slice(replay.screen().contents_formatted().as_bytes());
            // The sequence that hid the cursor may be older than the history.
            bytes.extend_from_slice(if screen.hide_cursor() {
                b"\x1b[?25l"
            } else {
                b"\x1b[?25h"
            });
        }
        bytes.extend_from_slice(screen.input_mode_formatted().as_bytes());
        Bytes::from(bytes)
    }

    /// What a redraw of rendered output for a viewer of `size` (the grid's
    /// size when `None`) needs: a copy, so that the replay can run after the
    /// screen lock is released.
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

    /// A rendered grid's rows as text lines that continue the history, for a
    /// viewer of `size`. Blank rows under the cursor are left out: a console
    /// host's repaint after a resize leaves them, and replayed they would push
    /// lines off the top. A viewer as wide as the grid wraps the cursor's line
    /// as the grid did, so the cursor keeps its column there; for any other
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

/// A rendered screen copied for one redraw (see [`Screen::frame`]).
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

    /// Lines printed at 120 columns and shown after a resize to 200x30 sit one
    /// per row and fill the rows that were empty, as a fresh 200-column
    /// terminal would show them.
    #[test]
    fn a_resize_shows_lines_rewrapped_at_the_new_size() {
        let mut screen = Screen::new(TerminalSize::default(), false);
        for i in 0..60 {
            screen.process(format!("{i:03} {}\r\n", "x".repeat(176)).as_bytes());
        }
        let size = TerminalSize {
            columns: 200,
            rows: 30,
        };
        screen.resize(size);
        let shown = draw(&screen.snapshot(None), 200, 30);
        let rows: Vec<String> = shown.screen().rows(0, 200).collect();
        // The last row holds the cursor after the final newline.
        assert_eq!(rows[29], "");
        for (row, i) in rows[..29].iter().zip(31..) {
            assert_eq!(row, &format!("{i:03} {}", "x".repeat(176)));
        }
    }

    /// An observer of another size gets the output at its own size.
    #[test]
    fn a_snapshot_uses_the_viewer_size() {
        let mut screen = Screen::new(TerminalSize::default(), false);
        screen.process(format!("{}\r\nnext\r\n", "y".repeat(150)).as_bytes());
        let size = TerminalSize {
            columns: 160,
            rows: 10,
        };
        let shown = draw(&screen.snapshot(Some(size)), 160, 10);
        assert_eq!(
            shown.screen().contents(),
            format!("{}\nnext", "y".repeat(150))
        );
    }

    /// The alternate screen is sent as the program drew it.
    #[test]
    fn the_alternate_screen_is_sent_as_drawn() {
        let mut screen = Screen::new(
            TerminalSize {
                columns: 40,
                rows: 5,
            },
            false,
        );
        screen.process(b"before\r\n\x1b[?1049h\x1b[Hmenu");
        let shown = draw(&screen.snapshot(None), 40, 5);
        assert_eq!(shown.screen().contents(), "menu");
    }

    /// Trimmed history starts at a line, never in the middle of one.
    #[test]
    fn history_is_trimmed_at_a_line_start() {
        let mut screen = Screen::new(TerminalSize::default(), false);
        let line = format!("{}\r\n", "z".repeat(998));
        for _ in 0..(HISTORY / line.len() + 10) {
            screen.process(line.as_bytes());
        }
        assert!(screen.history.len() <= HISTORY);
        assert_eq!(screen.history.front(), Some(&b'z'));
        assert_eq!(screen.history.len() % line.len(), 0);
    }

    /// Line `i` of the rendered tests: 50 columns, wider than their grid.
    fn line(i: usize) -> String {
        format!("L{i:02} {}", "x".repeat(46))
    }

    /// A 40x8 rendered screen fed lines 1 to 20 the way Windows' inbox console
    /// host sends them: the first screen as written, then, at the bottom, the
    /// first 40 columns, a line break, and the rest written from the last
    /// column of the row above, so the terminal marks that row as wrapped.
    fn rendered() -> Screen {
        let mut screen = Screen::new(
            TerminalSize {
                columns: 40,
                rows: 8,
            },
            true,
        );
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

    /// Rendered output is shown with whole lines at the viewer's size, not
    /// scattered by cursor moves meant for the console's size.
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
    /// leaves blank rows under the cursor; the snapshot still fills the screen
    /// from the lines above, and the line cut by the window's top edge is
    /// joined to its start.
    #[test]
    fn a_rendered_repaint_after_a_resize_fills_the_screen() {
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
    /// of a prompt stays after it.
    #[test]
    fn a_rendered_prompt_keeps_its_cursor() {
        let mut screen = Screen::new(
            TerminalSize {
                columns: 40,
                rows: 4,
            },
            true,
        );
        // The console host sends a prompt's trailing space as a cursor move.
        screen.process(b"one\r\ntwo\r\n>\x1b[3;3H");
        let trimmed = |shown: &vt100::Parser, columns| -> Vec<String> {
            shown
                .screen()
                .rows(0, columns)
                .map(|row| row.trim_end().to_owned())
                .collect()
        };
        let shown = draw(&redraw(&screen, None), 40, 4);
        assert_eq!(trimmed(&shown, 40), ["one", "two", ">", ""]);
        assert_eq!(shown.screen().cursor_position(), (2, 2));
        // A viewer of another width keeps the space after the prompt.
        let size = TerminalSize {
            columns: 30,
            rows: 4,
        };
        let shown = draw(&redraw(&screen, Some(size)), 30, 4);
        assert_eq!(trimmed(&shown, 30), ["one", "two", ">", ""]);
        assert_eq!(shown.screen().cursor_position(), (2, 2));
    }

    /// A rendered screen's redraw for a viewer of `size`, from scratch.
    fn redraw(screen: &Screen, size: Option<TerminalSize>) -> Bytes {
        screen.frame(size).render(None).0
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

    /// A redraw after another one changes only what differs, and leaves the
    /// viewer with the same screen as a whole redraw would.
    #[test]
    fn a_rendered_redraw_after_another_draws_the_difference() {
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
    fn a_rendered_clear_of_the_scrollback_drops_the_history() {
        let size = TerminalSize {
            columns: 40,
            rows: 4,
        };
        let mut screen = Screen::new(size, true);
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

    /// A taller rendered grid gets its new rows at the bottom, as the console
    /// host's own buffer does, so its repaint lands on the same rows.
    #[test]
    fn a_taller_rendered_grid_grows_at_the_bottom() {
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
}
