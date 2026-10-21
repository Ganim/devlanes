//! Text that comes from the daemon (names, paths, URLs, stderr) is cleaned and
//! fitted to its widget here. Raw control bytes or escape sequences in a buffer
//! cell move the real terminal's cursor and break the layout, so every dynamic
//! string goes through [`clean`] or [`clean_lines`] before it is drawn.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One line: ANSI sequences removed, every control character (newlines and
/// tabs included) turned into a space, runs of spaces kept as they are.
pub fn clean(text: &str) -> String {
    // The stripper drops tabs as control bytes; they are word gaps here.
    strip_ansi_escapes::strip_str(text.replace('\t', " "))
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Lines split on newlines, each cleaned; tabs become four spaces so indented
/// stderr keeps its shape. Trailing empty lines are dropped.
pub fn clean_lines(text: &str) -> Vec<String> {
    let stripped = strip_ansi_escapes::strip_str(text.replace('\t', "    "));
    let mut lines: Vec<String> = stripped
        .split('\n')
        .map(|line| line.chars().filter(|c| !c.is_control()).collect::<String>())
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// Display width in terminal cells.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Fits cleaned text into `max` cells, ending in `…` when it had to be cut.
pub fn clip(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        // One cell stays free for the ellipsis.
        if used + w > max - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Word-wraps cleaned text to `max` cells; words longer than a line are split.
/// Leading spaces (stderr indentation) are kept on every wrapped line while
/// they leave room for text.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let indent = text.len() - text.trim_start_matches(' ').len();
    if indent > 0 && indent < max / 2 {
        let pad = " ".repeat(indent);
        return wrap_words(&text[indent..], max - indent)
            .into_iter()
            .map(|line| format!("{pad}{line}"))
            .collect();
    }
    wrap_words(text.trim_start_matches(' '), max)
}

/// [`wrap`] without indentation handling.
fn wrap_words(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split(' ') {
        let word_width = width(word);
        let gap = usize::from(!line.is_empty());
        if used + gap + word_width <= max {
            if gap == 1 {
                line.push(' ');
            }
            line.push_str(word);
            used += gap + word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for c in word.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > max && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += w;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_removes_escapes_and_controls() {
        // The stripper drops a bare `\r`; newlines and tabs become spaces.
        assert_eq!(clean("\x1b[31mred\x1b[0m\tx\ny\r"), "red x y");
        assert_eq!(clean_lines("a\n\tb\x1b]0;title\x07\r\n\n"), ["a", "    b"]);
    }

    #[test]
    fn clip_respects_wide_characters() {
        assert_eq!(clip("hello", 5), "hello");
        assert_eq!(clip("hello world", 6), "hello…");
        assert_eq!(clip("日本語テキスト", 5), "日本…");
        assert!(width(&clip("日本語テキスト", 5)) <= 5);
        assert_eq!(clip("abc", 0), "");
    }

    #[test]
    fn wrap_breaks_words_and_long_tokens() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        for line in wrap("日本語テキスト and more", 5) {
            assert!(width(&line) <= 5, "{line}");
        }
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(
            wrap("    retry the whole thing", 14),
            ["    retry the", "    whole", "    thing"]
        );
    }
}
