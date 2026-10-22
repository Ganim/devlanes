//! Turns a rendered ratatui buffer into plain text, an ANSI dump and a PNG.
//!
//! The PNG draws each cell on a fixed grid with a system monospace font.
//! Box-drawing and block characters are drawn as shapes, so borders join up
//! across cells the way a terminal shows them.

use std::{fs, io, path::Path};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use unicode_width::UnicodeWidthStr;

/// Glyph height in pixels; cells are sized from the font's metrics.
const FONT_PX: f32 = 17.0;
/// A dark background close to the brand's ink color.
const BACKGROUND: [u8; 3] = [0x1a, 0x19, 0x17];
const FOREGROUND: [u8; 3] = [0xe6, 0xe3, 0xda];
/// The page and text of a light terminal theme, for the light palette.
const LIGHT_BACKGROUND: [u8; 3] = [0xff, 0xff, 0xff];
const LIGHT_FOREGROUND: [u8; 3] = [0x1a, 0x19, 0x17];
/// The 16 ANSI colors as a common dark theme draws them.
const ANSI: [[u8; 3]; 16] = [
    [0x1a, 0x19, 0x17],
    [0xd9, 0x4f, 0x3d],
    [0x3e, 0xa6, 0x6b],
    [0xe0, 0xb0, 0x3a],
    [0x4f, 0x8f, 0xe0],
    [0xb4, 0x6f, 0xd6],
    [0x3a, 0xb8, 0xc0],
    [0xc8, 0xc5, 0xbc],
    [0x6b, 0x68, 0x60],
    [0xf2, 0x6d, 0x5b],
    [0x5f, 0xd1, 0x8c],
    [0xf5, 0xc5, 0x42],
    [0x7a, 0xae, 0xf5],
    [0xd0, 0x92, 0xf0],
    [0x63, 0xd8, 0xdf],
    [0xf4, 0xf2, 0xec],
];

/// Fonts tried in order; later ones fill in glyphs the first lacks (braille,
/// symbols). `DEVLANES_SCREENS_FONT` puts one in front.
const FONT_PATHS: [&str; 7] = [
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

/// The loaded fonts and the cell size they give.
pub struct Fonts {
    faces: Vec<FontVec>,
    cell: (u32, u32),
    ascent: f32,
}

impl Fonts {
    /// Loads every font that exists; `None` when there is none at all.
    pub fn load() -> Option<Self> {
        let mut paths: Vec<String> = std::env::var("DEVLANES_SCREENS_FONT").into_iter().collect();
        paths.extend(FONT_PATHS.iter().map(|path| (*path).to_owned()));
        let faces: Vec<FontVec> = paths
            .iter()
            .filter_map(|path| fs::read(path).ok())
            .filter_map(|bytes| FontVec::try_from_vec_and_index(bytes, 0).ok())
            .collect();
        let first = faces.first()?;
        let scaled = first.as_scaled(PxScale::from(FONT_PX));
        let width = scaled.h_advance(first.glyph_id('M')).ceil() as u32;
        let height = (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil() as u32 + 2;
        Some(Self {
            cell: (width.max(1), height.max(1)),
            ascent: scaled.ascent() + 1.0,
            faces,
        })
    }
}

/// An RGB canvas.
struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    /// A canvas filled with `color`.
    fn new(width: u32, height: u32, color: [u8; 3]) -> Self {
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&color);
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    /// Blends `color` into one pixel with `alpha` from 0 to 1.
    fn blend(&mut self, x: i64, y: i64, color: [u8; 3], alpha: f32) {
        if x < 0 || y < 0 || x >= i64::from(self.width) || y >= i64::from(self.height) {
            return;
        }
        let index = ((y as u32 * self.width + x as u32) * 3) as usize;
        let alpha = alpha.clamp(0.0, 1.0);
        for (channel, value) in color.iter().enumerate() {
            let old = f32::from(self.pixels[index + channel]);
            self.pixels[index + channel] = (old + (f32::from(*value) - old) * alpha).round() as u8;
        }
    }

    /// Fills a rectangle.
    fn rect(&mut self, x: i64, y: i64, width: i64, height: i64, color: [u8; 3]) {
        for dy in 0..height {
            for dx in 0..width {
                self.blend(x + dx, y + dy, color, 1.0);
            }
        }
    }
}

/// The RGB value of a ratatui color; `None` for the terminal default.
fn rgb(color: Color) -> Option<[u8; 3]> {
    Some(match color {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(index) => indexed(index),
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::Gray => ANSI[7],
        Color::DarkGray => ANSI[8],
        Color::LightRed => ANSI[9],
        Color::LightGreen => ANSI[10],
        Color::LightYellow => ANSI[11],
        Color::LightBlue => ANSI[12],
        Color::LightMagenta => ANSI[13],
        Color::LightCyan => ANSI[14],
        Color::White => ANSI[15],
    })
}

/// The xterm 256-color palette.
fn indexed(index: u8) -> [u8; 3] {
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let n = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            [level(n / 36), level((n / 6) % 6), level(n % 6)]
        }
        _ => {
            let gray = 8 + (index - 232) * 10;
            [gray, gray, gray]
        }
    }
}

