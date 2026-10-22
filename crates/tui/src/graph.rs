//! Graphs for the monitor: filled braille areas colored by a load gradient,
//! block sparklines and meters. ratatui's `Chart` and `Canvas` paint one
//! color per dataset or shape, so a gradient that runs green at the bottom
//! to red at the top needs its own widget.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Span,
};

use crate::theme::{Glyphs, Theme, Tier};

/// Braille dot bits from the bottom of a cell up, left column then right.
const LEFT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];
/// Eighth blocks for sparklines, empty to full.
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const ASCII_BLOCKS: [char; 9] = [' ', '.', '.', ':', ':', '=', '=', '#', '#'];

/// The gradient stops at load 0, 0.5, 0.8 and 1: green, yellow, orange, red.
const DARK_STOPS: [(u8, u8, u8); 4] = [
    (0x3e, 0xa6, 0x6b),
    (0xf5, 0xc5, 0x42),
    (0xe8, 0x8a, 0x3c),
    (0xd9, 0x4f, 0x3d),
];
/// The same stops, darker so they read on a light background.
const LIGHT_STOPS: [(u8, u8, u8); 4] = [
    (0x1d, 0x7a, 0x46),
    (0x9a, 0x6b, 0x00),
    (0xb3, 0x5c, 0x00),
    (0xc0, 0x39, 0x2b),
];
const STOP_AT: [f64; 4] = [0.0, 0.5, 0.8, 1.0];

/// The color of a load from 0 (idle) to 1 (full) and above: interpolated in
/// true color, the nearest of green, yellow, light red and red in 16 colors,
/// none without color.
pub fn load_color(theme: &Theme, load: f64) -> Option<Color> {
    let load = if load.is_finite() {
        load.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let stops = match theme.tier {
        Tier::Plain => return None,
        Tier::Ansi => {
            return Some(match load {
                l if l < 0.5 => Color::Green,
                l if l < 0.8 => Color::Yellow,
                l if l < 1.0 => Color::LightRed,
                _ => Color::Red,
            });
        }
        Tier::Dark => DARK_STOPS,
        Tier::Light => LIGHT_STOPS,
    };
    let upper = STOP_AT
        .iter()
        .position(|at| load <= *at)
        .unwrap_or(3)
        .max(1);
    let (from, to) = (stops[upper - 1], stops[upper]);
    let span = STOP_AT[upper] - STOP_AT[upper - 1];
    let t = (load - STOP_AT[upper - 1]) / span;
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
    Some(Color::Rgb(
        mix(from.0, to.0),
        mix(from.1, to.1),
        mix(from.2, to.2),
    ))
}

/// Draws `values` (0 to 1, two per cell, newest at the right) as a filled
/// braille area, each cell colored by the load its row stands for. `overlay`
/// (same layout) adds one dot per sample at its height, in `overlay_style`
/// where the area leaves the cell empty: a system line or bucket maxima.
pub fn braille(
    buf: &mut Buffer,
    area: Rect,
    values: &[Option<f64>],
    overlay: &[Option<f64>],
    color: &dyn Fn(f64) -> Option<Color>,
    overlay_style: Style,
    glyphs: Glyphs,
) {
    if area.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let dots = height * 4;
    // Samples line up at the right edge: the newest is always in the last column.
    let sample = |series: &[Option<f64>], column: usize, side: usize| -> Option<usize> {
        let index = (column * 2 + side + series.len()).checked_sub(width * 2)?;
        let value = (*series.get(index)?)?;
        Some((value.clamp(0.0, 1.0) * dots as f64).round() as usize)
    };
    for column in 0..width {
        for row in 0..height {
            // Rows count from the bottom; `base` is the first dot this cell holds.
            let from_bottom = height - 1 - row;
            let base = from_bottom * 4;
            let mut fill = 0u8;
            let mut extra = 0u8;
            for (side, bits) in [LEFT, RIGHT].iter().enumerate() {
                if let Some(level) = sample(values, column, side) {
                    for (dot, bit) in bits.iter().enumerate() {
                        if level > base + dot {
                            fill |= bit;
                        }
                    }
                }
                if let Some(level) = sample(overlay, column, side)
                    && level > 0
                    && (base..base + 4).contains(&(level - 1))
                {
                    extra |= bits[level - 1 - base];
                }
            }
            let bits = fill | extra;
            if bits == 0 {
                continue;
            }
            let symbol = match glyphs {
                Glyphs::Unicode => char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' '),
                Glyphs::Ascii if fill != 0 => '#',
                Glyphs::Ascii => '.',
            };
            let style = if fill != 0 {
                let load = (from_bottom as f64 + 0.5) / height as f64;
                color(load).map_or_else(Style::new, |c| Style::new().fg(c))
            } else {
                overlay_style
            };
            let x = area.x + column as u16;
            let y = area.y + row as u16;
            buf[(x, y)].set_char(symbol).set_style(style);
        }
    }
}

