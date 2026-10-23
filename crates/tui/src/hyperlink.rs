//! Clickable service URLs in the TUI (`OSC 8`).
//!
//! ratatui measures a cell by its text, so a link sequence inside a cell
//! would throw its layout off. Instead, after each frame the URLs found on
//! screen are written again in place, wrapped in `OSC 8`, with the colors
//! and attributes of their cells, between a cursor save and restore. The
//! terminal then keeps them clickable until the cells are drawn over.

use std::fmt::Write as _;

use devlanes_term::Links;
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};

/// The bytes that turn every whole occurrence of each of `urls` in `buffer`
/// into a link; empty when none shows.
pub fn relink(buffer: &Buffer, urls: &[String], links: &Links) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for url in urls.iter().filter(|url| is_plain(url)) {
        let chars: Vec<char> = url.chars().collect();
        let len = u16::try_from(chars.len()).unwrap_or(u16::MAX);
        if len == 0 || len > area.width {
            continue;
        }
        for y in area.top()..area.bottom() {
            let mut x = area.left();
            while x + len <= area.right() {
                let found = chars.iter().enumerate().all(|(i, ch)| {
                    let mut buf = [0; 4];
                    buffer[(x + i as u16, y)].symbol() == ch.encode_utf8(&mut buf)
                });
                if !found {
                    x += 1;
                    continue;
                }
                let mut text = String::new();
                for i in 0..len {
                    let cell = &buffer[(x + i, y)];
                    text.push_str(&sgr(cell.fg, cell.bg, cell.modifier));
                    text.push_str(cell.symbol());
                }
                let _ = write!(
                    out,
                    "\x1b[{};{}H{}\x1b[0m",
                    y + 1,
                    x + 1,
                    links.url(&text, url)
                );
                x += len;
            }
        }
    }
    if out.is_empty() {
        return out;
    }
    // DECSC and DECRC: the cursor and the attributes ratatui left stay as they were.
    format!("\x1b7{out}\x1b8")
}

/// One cell per character: URLs are ASCII, and anything else could not be
/// matched cell by cell.
fn is_plain(url: &str) -> bool {
    url.chars().all(|c| c.is_ascii_graphic())
}

/// The SGR sequence for a cell's colors and attributes, from a reset.
fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = vec!["0".to_owned()];
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    codes.extend(color(fg, false));
    codes.extend(color(bg, true));
    format!("\x1b[{}m", codes.join(";"))
}

/// The SGR parameter of `color` as foreground, or background with `bg`.
fn color(color: Color, bg: bool) -> Option<String> {
    let base = |code: u8| Some(if bg { code + 10 } else { code }.to_string());
    match color {
        Color::Reset => None,
        Color::Black => base(30),
        Color::Red => base(31),
        Color::Green => base(32),
        Color::Yellow => base(33),
        Color::Blue => base(34),
        Color::Magenta => base(35),
        Color::Cyan => base(36),
        Color::Gray => base(37),
        Color::DarkGray => base(90),
        Color::LightRed => base(91),
        Color::LightGreen => base(92),
        Color::LightYellow => base(93),
        Color::LightBlue => base(94),
        Color::LightMagenta => base(95),
        Color::LightCyan => base(96),
        Color::White => base(97),
        Color::Indexed(index) => Some(format!("{};5;{index}", if bg { 48 } else { 38 })),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", if bg { 48 } else { 38 })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{layout::Rect, style::Style};

    #[test]
    fn urls_on_screen_are_written_again_as_links() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 2));
        buffer.set_string(
            2,
            1,
            "url http://a.io",
            Style::new().fg(Color::Blue).add_modifier(Modifier::DIM),
        );
        let out = relink(
            &buffer,
            &["http://a.io".into(), "https://gone.io".into()],
            &Links::new(false),
        );
        assert!(
            out.starts_with("\x1b7\x1b[2;7H\x1b]8;;http://a.io\x1b\\"),
            "{out:?}"
        );
        assert!(
            out.contains("\x1b[0;2;34mh"),
            "the cell's own look: {out:?}"
        );
        assert!(out.ends_with("\x1b]8;;\x1b\\\x1b[0m\x1b8"), "{out:?}");
        assert_eq!(out.matches("\x1b]8;;http").count(), 1);
        // Nothing on screen, nothing written.
        assert_eq!(
            relink(&buffer, &["https://gone.io".into()], &Links::new(false)),
            ""
        );
        // A URL cut by the edge of the pane is left alone.
        let mut narrow = Buffer::empty(Rect::new(0, 0, 8, 1));
        narrow.set_string(0, 0, "http://a.io", Style::new());
        assert_eq!(
            relink(&narrow, &["http://a.io".into()], &Links::new(false)),
            ""
        );
    }

    #[test]
    fn cells_keep_their_colors() {
        assert_eq!(
            sgr(Color::Reset, Color::Reset, Modifier::empty()),
            "\x1b[0m"
        );
        assert_eq!(
            sgr(
                Color::Rgb(1, 2, 3),
                Color::Indexed(8),
                Modifier::BOLD | Modifier::UNDERLINED
            ),
            "\x1b[0;1;4;38;2;1;2;3;48;5;8m"
        );
        assert_eq!(
            sgr(Color::LightCyan, Color::Black, Modifier::empty()),
            "\x1b[0;96;40m"
        );
    }
}
