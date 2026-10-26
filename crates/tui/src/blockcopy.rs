//! Copy mode over blocks of drawn lines, shared by the daemon page's log (a
//! block per shown entry) and its checks box (a block per check), so both
//! move, mark and copy the same way.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;

use crate::app::LogCopy;
use crate::daemonlog::{LogPos, LogView};

/// Blocks of drawn lines that copy mode moves over. Lines are drawn on demand:
/// the log keeps up to 2000 entries and a key only moves through a few.
pub struct Blocks<'a> {
    pub count: usize,
    pub lines: &'a dyn Fn(usize) -> Vec<Line<'static>>,
    /// The box's inside width, the last column the cursor reaches.
    pub width: usize,
}

/// What a copy-mode key asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The cursor or the mark moved, or the key does nothing here.
    Moved(LogCopy),
    /// `y` or Enter: the selection as drawn.
    Copy(String),
    /// `Y`: the whole blocks from the first index to the last.
    Whole(usize, usize),
    /// Esc, `c` or `q` left copy mode.
    Leave,
}

impl Blocks<'_> {
    /// One drawn line down, across blocks; `None` at the last line.
    fn down(&self, pos: LogPos) -> Option<LogPos> {
        if pos.line + 1 < (self.lines)(pos.entry).len() {
            return Some(LogPos {
                line: pos.line + 1,
                ..pos
            });
        }
        (pos.entry + 1 < self.count).then_some(LogPos {
            entry: pos.entry + 1,
            line: 0,
            ..pos
        })
    }

    /// One drawn line up, across blocks; `None` at the first line.
    fn up(&self, pos: LogPos) -> Option<LogPos> {
        if pos.line > 0 {
            return Some(LogPos {
                line: pos.line - 1,
                ..pos
            });
        }
        let entry = pos.entry.checked_sub(1)?;
        Some(LogPos {
            entry,
            line: (self.lines)(entry).len().saturating_sub(1),
            ..pos
        })
    }

    /// `times` lines down or up from `pos`, stopping on the last line there
    /// is when the blocks end first.
    fn step(&self, pos: LogPos, times: usize, down: bool) -> LogPos {
        let by = |at: LogPos| if down { self.down(at) } else { self.up(at) };
        (0..times).try_fold(pos, |at, _| by(at)).unwrap_or_else(|| {
            let mut at = pos;
            while let Some(next) = by(at) {
                at = next;
            }
            at
        })
    }

    /// Applies a copy-mode key, as in a unit's pane: `h j k l` move a cell
    /// cursor, `v` marks, `y` copies the text as drawn, `Y` the whole blocks
    /// it touches, Esc leaves. `page` is how far PageUp and PageDown move.
    pub fn key(&self, mut copy: LogCopy, key: KeyEvent, page: usize) -> Outcome {
        let last_column = self.width.saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => copy.cursor = self.step(copy.cursor, 1, true),
            KeyCode::Char('k') | KeyCode::Up => copy.cursor = self.step(copy.cursor, 1, false),
            KeyCode::PageDown => copy.cursor = self.step(copy.cursor, page, true),
            KeyCode::PageUp => copy.cursor = self.step(copy.cursor, page, false),
            KeyCode::Char('h') | KeyCode::Left => {
                copy.cursor.column = copy.cursor.column.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                copy.cursor.column = (copy.cursor.column + 1).min(last_column);
            }
            KeyCode::Char('0') | KeyCode::Home => copy.cursor.column = 0,
            KeyCode::Char('$') | KeyCode::End => copy.cursor.column = last_column,
            KeyCode::Char('g') => {
                copy.cursor = LogPos {
                    column: copy.cursor.column,
                    ..LogPos::default()
                };
            }
            KeyCode::Char('G') => {
                let entry = self.count.saturating_sub(1);
                copy.cursor = LogPos {
                    entry,
                    line: (self.lines)(entry).len().saturating_sub(1),
                    column: copy.cursor.column,
                };
            }
            KeyCode::Char('v') | KeyCode::Char(' ') => {
                copy.anchor = match copy.anchor {
                    Some(_) => None,
                    None => Some(copy.cursor),
                };
            }
            KeyCode::Char('Y') => {
                let (first, last) = copy.copied();
                return Outcome::Whole(first.entry, last.entry);
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let (first, last) = copy.copied();
                return Outcome::Copy(self.text(first, last));
            }
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => return Outcome::Leave,
            _ => {}
        }
        Outcome::Moved(copy)
    }

    /// The drawn text from `first` to `last`, both cells included, one line
    /// per drawn row without trailing spaces.
    pub fn text(&self, first: LogPos, last: LogPos) -> String {
        let mut text: Vec<String> = Vec::new();
        for index in first.entry..=last.entry.min(self.count.saturating_sub(1)) {
            for (line, drawn) in (self.lines)(index).iter().enumerate() {
                let start = LogPos {
                    entry: index,
                    line,
                    column: 0,
                };
                let end = LogPos {
                    column: usize::MAX,
                    ..start
                };
                if end < first || start > last {
                    continue;
                }
                let from = if start < first { first.column } else { 0 };
                let to = if (LogPos { column: 0, ..last }) == start {
                    last.column
                } else {
                    usize::MAX
                };
                let plain = crate::ui::daemon::plain(drawn);
                let part: String = plain
                    .chars()
                    .skip(from)
                    .take(to.saturating_sub(from).saturating_add(1))
                    .collect();
                text.push(part.trim_end().to_owned());
            }
        }
        text.join("\n")
    }
}

/// Whether screen `(column, row)` is inside the box `view` last drew.
pub fn contains(view: LogView, column: u16, row: u16) -> bool {
    let (column, row) = (usize::from(column), usize::from(row));
    let (x, y) = (usize::from(view.x), usize::from(view.y));
    view.height > 0 && (x..x + view.width).contains(&column) && (y..y + view.height).contains(&row)
}

/// The cell at screen `(column, row)` of a box drawn at `view` whose rows show
/// `visible`, top to bottom. Outside the rows shown it is `None`, or with
/// `clamp` the nearest cell, so a drag past the box's edge selects up to its
/// first or last line.
pub fn cell(
    view: LogView,
    visible: &[LogPos],
    column: u16,
    row: u16,
    clamp: bool,
) -> Option<LogPos> {
    if view.height == 0 {
        return None;
    }
    let last = visible.len().checked_sub(1)?;
    let dy = i32::from(row) - i32::from(view.y);
    let dx = i32::from(column) - i32::from(view.x);
    let line = usize::try_from(dy).ok().filter(|line| *line <= last);
    let cell = usize::try_from(dx).ok().filter(|cell| *cell < view.width);
    if !clamp && (line.is_none() || cell.is_none()) {
        return None;
    }
    let line = line.unwrap_or(if dy < 0 { 0 } else { last });
    let cell = cell.unwrap_or(if dx < 0 {
        0
    } else {
        view.width.saturating_sub(1)
    });
    Some(LogPos {
        column: cell,
        ..visible[line]
    })
}
