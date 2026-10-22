//! The devlanes banner of the Esc menu: "devlanes" in one of three figlet
//! fonts (DOS Rebel, Coder Mini, Pagga), the first that fits, with the logo
//! (three stacked bars, the top one yellow, each offset to the right like
//! lanes) on its left when that fits too; else the plain word. All of it
//! over a backdrop of lane lines with a doubled center line, as in the
//! website banner.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::theme::{Glyphs, Theme, Tier};

const WORD: &str = "devlanes";

/// DOS Rebel, 75x8.
const LARGE: [&str; 8] = [
    "     █████                      ████",
    "    ░░███                      ░░███",
    "  ███████   ██████  █████ █████ ░███   ██████   ████████    ██████   █████",
    " ███░░███  ███░░███░░███ ░░███  ░███  ░░░░░███ ░░███░░███  ███░░███ ███░░",
    "░███ ░███ ░███████  ░███  ░███  ░███   ███████  ░███ ░███ ░███████ ░░█████",
    "░███ ░███ ░███░░░   ░░███ ███   ░███  ███░░███  ░███ ░███ ░███░░░   ░░░░███",
    "░░████████░░██████   ░░█████    █████░░████████ ████ █████░░██████  ██████",
    " ░░░░░░░░  ░░░░░░     ░░░░░    ░░░░░  ░░░░░░░░ ░░░░ ░░░░░  ░░░░░░  ░░░░░░",
];
/// The bars beside DOS Rebel, on its x-height rows.
const LARGE_LOGO: [&str; 8] = [
    "",
    "",
    "██████████████",
    "",
    "    ██████████████",
    "",
    "        ██████████████",
    "",
];

/// Coder Mini, 44x5.
const MEDIUM: [&str; 5] = [
    "   ▄▄             ▄▄",
    "   ██             ██",
    "▄████ ▄█▀█▄ ██ ██ ██  ▀▀█▄ ████▄ ▄█▀█▄ ▄█▀▀▀",
    "██ ██ ██▄█▀ ██▄██ ██ ▄█▀██ ██ ██ ██▄█▀ ▀███▄",
    "▀████ ▀█▄▄▄  ▀█▀  ██ ▀█▄██ ██ ██ ▀█▄▄▄ ▄▄▄█▀",
];
/// The bars beside Coder Mini, in half blocks.
const MEDIUM_LOGO: [&str; 5] = ["", "", "▗▄▄▄▄▄▖", "  ▗▄▄▄▄▄▖", "    ▗▄▄▄▄▄▖"];

/// Pagga, 32x3.
const SMALL: [&str; 3] = [
    "░█▀▄░█▀▀░█░█░█░░░█▀█░█▀█░█▀▀░█▀▀",
    "░█░█░█▀▀░▀▄▀░█░░░█▀█░█░█░█▀▀░▀▀█",
    "░▀▀░░▀▀▀░░▀░░▀▀▀░▀░▀░▀░▀░▀▀▀░▀▀▀",
];
/// The bars beside Pagga, stepped one column right per row.
const SMALL_LOGO: [&str; 3] = ["▀▀▀▀▀", " ▀▀▀▀▀", "  ▀▀▀▀▀"];

/// The fonts, largest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Large,
    Medium,
    Small,
}

impl Font {
    fn rows(self) -> &'static [&'static str] {
        match self {
            Self::Large => &LARGE,
            Self::Medium => &MEDIUM,
            Self::Small => &SMALL,
        }
    }

    fn logo(self) -> &'static [&'static str] {
        match self {
            Self::Large => &LARGE_LOGO,
            Self::Medium => &MEDIUM_LOGO,
            Self::Small => &SMALL_LOGO,
        }
    }

    /// Columns between the logo and the word.
    fn gap(self) -> u16 {
        match self {
            Self::Large | Self::Medium => 3,
            Self::Small => 1,
        }
    }
}

/// Columns of the widest row.
fn width_of(rows: &[&str]) -> u16 {
    let widest = rows
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    u16::try_from(widest).unwrap_or(u16::MAX)
}

/// How the banner is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// A figlet font, with the logo on its left when `logo`.
    Art { font: Font, logo: bool },
    /// The word in the title gradient, one row.
    Plain,
}

