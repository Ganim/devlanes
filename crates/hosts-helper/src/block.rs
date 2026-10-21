//! The marker-delimited block devlanes owns inside `/etc/hosts`.
//!
//! The file is handled as bytes because hosts files can carry comments in any
//! encoding; every byte outside the block is copied unchanged. The only byte ever
//! added outside the block is one line break before a new block when the file does
//! not end with one.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

/// First line of the block.
pub const BEGIN_MARKER: &str = "# devlanes begin";

/// Last line of the block.
pub const END_MARKER: &str = "# devlanes end";

/// Second line of the block, telling people who open the file who owns it.
pub const NOTE: &str =
    "# Managed by devlanes-hosts. Lines between these markers are replaced on every update.";

/// Addresses every name maps to. IPv4 first so clients that try addresses in file
/// order reach dev servers bound only to 127.0.0.1 without a refused `::1` attempt.
pub const ADDRESSES: [&str; 2] = ["127.0.0.1", "::1"];

/// The markers do not delimit exactly one block, so the helper refuses to guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// The same marker appears on more than one line.
    DuplicateMarker {
        /// The repeated marker.
        marker: &'static str,
        /// 1-based line numbers where it appears.
        lines: Vec<usize>,
    },
    /// The end marker comes before the begin marker.
    OutOfOrder {
        /// Line of the begin marker.
        begin: usize,
        /// Line of the end marker.
        end: usize,
    },
    /// Only one of the two markers is present.
    Unpaired {
        /// The marker that is present.
        marker: &'static str,
        /// Its line.
        line: usize,
    },
}

impl fmt::Display for BlockError {
    /// Message pointing at the lines to fix by hand.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateMarker { marker, lines } => {
                let lines: Vec<String> = lines.iter().map(ToString::to_string).collect();
                write!(
                    f,
                    "`{marker}` appears on lines {}; remove the extra devlanes block by hand",
                    lines.join(", ")
                )
            }
            Self::OutOfOrder { begin, end } => write!(
                f,
                "`{END_MARKER}` (line {end}) comes before `{BEGIN_MARKER}` (line {begin}); fix the markers by hand"
            ),
            Self::Unpaired { marker, line } => write!(
                f,
                "`{marker}` on line {line} has no matching marker; fix or remove it by hand"
            ),
        }
    }
}

impl std::error::Error for BlockError {}

/// One line of the file as byte offsets.
#[derive(Debug, Clone, Copy)]
struct Line {
    /// Offset of the first byte.
    start: usize,
    /// Offset after the content, before `\r\n` or `\n`.
    content_end: usize,
    /// Offset after the terminator, or the file length for an unterminated last line.
    end: usize,
}

/// Splits `content` into lines, keeping offsets so slices can be copied verbatim.
fn lines(content: &[u8]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < content.len() {
        let (content_end, end) = match content[start..].iter().position(|&b| b == b'\n') {
            Some(offset) => {
                let newline = start + offset;
                let content_end = if newline > start && content[newline - 1] == b'\r' {
                    newline - 1
                } else {
                    newline
                };
                (content_end, newline + 1)
            }
            None => (content.len(), content.len()),
        };
        out.push(Line {
            start,
            content_end,
            end,
        });
        start = end;
    }
    out
}

/// Whether a line is `marker`, ignoring trailing spaces and tabs left by editors.
fn is_marker(line: &[u8], marker: &str) -> bool {
    let trimmed = line
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(&line[..0], |last| &line[..=last]);
    trimmed == marker.as_bytes()
}

/// The located block: its byte range, including the end marker's line break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// Bytes from the start of the begin line to the end of the end line.
    pub range: Range<usize>,
    /// Whether the begin line ends with `\r\n`.
    pub crlf: bool,
}

/// Finds the block; `Ok(None)` when neither marker is present.
pub fn find(content: &[u8]) -> Result<Option<Located>, BlockError> {
    let all = lines(content);
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    for (index, line) in all.iter().enumerate() {
        let text = &content[line.start..line.content_end];
        if is_marker(text, BEGIN_MARKER) {
            begins.push(index);
        } else if is_marker(text, END_MARKER) {
            ends.push(index);
        }
    }
    let numbers = |indexes: &[usize]| indexes.iter().map(|index| index + 1).collect();
    if begins.len() > 1 {
        return Err(BlockError::DuplicateMarker {
            marker: BEGIN_MARKER,
            lines: numbers(&begins),
        });
    }
    if ends.len() > 1 {
        return Err(BlockError::DuplicateMarker {
            marker: END_MARKER,
            lines: numbers(&ends),
        });
    }
    match (begins.first(), ends.first()) {
        (None, None) => Ok(None),
        (Some(&begin), None) => Err(BlockError::Unpaired {
            marker: BEGIN_MARKER,
            line: begin + 1,
        }),
        (None, Some(&end)) => Err(BlockError::Unpaired {
            marker: END_MARKER,
            line: end + 1,
        }),
        (Some(&begin), Some(&end)) if end < begin => Err(BlockError::OutOfOrder {
            begin: begin + 1,
            end: end + 1,
        }),
        (Some(&begin), Some(&end)) => {
            let begin_line = all[begin];
            Ok(Some(Located {
                range: begin_line.start..all[end].end,
                crlf: begin_line.end - begin_line.content_end == 2,
            }))
        }
    }
}

