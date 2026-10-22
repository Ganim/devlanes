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
//! that left the top of the grid as text lines instead, and a snapshot replays
//! those plus the grid's own rows, joined where a row wrapped.

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

/// The screen model plus the output it was fed, for replay at other sizes.
pub(crate) struct Screen {
    parser: vt100::Parser<Rows>,
    /// The last [`HISTORY`] bytes fed to `parser`, or for rendered output the
    /// rows that left the grid as text lines, starting at a line start once
    /// trimmed.
    history: VecDeque<u8>,
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
        // Rendered output has no scrollback in the grid: every row that leaves
        // it goes to the history, and growing the grid never pulls rows back
        // down for the console host's repaint to overwrite.
        let scrollback = if rendered { 0 } else { SCROLLBACK };
        Self {
            parser: vt100::Parser::new_with_callbacks(
                cells(size.rows),
                cells(size.columns),
                scrollback,
                rows,
            ),
            history: VecDeque::new(),
        }
    }

    /// Whether the output renders a screen.
    fn rendered(&self) -> bool {
        self.parser.callbacks().enabled
    }

    /// Feeds output to the model and the replay history.
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        if self.rendered() {
            self.take_rows();
        } else {
            self.history.extend(bytes);
            self.trim();
        }
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

    /// Resizes the grid, as the PTY was. Rows a shorter grid pushes off the top
    /// join a rendered screen's history.
    pub(crate) fn resize(&mut self, size: TerminalSize) {
        self.parser.set_size(cells(size.rows), cells(size.columns));
        if self.rendered() {
            self.take_rows();
        }
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
            if self.rendered() {
                replay.process(&self.grid_lines(size));
            }
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

    /// A rendered grid's rows as text lines that continue the history, for a
    /// viewer of `size`. Blank rows under the cursor are left out: a console
    /// host's repaint after a resize leaves them, and replayed they would push
    /// lines off the top. A viewer as wide as the grid wraps the cursor's line
    /// as the grid did, so the cursor keeps its column there.
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
        if blank >= below && size.columns == grid.columns {
            bytes.extend_from_slice(format!("\x1b[{}G", column + 1).as_bytes());
        }
        bytes
    }
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
        let shown = draw(&screen.snapshot(Some(size)), 60, 10);
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
        let shown = draw(&screen.snapshot(None), 60, 8);
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
        screen.process(b"one\r\ntwo\r\n> \x1b[3;3H");
        let shown = draw(&screen.snapshot(None), 40, 4);
        let rows: Vec<String> = shown
            .screen()
            .rows(0, 40)
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert_eq!(rows, ["one", "two", ">", ""]);
        assert_eq!(shown.screen().cursor_position(), (2, 2));
    }
}
