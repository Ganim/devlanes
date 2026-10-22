//! A unit's screen model and the recent output it was built from.
//!
//! A terminal grid keeps every line wrapped at the width it was printed at, so
//! after a resize, or for a viewer of another size, the grid alone shows long
//! lines cut at the old width and leaves extra rows empty. Programs on the
//! main screen (dev servers, loggers, pipe units) get their snapshot by
//! replaying the recent output at the viewer's size instead, the way a fresh
//! terminal of that size would have drawn it. Programs on the alternate screen
//! (editors, pagers) redraw themselves after a resize, so their grid is sent.

use std::{collections::VecDeque, num::NonZeroU16};

use bytes::Bytes;
use devlanes_core::TerminalSize;

/// Rows kept above the grid; deep history comes from log segments.
const SCROLLBACK: usize = 200;
/// Recent output kept for replay: enough for several screens of colored log
/// lines at any width.
const HISTORY: usize = 128 * 1024;
/// How far into trimmed history a line start is looked for.
const LINE_SEARCH: usize = 16 * 1024;

/// The screen model plus the output it was fed, for replay at other sizes.
pub(crate) struct Screen {
    parser: vt100::Parser,
    /// The last [`HISTORY`] bytes fed to `parser`, starting at a line start
    /// once trimmed.
    history: VecDeque<u8>,
}

impl Screen {
    /// An empty screen of `size`.
    pub(crate) fn new(size: TerminalSize) -> Self {
        Self {
            parser: vt100::Parser::new(cells(size.rows), cells(size.columns), SCROLLBACK),
            history: VecDeque::new(),
        }
    }

    /// Feeds output to the model and the replay history.
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.history.extend(bytes);
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
        self.parser
            .screen_mut()
            .set_size(cells(size.rows), cells(size.columns));
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
        let mut screen = Screen::new(TerminalSize::default());
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
        let mut screen = Screen::new(TerminalSize::default());
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
        let mut screen = Screen::new(TerminalSize {
            columns: 40,
            rows: 5,
        });
        screen.process(b"before\r\n\x1b[?1049h\x1b[Hmenu");
        let shown = draw(&screen.snapshot(None), 40, 5);
        assert_eq!(shown.screen().contents(), "menu");
    }

    /// Trimmed history starts at a line, never in the middle of one.
    #[test]
    fn history_is_trimmed_at_a_line_start() {
        let mut screen = Screen::new(TerminalSize::default());
        let line = format!("{}\r\n", "z".repeat(998));
        for _ in 0..(HISTORY / line.len() + 10) {
            screen.process(line.as_bytes());
        }
        assert!(screen.history.len() <= HISTORY);
        assert_eq!(screen.history.front(), Some(&b'z'));
        assert_eq!(screen.history.len() % line.len(), 0);
    }
}