/// A cell's colors after reverse video and dimming.
fn colors(cell: &ratatui::buffer::Cell, light: bool) -> ([u8; 3], [u8; 3]) {
    let (foreground, background) = if light {
        (LIGHT_FOREGROUND, LIGHT_BACKGROUND)
    } else {
        (FOREGROUND, BACKGROUND)
    };
    let mut fg = rgb(cell.fg).unwrap_or(foreground);
    let mut bg = rgb(cell.bg).unwrap_or(background);
    if cell.modifier.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.modifier.contains(Modifier::DIM) {
        fg = [0, 1, 2].map(|i| ((u16::from(fg[i]) + u16::from(bg[i])) / 2) as u8);
    }
    (fg, bg)
}

/// The buffer's rows, each a line of text with trailing spaces trimmed.
pub fn text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..buffer.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buffer[(x, y)].symbol();
            skip = symbol.width().saturating_sub(1);
            line.push_str(symbol);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// SGR parameters for a color as foreground (`base` 30) or background (40).
fn sgr_color(color: Color, base: u8) -> String {
    match color {
        Color::Reset => format!("{}", base + 9),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        Color::Indexed(index) => format!("{};5;{index}", base + 8),
        named => {
            let index = match named {
                Color::Black => 0,
                Color::Red => 1,
                Color::Green => 2,
                Color::Yellow => 3,
                Color::Blue => 4,
                Color::Magenta => 5,
                Color::Cyan => 6,
                Color::Gray => 7,
                Color::DarkGray => 8,
                Color::LightRed => 9,
                Color::LightGreen => 10,
                Color::LightYellow => 11,
                Color::LightBlue => 12,
                Color::LightMagenta => 13,
                Color::LightCyan => 14,
                _ => 15,
            };
            if index < 8 {
                format!("{}", base + index)
            } else {
                format!("{}", base + 60 + index - 8)
            }
        }
    }
}

