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
                    // A zero sample is one dim dot on the bottom row, so it
                    // is not mistaken for a second without data.
                    if level == 0 && from_bottom == 0 {
                        extra |= bits[0];
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

/// One block per value (0 to 1); a column without a sample stays blank.
/// Zero is a sample too: it draws the lowest block (callers dim it), so an
/// idle unit is never mistaken for one without data.
pub fn blocks(value: Option<f64>, glyphs: Glyphs) -> char {
    let Some(value) = value else {
        return ' ';
    };
    let level = ((value.clamp(0.0, 1.0) * 8.0).round() as usize).max(1);
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

/// Graph columns pinned to the clock, as btop draws them: column `n` holds
/// the samples of `[n * step, (n + 1) * step)`, and the last one the newest
/// sample. While time passes inside a step no column moves; when a step
/// ends, everything shifts left by exactly one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeline {
    /// Milliseconds per column: the range over the column count, rounded up.
    pub step: u64,
    /// The newest column's number, `now / step`.
    pub last: u64,
    pub count: usize,
}

impl Timeline {
    /// `count` columns over `range` milliseconds ending at `now`.
    pub fn new(now: u64, range: u64, count: usize) -> Self {
        let step = range.div_ceil(count.max(1) as u64).max(1);
        Self {
            step,
            last: now / step,
            count,
        }
    }

    /// The oldest column's number.
    pub fn first(&self) -> u64 {
        (self.last + 1).saturating_sub(self.count as u64)
    }

    /// Folds `samples` into the columns: the highest when `max`, else the
    /// average. Columns without a sample are `None`.
    pub fn fold(&self, samples: &[(u64, f64)], max: bool) -> Vec<Option<f64>> {
        let first = self.first();
        let mut sums = vec![(0.0f64, 0u32); self.count];
        for (ts, value) in samples {
            let Some(index) = (ts / self.step).checked_sub(first) else {
                continue;
            };
            let Some(bucket) = sums.get_mut(index as usize) else {
                continue;
            };
            fold_into(bucket, *value, max);
        }
        finish(sums, max)
    }
}

fn fold_into(bucket: &mut (f64, u32), value: f64, max: bool) {
    if max {
        bucket.0 = if bucket.1 == 0 {
            value
        } else {
            bucket.0.max(value)
        };
    } else {
        bucket.0 += value;
    }
    bucket.1 += 1;
}

fn finish(sums: Vec<(f64, u32)>, max: bool) -> Vec<Option<f64>> {
    sums.into_iter()
        .map(|(value, n)| match n {
            0 => None,
            _ if max => Some(value),
            n => Some(value / f64::from(n)),
        })
        .collect()
}

/// Series past this many are dropped together; the page shows far fewer.
const CACHED_SERIES: usize = 512;

/// Finished graph columns and sticky scales, kept between frames so a column
/// is computed once, when its step ends, and never redrawn differently: a
/// later reply that buckets or trims history differently does not make the
/// past wobble. Interior mutability because drawing borrows the app shared.
#[derive(Debug, Default)]
pub struct Columns {
    inner: std::cell::RefCell<ColumnsInner>,
}

#[derive(Debug, Default)]
struct ColumnsInner {
    /// Raw (not normalized) finished columns by series key and step.
    series: std::collections::HashMap<(String, u64), std::collections::BTreeMap<u64, Option<f64>>>,
    /// Scales that only grow, by key.
    sticky: std::collections::HashMap<String, f64>,
}

impl Columns {
    /// The columns of `samples` on `timeline`, raw: finished columns from
    /// the cache once computed, the newest one always from `samples`. `key`
    /// names the series and how it folds (`max`).
    pub fn values(
        &self,
        key: &str,
        samples: &[(u64, f64)],
        timeline: &Timeline,
        max: bool,
    ) -> Vec<Option<f64>> {
        let fresh = timeline.fold(samples, max);
        let mut inner = self.inner.borrow_mut();
        if inner.series.len() >= CACHED_SERIES {
            inner.series.clear();
        }
        let done = inner
            .series
            .entry((format!("{key}/{max}"), timeline.step))
            .or_default();
        let first = timeline.first();
        // Columns that scrolled off are not needed again.
        done.retain(|column, _| *column >= first);
        fresh
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let column = first + index as u64;
                if column >= timeline.last {
                    return value;
                }
                *done.entry(column).or_insert(value)
            })
            .collect()
    }

    /// `value` or the largest value seen for `key` before, whichever is
    /// larger: a scale that grows with a peak and never shrinks back.
    pub fn sticky(&self, key: &str, value: f64) -> f64 {
        let mut inner = self.inner.borrow_mut();
        let entry = inner.sticky.entry(key.to_owned()).or_insert(value);
        if value > *entry {
            *entry = value;
        }
        *entry
    }
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
        // Zero is one dot at the bottom; no sample, nothing.
        assert_eq!(draw(1, 1, &[Some(0.0), Some(0.0)], &[]), ["⣀"]);
        assert_eq!(draw(1, 1, &[None, None], &[]), [" "]);
        // Two rows: 0.5 fills the bottom row only.
        assert_eq!(draw(1, 2, &[Some(0.5), Some(0.5)], &[]), [" ", "⣿"]);
    }

    #[test]
    fn braille_aligns_the_newest_sample_to_the_right_edge() {
        assert_eq!(draw(3, 1, &[Some(1.0), Some(1.0)], &[]), ["  ⣿"]);
        // Too many samples: the oldest drop off the left (the zero is the
        // bottom-left dot).
        assert_eq!(
            draw(1, 1, &[Some(1.0), Some(1.0), Some(0.0), Some(1.0)], &[]),
            ["⣸"]
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
    fn blocks_tell_zero_from_missing() {
        // No sample: blank. A zero sample: the baseline block.
        assert_eq!(blocks(None, Glyphs::Unicode), ' ');
        assert_eq!(blocks(Some(0.0), Glyphs::Unicode), '▁');
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
    fn columns_keep_the_highest_or_the_average() {
        // Two columns of 5ms ending at 9: [0, 5) and [5, 10).
        let timeline = Timeline::new(9, 10, 2);
        assert_eq!((timeline.step, timeline.first(), timeline.last), (5, 0, 1));
        let samples = [(0, 1.0), (4, 3.0), (5, 10.0), (9, 2.0)];
        assert_eq!(timeline.fold(&samples, true), [Some(3.0), Some(10.0)]);
        assert_eq!(timeline.fold(&samples, false), [Some(2.0), Some(6.0)]);
        assert_eq!(timeline.fold(&[], true), [None, None]);
    }

    #[test]
    fn columns_are_pinned_to_the_clock() {
        let samples: Vec<(u64, f64)> = (0..100).map(|ts| (ts, ts as f64)).collect();
        // Within one step the columns do not move, whatever `now` is.
        let early = Timeline::new(40, 50, 5);
        let late = Timeline::new(49, 50, 5);
        assert_eq!(early.first(), late.first());
        let (a, b) = (early.fold(&samples, true), late.fold(&samples, true));
        assert_eq!(a[..4], b[..4], "finished columns match");
        // The next step shifts everything left by exactly one column.
        let next = Timeline::new(50, 50, 5).fold(&samples, true);
        assert_eq!(b[1..], next[..4]);
    }

    #[test]
    fn a_finished_column_is_identical_across_frames() {
        // Six columns of 10s; samples every second for 30s.
        let base = 1_000_000;
        let cache = Columns::default();
        let samples: Vec<(u64, f64)> = (0..30).map(|s| (base + s * 1000, 50.0)).collect();
        let timeline = Timeline::new(base + 29_999, 60_000, 6);
        let first = cache.values("api", &samples, &timeline, true);
        // A later reply has the past differently (a spike the first reply
        // did not have, early samples trimmed); finished columns stay.
        let mut later: Vec<(u64, f64)> = samples.iter().skip(3).copied().collect();
        later.push((base + 12_500, 99.0));
        later.push((base + 31_000, 70.0));
        let timeline = Timeline::new(base + 31_000, 60_000, 6);
        let second = cache.values("api", &later, &timeline, true);
        // One step later every column moved left by one, unchanged.
        assert_eq!(first[1..], second[..5], "{first:?} {second:?}");
        // Only the newest column follows the data.
        assert_eq!(second[5], Some(70.0));
        // A fresh cache sees the spike: only the cache kept the past fixed.
        let third = Columns::default().values("api", &later, &timeline, true);
        assert_eq!(third[3], Some(99.0));
    }

    #[test]
    fn one_second_columns_shift_left_one_per_second() {
        // Each column is one second: a graph of 5 columns shows 5 seconds.
        let base = 1_000_000;
        let samples: Vec<(u64, f64)> = (0..10).map(|s| (base + s * 1000, s as f64)).collect();
        let at = |now: u64| Timeline::new(base + now, 5_000, 5).fold(&samples, true);
        assert_eq!(at(9_000), [5.0, 6.0, 7.0, 8.0, 9.0].map(Some));
        // A wider graph shows more history, the same newest second.
        assert_eq!(
            Timeline::new(base + 9_000, 8_000, 8).fold(&samples, true)[7],
            Some(9.0)
        );
        // A second later the newest enters on the right; the rest moved left.
        let mut more = samples.clone();
        more.push((base + 10_000, 10.0));
        let next = Timeline::new(base + 10_000, 5_000, 5).fold(&more, true);
        assert_eq!(next, [6.0, 7.0, 8.0, 9.0, 10.0].map(Some));
    }

    #[test]
    fn sticky_scales_only_grow() {
        let cache = Columns::default();
        assert_eq!(cache.sticky("mem", 100.0), 100.0);
        assert_eq!(cache.sticky("mem", 50.0), 100.0);
        assert_eq!(cache.sticky("mem", 200.0), 200.0);
        assert_eq!(cache.sticky("cpu", 50.0), 50.0);
    }
}