/// Whether the file's first line break is `\r\n`; files without one count as LF.
fn file_uses_crlf(content: &[u8]) -> bool {
    lines(content)
        .first()
        .is_some_and(|line| line.end - line.content_end == 2)
}

/// Renders the block for `names`, every line terminated by `newline`.
pub fn render(names: &BTreeSet<String>, newline: &str) -> Vec<u8> {
    let mut out = String::new();
    for line in [BEGIN_MARKER, NOTE] {
        out.push_str(line);
        out.push_str(newline);
    }
    for name in names {
        for address in ADDRESSES {
            out.push_str(address);
            out.push(' ');
            out.push_str(name);
            out.push_str(newline);
        }
    }
    out.push_str(END_MARKER);
    out.push_str(newline);
    out.into_bytes()
}

/// Returns `content` with the block set to `names`; an empty set removes the block.
///
/// An existing block is replaced in place and keeps its line-break style. A new block
/// is appended using the file's style.
pub fn rewrite(content: &[u8], names: &BTreeSet<String>) -> Result<Vec<u8>, BlockError> {
    let located = find(content)?;
    let crlf = located
        .as_ref()
        .map_or_else(|| file_uses_crlf(content), |block| block.crlf);
    let newline = if crlf { "\r\n" } else { "\n" };
    let block = if names.is_empty() {
        Vec::new()
    } else {
        render(names, newline)
    };
    let mut out = Vec::with_capacity(content.len() + block.len() + 2);
    match located {
        Some(Located { range, .. }) => {
            out.extend_from_slice(&content[..range.start]);
            out.extend_from_slice(&block);
            out.extend_from_slice(&content[range.end..]);
        }
        None => {
            out.extend_from_slice(content);
            if !block.is_empty() {
                if !content.is_empty() && !content.ends_with(b"\n") {
                    out.extend_from_slice(newline.as_bytes());
                }
                out.extend_from_slice(&block);
            }
        }
    }
    Ok(out)
}