/// The buffer as ANSI text: `cat` it in a true-color terminal to see the page.
pub fn ansi(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut last: Option<(Color, Color, Modifier)> = None;
        let mut skip = 0;
        for x in 0..buffer.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buffer[(x, y)];
            let style = (cell.fg, cell.bg, cell.modifier);
            if last != Some(style) {
                let mut codes = vec!["0".to_owned()];
                for (flag, code) in [
                    (Modifier::BOLD, "1"),
                    (Modifier::DIM, "2"),
                    (Modifier::ITALIC, "3"),
                    (Modifier::UNDERLINED, "4"),
                    (Modifier::REVERSED, "7"),
                ] {
                    if cell.modifier.contains(flag) {
                        codes.push(code.into());
                    }
                }
                codes.push(sgr_color(cell.fg, 30));
                codes.push(sgr_color(cell.bg, 40));
                out.push_str(&format!("\x1b[{}m", codes.join(";")));
                last = Some(style);
            }
            let symbol = cell.symbol();
            skip = symbol.width().saturating_sub(1);
            out.push_str(symbol);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

/// Writes the buffer as a PNG, on a light page when `light`.
pub fn png(buffer: &Buffer, fonts: &Fonts, path: &Path, light: bool) -> io::Result<()> {
    let (cell_w, cell_h) = fonts.cell;
    let pad = cell_w;
    let width = u32::from(buffer.area.width) * cell_w + pad * 2;
    let height = u32::from(buffer.area.height) * cell_h + pad * 2;
    let page = if light { LIGHT_BACKGROUND } else { BACKGROUND };
    let mut canvas = Canvas::new(width, height, page);
    for y in 0..buffer.area.height {
        let mut skip = 0;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let (fg, bg) = colors(cell, light);
            let left = i64::from(pad + u32::from(x) * cell_w);
            let top = i64::from(pad + u32::from(y) * cell_h);
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = cell.symbol();
            let columns = symbol.width().max(1);
            skip = columns - 1;
            canvas.rect(
                left,
                top,
                i64::from(cell_w) * columns as i64,
                i64::from(cell_h),
                bg,
            );
            let Some(ch) = symbol.chars().next() else {
                continue;
            };
            if ch != ' ' && !shape(&mut canvas, ch, left, top, cell_w, cell_h, fg) {
                glyph(&mut canvas, fonts, ch, left, top, fg, cell.modifier);
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                canvas.rect(left, top + i64::from(cell_h) - 2, i64::from(cell_w), 1, fg);
            }
        }
    }
    let file = fs::File::create(path)?;
    let mut encoder = png::Encoder::new(io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer
        .write_image_data(&canvas.pixels)
        .map_err(io::Error::other)
}

/// Draws one character from the first font that has it; bold is drawn twice,
/// one pixel apart.
fn glyph(
    canvas: &mut Canvas,
    fonts: &Fonts,
    ch: char,
    left: i64,
    top: i64,
    color: [u8; 3],
    modifier: Modifier,
) {
    let Some(font) = fonts
        .faces
        .iter()
        .find(|font| font.glyph_id(ch).0 != 0)
        .or(fonts.faces.first())
    else {
        return;
    };
    let scale = PxScale::from(FONT_PX);
    let passes: &[f32] = if modifier.contains(Modifier::BOLD) {
        &[0.0, 0.6]
    } else {
        &[0.0]
    };
    for offset in passes {
        let positioned = font.glyph_id(ch).with_scale_and_position(
            scale,
            point(left as f32 + offset, top as f32 + fonts.ascent),
        );
        if let Some(outline) = font.outline_glyph(positioned) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                canvas.blend(
                    bounds.min.x as i64 + i64::from(gx),
                    bounds.min.y as i64 + i64::from(gy),
                    color,
                    coverage,
                );
            });
        }
    }
}

/// Draws box-drawing lines and block elements as shapes; false for other characters.
fn shape(
    canvas: &mut Canvas,
    ch: char,
    left: i64,
    top: i64,
    w: u32,
    h: u32,
    color: [u8; 3],
) -> bool {
    let (w, h) = (i64::from(w), i64::from(h));
    let (cx, cy) = (left + w / 2, top + h / 2);
    // Which arms of a light box-drawing character exist: up, down, left, right.
    let arms = match ch {
        '─' => [false, false, true, true],
        '│' => [true, true, false, false],
        '┌' | '╭' => [false, true, false, true],
        '┐' | '╮' => [false, true, true, false],
        '└' | '╰' => [true, false, false, true],
        '┘' | '╯' => [true, false, true, false],
        '├' => [true, true, false, true],
        '┤' => [true, true, true, false],
        '┬' => [false, true, true, true],
        '┴' => [true, false, true, true],
        '┼' => [true, true, true, true],
        _ => {
            return block(canvas, ch, left, top, w, h, color);
        }
    };
    let [up, down, left_arm, right_arm] = arms;
    if up {
        canvas.rect(cx, top, 1, cy - top + 1, color);
    }
    if down {
        canvas.rect(cx, cy, 1, top + h - cy, color);
    }
    if left_arm {
        canvas.rect(left, cy, cx - left + 1, 1, color);
    }
    if right_arm {
        canvas.rect(cx, cy, left + w - cx, 1, color);
    }
    true
}

/// Block elements: eighths from the bottom, the full block and the left eighth.
fn block(
    canvas: &mut Canvas,
    ch: char,
    left: i64,
    top: i64,
    w: i64,
    h: i64,
    color: [u8; 3],
) -> bool {
    let eighths = match ch {
        '▁' => 1,
        '▂' => 2,
        '▃' => 3,
        '▄' => 4,
        '▅' => 5,
        '▆' => 6,
        '▇' => 7,
        '█' => 8,
        '▏' => {
            canvas.rect(left, top, (w / 8).max(1), h, color);
            return true;
        }
        '▌' => {
            canvas.rect(left, top, w / 2, h, color);
            return true;
        }
        _ => return false,
    };
    let filled = h * eighths / 8;
    canvas.rect(left, top + h - filled, w, filled, color);
    true
}