impl Size {
    /// The largest banner that fits `width` x `height` cells with `theme`'s
    /// glyphs: each font with its logo, then without, largest first.
    pub fn pick(theme: &Theme, width: u16, height: u16) -> Self {
        if theme.glyphs == Glyphs::Ascii {
            return Size::Plain;
        }
        [Font::Large, Font::Medium, Font::Small]
            .into_iter()
            .flat_map(|font| [true, false].map(|logo| Size::Art { font, logo }))
            .find(|size| {
                let (w, h) = size.dimensions();
                w <= width && h <= height
            })
            .unwrap_or(Size::Plain)
    }

    /// Columns and rows the banner takes.
    pub fn dimensions(self) -> (u16, u16) {
        match self {
            Size::Art { font, logo } => {
                let word = width_of(font.rows());
                let logo = if logo {
                    width_of(font.logo()) + font.gap()
                } else {
                    0
                };
                let rows = u16::try_from(font.rows().len()).unwrap_or(u16::MAX);
                (logo + word, rows)
            }
            Size::Plain => (8, 1),
        }
    }
}

/// Mixes two true colors; `t` from 0 to 1.
fn mix(from: (u8, u8, u8), to: (u8, u8, u8), t: f64) -> Color {
    let channel = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
    Color::Rgb(
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

/// The yellow-to-orange of the title at `t` from 0 (left) to 1 (right).
pub fn gradient(theme: &Theme, t: f64) -> Style {
    let t = t.clamp(0.0, 1.0);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    match theme.tier {
        Tier::Plain => bold,
        // The 16 colors have no orange: bright yellow, then the darker yellow.
        Tier::Ansi => bold.fg(if t < 0.5 {
            Color::LightYellow
        } else {
            Color::Yellow
        }),
        Tier::Dark => bold.fg(mix((0xf5, 0xc5, 0x42), (0xe8, 0x8a, 0x3c), t)),
        Tier::Light => bold.fg(mix((0x9a, 0x6b, 0x00), (0xb3, 0x5c, 0x00), t)),
    }
}

/// The letters' face color.
fn face(theme: &Theme) -> Style {
    match theme.tier {
        Tier::Plain => Style::new().add_modifier(Modifier::BOLD),
        Tier::Ansi => Style::new().fg(Color::LightYellow),
        Tier::Dark => Style::new().fg(Color::Rgb(0xf5, 0xc5, 0x42)),
        Tier::Light => Style::new().fg(Color::Rgb(0x9a, 0x6b, 0x00)),
    }
}

/// The letters' shade: DOS Rebel's `░`, Coder Mini's half blocks.
fn shade(theme: &Theme) -> Style {
    match theme.tier {
        Tier::Plain => Style::new().add_modifier(Modifier::DIM),
        Tier::Ansi => Style::new().fg(Color::Yellow),
        Tier::Dark => Style::new().fg(Color::Rgb(0xa8, 0x84, 0x2a)),
        Tier::Light => Style::new().fg(Color::Rgb(0xd8, 0xbf, 0x72)),
    }
}

/// Pagga's `░`, a deeper shade behind its small letters.
fn deep_shade(theme: &Theme) -> Style {
    match theme.tier {
        Tier::Plain => Style::new().add_modifier(Modifier::DIM),
        Tier::Ansi => Style::new().fg(Color::DarkGray),
        Tier::Dark => Style::new().fg(Color::Rgb(0x7e, 0x6b, 0x31)),
        Tier::Light => Style::new().fg(Color::Rgb(0xd8, 0xbf, 0x72)),
    }
}

/// A logo bar: the top one yellow, the others gray.
fn bar(theme: &Theme, top: bool) -> Style {
    let color = match (theme.tier, top) {
        (Tier::Plain, _) => return Style::new(),
        (Tier::Ansi, true) => Color::Yellow,
        (Tier::Ansi, false) => Color::Gray,
        (Tier::Dark, true) => Color::Rgb(0xf5, 0xc5, 0x42),
        (Tier::Dark, false) => Color::Rgb(0xc9, 0xc6, 0xbd),
        (Tier::Light, true) => Color::Rgb(0x9a, 0x6b, 0x00),
        (Tier::Light, false) => Color::Rgb(0x6b, 0x67, 0x5e),
    };
    Style::new().fg(color)
}

/// The style of one character of `font`.
fn letter_style(theme: &Theme, font: Font, c: char) -> Style {
    match (font, c) {
        (_, '█') | (Font::Small, '▀' | '▄') => face(theme),
        (Font::Small, '░') => deep_shade(theme),
        _ => shade(theme),
    }
}

/// Draws the banner with its top-left corner at `area`'s; cells outside
/// `area` are left alone.
pub fn render(buf: &mut Buffer, area: Rect, theme: &Theme, size: Size) {
    let mut put = |x: usize, y: usize, c: char, style: Style| {
        let (Ok(dx), Ok(dy)) = (u16::try_from(x), u16::try_from(y)) else {
            return;
        };
        if dx < area.width && dy < area.height {
            buf[(area.x + dx, area.y + dy)].set_char(c).set_style(style);
        }
    };
    let Size::Art { font, logo } = size else {
        for (x, c) in WORD.chars().enumerate() {
            let t = x as f64 / (WORD.len() - 1) as f64;
            put(x, 0, c, gradient(theme, t));
        }
        return;
    };
    let mut left = 0;
    if logo {
        let mut top = true;
        for (y, row) in font.logo().iter().enumerate() {
            if row.is_empty() {
                continue;
            }
            let style = bar(theme, top);
            top = false;
            for (x, c) in row.chars().enumerate().filter(|(_, c)| *c != ' ') {
                put(x, y, c, style);
            }
        }
        left = usize::from(width_of(font.logo()) + font.gap());
    }
    for (y, row) in font.rows().iter().enumerate() {
        for (x, c) in row.chars().enumerate().filter(|(_, c)| *c != ' ') {
            put(left + x, y, c, letter_style(theme, font, c));
        }
    }
}

/// Columns between lane lines.
const LANE_SPACING: u16 = 8;

/// Fills `area` with dashed lane lines every few columns and a doubled
/// yellow line in the middle, leaving `keep` (and a column around it) clear
/// for content drawn on top.
pub fn backdrop(buf: &mut Buffer, area: Rect, theme: &Theme, keep: &[Rect]) {
    let ascii = theme.glyphs == Glyphs::Ascii;
    let center = area.x + area.width / 2;
    let lane = theme.rule();
    let middle = match theme.tier {
        Tier::Plain => Style::new().add_modifier(Modifier::DIM),
        Tier::Ansi => Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
        Tier::Dark => Style::new().fg(Color::Rgb(0x7e, 0x6b, 0x31)),
        Tier::Light => Style::new().fg(Color::Rgb(0xd8, 0xbf, 0x72)),
    };
    let blocked = |x: u16, y: u16| {
        keep.iter()
            .any(|rect| x + 1 >= rect.x && x <= rect.right() && y >= rect.y && y < rect.bottom())
    };
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            // ASCII has no double line, so the middle takes two columns.
            let (symbol, style) = if x == center {
                (if ascii { "|" } else { "║" }, middle)
            } else if ascii && x == center + 1 {
                ("|", middle)
            } else if x.abs_diff(center) % LANE_SPACING == 0 {
                (if ascii { ":" } else { "╎" }, lane)
            } else {
                continue;
            };
            if !blocked(x, y) {
                buf[(x, y)].set_symbol(symbol).set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(glyphs: Glyphs) -> Theme {
        Theme {
            tier: Tier::Ansi,
            glyphs,
        }
    }

    /// The banner of `size` as text, one string per row, trailing spaces cut.
    fn drawn(size: Size) -> Vec<String> {
        let (width, height) = size.dimensions();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme(Glyphs::Unicode), size);
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_owned()
            })
            .collect()
    }

    fn art(font: Font, logo: bool) -> Size {
        Size::Art { font, logo }
    }

    #[test]
    fn the_fonts_have_the_sizes_the_design_names() {
        assert_eq!(art(Font::Large, false).dimensions(), (75, 8));
        assert_eq!(art(Font::Large, true).dimensions(), (100, 8));
        assert_eq!(art(Font::Medium, false).dimensions(), (44, 5));
        assert_eq!(art(Font::Medium, true).dimensions(), (58, 5));
        assert_eq!(art(Font::Small, false).dimensions(), (32, 3));
        assert_eq!(art(Font::Small, true).dimensions(), (40, 3));
    }

    #[test]
    fn the_first_size_that_fits_wins() {
        let unicode = theme(Glyphs::Unicode);
        assert_eq!(Size::pick(&unicode, 200, 20), art(Font::Large, true));
        assert_eq!(Size::pick(&unicode, 90, 20), art(Font::Large, false));
        assert_eq!(Size::pick(&unicode, 70, 20), art(Font::Medium, true));
        assert_eq!(Size::pick(&unicode, 50, 20), art(Font::Medium, false));
        assert_eq!(Size::pick(&unicode, 200, 7), art(Font::Medium, true));
        assert_eq!(Size::pick(&unicode, 40, 4), art(Font::Small, true));
        assert_eq!(Size::pick(&unicode, 35, 3), art(Font::Small, false));
        assert_eq!(Size::pick(&unicode, 30, 20), Size::Plain);
        assert_eq!(Size::pick(&unicode, 200, 2), Size::Plain);
        // ASCII glyphs: the plain word whatever the room.
        assert_eq!(Size::pick(&theme(Glyphs::Ascii), 200, 20), Size::Plain);
    }

    #[test]
    fn the_logo_sits_left_of_the_word_on_its_lower_rows() {
        let rows = drawn(art(Font::Large, true));
        assert!(rows[2].starts_with("██████████████   "), "{rows:#?}");
        assert!(rows[4].starts_with("    ██████████████"), "{rows:#?}");
        assert!(rows[6].starts_with("        ██████████████"), "{rows:#?}");
        // The word starts after the logo and its gap.
        assert!(rows[7].chars().take(26).all(|c| c == ' '), "{rows:#?}");
        assert!(
            rows[7].chars().nth(26).is_some_and(|c| c == '░'),
            "{rows:#?}"
        );
        let rows = drawn(art(Font::Small, true));
        assert_eq!(
            rows,
            [
                "▀▀▀▀▀   ░█▀▄░█▀▀░█░█░█░░░█▀█░█▀█░█▀▀░█▀▀",
                " ▀▀▀▀▀  ░█░█░█▀▀░▀▄▀░█░░░█▀█░█░█░█▀▀░▀▀█",
                "  ▀▀▀▀▀ ░▀▀░░▀▀▀░░▀░░▀▀▀░▀░▀░▀░▀░▀▀▀░▀▀▀",
            ]
        );
        assert_eq!(drawn(Size::Plain), ["devlanes"]);
    }

    #[test]
    fn faces_and_shades_take_their_colors() {
        let size = art(Font::Large, true);
        let (width, height) = size.dimensions();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &theme(Glyphs::Unicode), size);
        // The top bar yellow, the others gray.
        assert_eq!(buf[(0, 2)].fg, Color::Yellow);
        assert_eq!(buf[(4, 4)].fg, Color::Gray);
        // DOS Rebel: `█` face, `░` shade.
        let first = |c: &str| {
            (0..height)
                .flat_map(|y| (25..width).map(move |x| (x, y)))
                .find(|&(x, y)| buf[(x, y)].symbol() == c)
        };
        let (x, y) = first("█").expect("a face cell");
        assert_eq!(buf[(x, y)].fg, Color::LightYellow);
        let (x, y) = first("░").expect("a shade cell");
        assert_eq!(buf[(x, y)].fg, Color::Yellow);
    }

    #[test]
    fn the_backdrop_keeps_content_clear() {
        let area = Rect::new(0, 0, 33, 3);
        let mut buf = Buffer::empty(area);
        let content = Rect::new(14, 1, 5, 1);
        backdrop(&mut buf, area, &theme(Glyphs::Unicode), &[content]);
        let row = |y: u16| -> String { (0..33).map(|x| buf[(x, y)].symbol()).collect() };
        assert_eq!(row(0), "╎       ╎       ║       ╎       ╎");
        assert_eq!(row(1), "╎       ╎               ╎       ╎");
        let mut buf = Buffer::empty(area);
        backdrop(&mut buf, area, &theme(Glyphs::Ascii), &[]);
        let row: String = (0..33).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(row, ":       :       ||      :       :");
    }
}