/// The block's bytes, markers included; `Ok(None)` when absent.
pub fn current(content: &[u8]) -> Result<Option<&[u8]>, BlockError> {
    Ok(find(content)?.map(|block| &content[block.range]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn rewrite_str(content: &str, list: &[&str]) -> String {
        String::from_utf8(rewrite(content.as_bytes(), &names(list)).expect("rewrite"))
            .expect("utf-8")
    }

    const STOCK: &str = "##\n# Host Database\n##\n127.0.0.1\tlocalhost\n255.255.255.255\tbroadcasthost\n::1             localhost\n";

    fn block(lines: &[&str]) -> String {
        let mut out = format!("{BEGIN_MARKER}\n{NOTE}\n");
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(END_MARKER);
        out.push('\n');
        out
    }

    #[test]
    fn appends_a_missing_block() {
        let out = rewrite_str(STOCK, &["b.localhost", "a.localhost"]);
        assert_eq!(
            out,
            format!(
                "{STOCK}{}",
                block(&[
                    "127.0.0.1 a.localhost",
                    "::1 a.localhost",
                    "127.0.0.1 b.localhost",
                    "::1 b.localhost",
                ])
            )
        );
    }

    #[test]
    fn adds_a_line_break_only_when_missing() {
        let out = rewrite_str("127.0.0.1 localhost", &["a.localhost"]);
        assert!(out.starts_with(&format!("127.0.0.1 localhost\n{BEGIN_MARKER}\n")));
        assert_eq!(
            rewrite_str("", &["a.localhost"]),
            block(&["127.0.0.1 a.localhost", "::1 a.localhost"])
        );
    }

    #[test]
    fn replaces_an_existing_block_in_place() {
        let before = "# top\n127.0.0.1 localhost\n";
        let after = "# user line after\n10.0.0.1 nas.lan\n";
        let original = format!("{before}{}{after}", block(&["127.0.0.1 old.localhost"]));
        let out = rewrite_str(&original, &["new.localhost"]);
        assert_eq!(
            out,
            format!(
                "{before}{}{after}",
                block(&["127.0.0.1 new.localhost", "::1 new.localhost"])
            )
        );
    }

    #[test]
    fn other_bytes_survive_verbatim() {
        // Latin-1 comment, tabs, trailing spaces, a blank line, no final line break.
        let mut prefix = b"# caf\xe9 \t \n\n127.0.0.1\tlocalhost   \n".to_vec();
        let suffix = b"\n# after\t\n::1 localhost".to_vec();
        let mut original = prefix.clone();
        original.extend_from_slice(block(&["127.0.0.1 x.localhost"]).as_bytes());
        original.extend_from_slice(&suffix);
        let out = rewrite(&original, &names(&["y.localhost"])).expect("rewrite");
        assert!(out.starts_with(&prefix));
        assert!(out.ends_with(&suffix));
        let cleared = rewrite(&original, &BTreeSet::new()).expect("clear");
        prefix.extend_from_slice(&suffix);
        assert_eq!(cleared, prefix);
    }

    #[test]
    fn empty_set_removes_the_block_and_is_a_no_op_without_one() {
        let original = format!("{STOCK}{}", block(&["127.0.0.1 a.localhost"]));
        assert_eq!(rewrite_str(&original, &[]), STOCK);
        assert_eq!(rewrite_str(STOCK, &[]), STOCK);
        assert_eq!(rewrite_str("no newline", &[]), "no newline");
    }

    #[test]
    fn apply_then_clear_restores_the_original() {
        for original in [STOCK, "", "no newline at end", "crlf\r\nfile\r\n"] {
            let applied = rewrite(original.as_bytes(), &names(&["a.localhost"])).expect("apply");
            let cleared = rewrite(&applied, &BTreeSet::new()).expect("clear");
            let expected = if original.is_empty() || original.ends_with('\n') {
                original.to_owned()
            } else {
                format!("{original}\n")
            };
            assert_eq!(String::from_utf8(cleared).expect("utf-8"), expected);
            let reapplied = rewrite(&applied, &names(&["a.localhost"])).expect("reapply");
            assert_eq!(reapplied, applied, "applying twice is stable");
        }
    }

    #[test]
    fn crlf_files_get_crlf_blocks() {
        let original = "127.0.0.1 localhost\r\n::1 localhost\r\n";
        let out = rewrite_str(original, &["a.localhost"]);
        assert_eq!(
            out,
            format!(
                "{original}{BEGIN_MARKER}\r\n{NOTE}\r\n127.0.0.1 a.localhost\r\n::1 a.localhost\r\n{END_MARKER}\r\n"
            )
        );
        let replaced = rewrite_str(&out, &["b.localhost"]);
        assert_eq!(replaced, out.replace("a.localhost", "b.localhost"));
        let unterminated = rewrite_str("x\r\ny", &["a.localhost"]);
        assert!(unterminated.starts_with(&format!("x\r\ny\r\n{BEGIN_MARKER}\r\n")));
    }

    #[test]
    fn end_marker_at_eof_without_line_break() {
        let original = format!("x\n{BEGIN_MARKER}\n127.0.0.1 a.localhost\n{END_MARKER}");
        assert_eq!(rewrite_str(&original, &[]), "x\n");
        assert_eq!(
            rewrite_str(&original, &["b.localhost"]),
            format!(
                "x\n{}",
                block(&["127.0.0.1 b.localhost", "::1 b.localhost"])
            )
        );
    }

    #[test]
    fn markers_with_trailing_whitespace_still_match() {
        let original = format!("{BEGIN_MARKER} \t\n127.0.0.1 a.localhost\n{END_MARKER}  \n");
        assert_eq!(rewrite_str(&original, &[]), "");
        // Leading whitespace or other text makes it an ordinary comment.
        let not_markers = format!(" {BEGIN_MARKER}\n{END_MARKER}s\n");
        assert_eq!(find(not_markers.as_bytes()), Ok(None));
    }

    #[test]
    fn malformed_markers_are_refused() {
        let twice = format!("{}{}", block(&[]), block(&[]));
        assert_eq!(
            find(twice.as_bytes()),
            Err(BlockError::DuplicateMarker {
                marker: BEGIN_MARKER,
                lines: vec![1, 4]
            })
        );
        let two_ends = format!("{BEGIN_MARKER}\n{END_MARKER}\n{END_MARKER}\n");
        assert!(matches!(
            find(two_ends.as_bytes()),
            Err(BlockError::DuplicateMarker {
                marker: END_MARKER,
                ..
            })
        ));
        let reversed = format!("{END_MARKER}\n{BEGIN_MARKER}\n");
        assert_eq!(
            find(reversed.as_bytes()),
            Err(BlockError::OutOfOrder { begin: 2, end: 1 })
        );
        let lonely = format!("x\n{BEGIN_MARKER}\n");
        assert_eq!(
            find(lonely.as_bytes()),
            Err(BlockError::Unpaired {
                marker: BEGIN_MARKER,
                line: 2
            })
        );
        let lonely_end = format!("{END_MARKER}\n");
        assert!(matches!(
            rewrite(lonely_end.as_bytes(), &names(&["a.localhost"])),
            Err(BlockError::Unpaired {
                marker: END_MARKER,
                line: 1
            })
        ));
    }

    #[test]
    fn current_returns_the_block_bytes() {
        let original = format!("x\n{}y\n", block(&["127.0.0.1 a.localhost"]));
        assert_eq!(
            current(original.as_bytes()).expect("block"),
            Some(block(&["127.0.0.1 a.localhost"]).as_bytes())
        );
        assert_eq!(current(STOCK.as_bytes()), Ok(None));
    }
}