/// One block per value (0 to 1), `None` for a bucket without a sample.
pub fn blocks(value: Option<f64>, glyphs: Glyphs) -> char {
    let Some(value) = value else {
        return ' ';
    };
    let level = (value.clamp(0.0, 1.0) * 8.0).round() as usize;
    // Anything above zero shows, so a quiet unit is not mistaken for no data.
    let level = if value > 0.0 { level.max(1) } else { level };
    match glyphs {
        Glyphs::Unicode => BLOCKS[level],
        Glyphs::Ascii => ASCII_BLOCKS[level],
    }
}

/// A bar of `cells` cells for `share` (0 to 1): filled cells colored by
/// their position along the bar, empty ones as rule-colored squares.
pub fn meter(
    theme: &Theme,
    share: f64,
    cells: usize,
    color: &dyn Fn(f64) -> Option<Color>,
) -> Vec<Span<'static>> {
    let ascii = theme.glyphs == Glyphs::Ascii;
    let filled = ((share.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
    let mut spans: Vec<Span> = (0..filled)
        .map(|cell| {
            let load = (cell as f64 + 0.5) / cells as f64;
            let style = color(load).map_or_else(Style::new, |c| Style::new().fg(c));
            Span::styled(if ascii { "#" } else { "■" }, style)
        })
        .collect();
    if filled < cells {
        let empty = if ascii { "." } else { "■" };
        spans.push(Span::styled(empty.repeat(cells - filled), theme.rule()));
    }
    spans
}

/// Splits `[since, since + range)` into `count` buckets and folds the
/// samples of each: the highest when `max`, else the average. Buckets
/// without a sample are `None`.
pub fn buckets(
    samples: &[(u64, f64)],
    since: u64,
    range: u64,
    count: usize,
    max: bool,
) -> Vec<Option<f64>> {
    if count == 0 || range == 0 {
        return Vec::new();
    }
    let mut sums = vec![(0.0f64, 0u32); count];
    for (ts, value) in samples {
        let Some(offset) = ts.checked_sub(since) else {
            continue;
        };
        let index = ((u128::from(offset) * count as u128) / u128::from(range)) as usize;
        let Some(bucket) = sums.get_mut(index.min(count - 1)) else {
            continue;
        };
        if max {
            bucket.0 = if bucket.1 == 0 {
                *value
            } else {
                bucket.0.max(*value)
            };
        } else {
            bucket.0 += value;
        }
        bucket.1 += 1;
    }
    sums.into_iter()
        .map(|(value, n)| match n {
            0 => None,
            _ if max => Some(value),
            n => Some(value / f64::from(n)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(tier: Tier) -> Theme {
        Theme {
            tier,
            glyphs: Glyphs::Unicode,
        }
    }

    /// Draws `values` into a `width` x `height` buffer and returns its rows.
    fn draw(
        width: u16,
        height: u16,
        values: &[Option<f64>],
        overlay: &[Option<f64>],
    ) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        braille(
            &mut buf,
            area,
            values,
            overlay,
            &|_| None,
            Style::new(),
            Glyphs::Unicode,
        );
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn braille_fills_from_the_bottom_two_samples_per_cell() {
        // One cell, four dots high: left full, right half.
        assert_eq!(draw(1, 1, &[Some(1.0), Some(0.5)], &[]), ["⣧"]);
        assert_eq!(draw(1, 1, &[Some(1.0), Some(1.0)], &[]), ["⣿"]);
        assert_eq!(draw(1, 1, &[Some(0.0), Some(0.0)], &[]), [" "]);
        // Two rows: 0.5 fills the bottom row only.
        assert_eq!(draw(1, 2, &[Some(0.5), Some(0.5)], &[]), [" ", "⣿"]);
    }

    #[test]
    fn braille_aligns_the_newest_sample_to_the_right_edge() {
        assert_eq!(draw(3, 1, &[Some(1.0), Some(1.0)], &[]), ["  ⣿"]);
        // Too many samples: the oldest drop off the left.
        assert_eq!(
            draw(1, 1, &[Some(1.0), Some(1.0), Some(0.0), Some(1.0)], &[]),
            ["⢸"]
        );
    }

    #[test]
    fn the_overlay_sets_one_dot_at_its_height() {
        // A line at the top of an empty cell, left side only.
        assert_eq!(draw(1, 1, &[None, None], &[Some(1.0), None]), ["⠁"]);
        // Over a filled area it joins the cell.
        assert_eq!(draw(1, 1, &[Some(0.25), None], &[Some(1.0), None]), ["⡁"]);
    }

    #[test]
    fn rows_take_the_load_they_stand_for() {
        let area = Rect::new(0, 0, 1, 4);
        let mut buf = Buffer::empty(area);
        let color = |load: f64| Some(if load > 0.5 { Color::Red } else { Color::Green });
        braille(
            &mut buf,
            area,
            &[Some(1.0), Some(1.0)],
            &[],
            &color,
            Style::new(),
            Glyphs::Unicode,
        );
        assert_eq!(buf[(0, 0)].fg, Color::Red, "top");
        assert_eq!(buf[(0, 3)].fg, Color::Green, "bottom");
    }

    #[test]
    fn the_gradient_runs_green_yellow_orange_red() {
        let dark = theme(Tier::Dark);
        assert_eq!(load_color(&dark, 0.0), Some(Color::Rgb(0x3e, 0xa6, 0x6b)));
        assert_eq!(load_color(&dark, 0.5), Some(Color::Rgb(0xf5, 0xc5, 0x42)));
        assert_eq!(load_color(&dark, 0.8), Some(Color::Rgb(0xe8, 0x8a, 0x3c)));
        assert_eq!(load_color(&dark, 3.0), Some(Color::Rgb(0xd9, 0x4f, 0x3d)));
        let ansi = theme(Tier::Ansi);
        let picks: Vec<_> = [0.1, 0.6, 0.9, 1.0]
            .iter()
            .map(|l| load_color(&ansi, *l))
            .collect();
        assert_eq!(
            picks,
            [
                Some(Color::Green),
                Some(Color::Yellow),
                Some(Color::LightRed),
                Some(Color::Red)
            ]
        );
        assert_eq!(load_color(&theme(Tier::Plain), 1.0), None);
        assert_eq!(load_color(&dark, f64::NAN), load_color(&dark, 0.0));
    }

    #[test]
    fn blocks_show_any_load_and_leave_gaps_empty() {
        assert_eq!(blocks(None, Glyphs::Unicode), ' ');
        assert_eq!(blocks(Some(0.0), Glyphs::Unicode), ' ');
        assert_eq!(blocks(Some(0.01), Glyphs::Unicode), '▁');
        assert_eq!(blocks(Some(1.0), Glyphs::Unicode), '█');
        assert_eq!(blocks(Some(1.0), Glyphs::Ascii), '#');
    }

    #[test]
    fn meters_fill_by_share() {
        let spans = meter(&theme(Tier::Ansi), 0.5, 8, &|_| None);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().count(), 8);
        assert_eq!(spans.len(), 5, "four filled cells and one empty run");
    }

    #[test]
    fn buckets_keep_the_highest_or_the_average() {
        let samples = [(0, 1.0), (4, 3.0), (5, 10.0), (9, 2.0)];
        assert_eq!(buckets(&samples, 0, 10, 2, true), [Some(3.0), Some(10.0)]);
        assert_eq!(buckets(&samples, 0, 10, 2, false), [Some(2.0), Some(6.0)]);
        assert_eq!(buckets(&samples, 0, 10, 4, true)[1], Some(3.0));
        assert_eq!(buckets(&[], 0, 10, 2, true), [None, None]);
        assert!(buckets(&samples, 0, 10, 0, true).is_empty());
    }
}
