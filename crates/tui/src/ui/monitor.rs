//! The monitor page (`M`) as monitor.md draws it: a cpu and a mem box with
//! braille graphs and per-lane meters, the unit table with bars and
//! sparklines, and the selected row's detail graphs. Every graph, bar and
//! sparkline takes its color from the load gradient in `graph`.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{draw_box, fit, hit, spans_width, unit_look};
use crate::api::{Metrics, UnitMetrics};
use crate::app::{App, Click};
use crate::graph::{self, load_color};
use crate::monitor::{Entry, Group, MonitorRow, SortKey};
use crate::text::{self, clean, clip};
use crate::theme::{Glyph, Glyphs, Theme, Tier, Tone};

/// `(ts, value)` samples, a value normalized for a graph.
type Points = Vec<(u64, f64)>;

/// What every part of the page reads.
struct Page<'a> {
    app: &'a App,
    theme: &'a Theme,
    metrics: Option<&'a Metrics>,
    rows: Vec<MonitorRow>,
    /// The newest sample time, so graphs line up with the data.
    now: u64,
}

impl Page<'_> {
    /// The raw columns of a series (`key` names it): one per second, the
    /// newest on the right, as many as the graph is wide. Finished columns
    /// come from the app's cache; the widest graph sets how much history
    /// the next `metrics` call asks for.
    fn columns(
        &self,
        key: &str,
        samples: &[(u64, f64)],
        count: usize,
        max: bool,
    ) -> Vec<Option<f64>> {
        let seconds = self.app.monitor_seconds.get().max(count);
        self.app.monitor_seconds.set(seconds);
        let timeline = graph::Timeline::new(self.now, count as u64 * SAMPLE_MS, count);
        self.app.graphs.values(key, samples, &timeline, max)
    }

    /// `scale` from now on, or a larger one an earlier frame needed.
    fn sticky(&self, key: &str, scale: f64) -> f64 {
        self.app.graphs.sticky(key, scale)
    }

    /// The scale of the devlanes CPU total: the machine's cores, else one
    /// core per 100% of the highest total seen.
    fn total_cpu_scale(&self, peak: f64) -> f64 {
        match self.cpus() {
            Some(cpus) => cpus * 100.0,
            None => self.sticky("cpu:devlanes", ((peak / 100.0).ceil() * 100.0).max(100.0)),
        }
    }

    fn cpus(&self) -> Option<f64> {
        self.metrics
            .and_then(|m| m.system.as_ref())
            .and_then(|s| s.cpus)
            .map(f64::from)
    }

    fn memory_total(&self) -> Option<u64> {
        self.metrics
            .and_then(|m| m.system.as_ref())
            .and_then(|s| s.memory_total_bytes)
    }

    /// Every unit and container row of the metrics, filter or not.
    fn units(&self) -> &[UnitMetrics] {
        self.metrics.map_or(&[], |m| m.units.as_slice())
    }

    /// The unit and container rows shown, without lane rows.
    fn entries(&self) -> Vec<&Entry> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                MonitorRow::Entry(entry) => Some(entry.as_ref()),
                MonitorRow::Group(_) => None,
            })
            .collect()
    }

    /// The devlanes total as `(ts, cpu, memory)`: lane series plus shared
    /// containers, each container once; summed units on older daemons.
    fn total_history(&self) -> Vec<(u64, f64, u64)> {
        let Some(metrics) = self.metrics else {
            return Vec::new();
        };
        let mut series: Vec<&[(u64, f64, u64)]> = Vec::new();
        if metrics.lanes.is_empty() {
            series.extend(metrics.units.iter().map(|u| u.history.as_slice()));
        } else {
            series.extend(metrics.lanes.iter().map(|l| l.history.as_slice()));
            series.extend(
                metrics
                    .units
                    .iter()
                    .filter(|u| u.shared)
                    .map(|u| u.history.as_slice()),
            );
        }
        sum_series(&series)
    }

    /// The devlanes total now: `(cpu, memory)`.
    fn total_now(&self) -> (f64, u64) {
        let Some(metrics) = self.metrics else {
            let entries = self.entries();
            return (
                entries.iter().filter_map(|e| e.metrics.cpu_percent).sum(),
                entries.iter().filter_map(|e| e.metrics.memory_bytes).sum(),
            );
        };
        if metrics.lanes.is_empty() {
            return (
                metrics.units.iter().filter_map(|u| u.cpu_percent).sum(),
                metrics.units.iter().filter_map(|u| u.memory_bytes).sum(),
            );
        }
        let shared: Vec<&UnitMetrics> = metrics.units.iter().filter(|u| u.shared).collect();
        (
            metrics
                .lanes
                .iter()
                .filter_map(|l| l.cpu_percent)
                .sum::<f64>()
                + shared.iter().filter_map(|u| u.cpu_percent).sum::<f64>(),
            metrics
                .lanes
                .iter()
                .filter_map(|l| l.memory_bytes)
                .sum::<u64>()
                + shared.iter().filter_map(|u| u.memory_bytes).sum::<u64>(),
        )
    }

    /// Lanes and shared groups with their CPU and memory now, for meters.
    fn groups(&self) -> Vec<(String, f64, u64)> {
        let Some(metrics) = self.metrics else {
            return Vec::new();
        };
        let mut groups: Vec<(String, f64, u64)> = metrics
            .lanes
            .iter()
            .map(|l| {
                (
                    l.lane.clone(),
                    l.cpu_percent.unwrap_or(0.0),
                    l.memory_bytes.unwrap_or(0),
                )
            })
            .collect();
        // Shared containers count in their own group; older daemons send no
        // lane series, so every unit sums into its lane.
        for unit in metrics
            .units
            .iter()
            .filter(|u| u.shared || metrics.lanes.is_empty())
        {
            match groups.iter_mut().find(|g| g.0 == unit.lane) {
                Some(group) => {
                    group.1 += unit.cpu_percent.unwrap_or(0.0);
                    group.2 += unit.memory_bytes.unwrap_or(0);
                }
                None => groups.push((
                    unit.lane.clone(),
                    unit.cpu_percent.unwrap_or(0.0),
                    unit.memory_bytes.unwrap_or(0),
                )),
            }
        }
        // Lanes first, then the shared groups, as in the mockups.
        groups.sort_by(|a, b| {
            (a.0.ends_with(" (shared)"), &a.0).cmp(&(b.0.ends_with(" (shared)"), &b.0))
        });
        groups
    }
}

/// One graph column: the daemon samples once a second.
const SAMPLE_MS: u64 = 1000;

/// `45s`, `3m 20s`, `1h 2m`: how far back `seconds` columns reach.
fn span(seconds: usize) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 if seconds.is_multiple_of(60) => format!("{}m", seconds / 60),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds / 60 % 60),
    }
}

/// Point-by-point sums of several histories, by timestamp.
fn sum_series(series: &[&[(u64, f64, u64)]]) -> Vec<(u64, f64, u64)> {
    let mut totals: std::collections::BTreeMap<u64, (f64, u64)> = std::collections::BTreeMap::new();
    for history in series {
        for (ts, cpu, memory) in history.iter() {
            let total = totals.entry(*ts).or_default();
            total.0 += cpu;
            total.1 += memory;
        }
    }
    totals.into_iter().map(|(ts, (c, m))| (ts, c, m)).collect()
}

/// The newest timestamp in the metrics, else the clock.
fn newest(metrics: Option<&Metrics>) -> u64 {
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let Some(metrics) = metrics else {
        return clock;
    };
    metrics
        .units
        .iter()
        .flat_map(|u| u.history.last())
        .chain(metrics.lanes.iter().flat_map(|l| l.history.last()))
        .chain(metrics.system.iter().flat_map(|s| s.history.last()))
        .map(|p| p.0)
        .max()
        .unwrap_or(clock)
}

/// Draws the page into the body.
pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let metrics = app.metrics.as_ref();
    // Graphs drawn below record how many seconds the widest one shows.
    app.monitor_seconds.set(0);
    let page = Page {
        app,
        theme: &app.theme,
        metrics,
        rows: app.monitor_rows(),
        now: newest(metrics),
    };
    if app.monitor.zoom {
        detail_column(&page, frame, area);
        return;
    }
    // Size rules from monitor.md, judged on the body.
    let wide = area.width >= 160;
    let medium = area.width >= 110 && area.height >= 22;
    let rest = if wide || medium {
        let height = (area.height * 3 / 10).clamp(6, 17);
        let names = group_name_width(&page);
        let memory_first = app.monitor.memory_first;
        // Without its own graph the small box takes what its rows need; the
        // big box and its graph get the rest.
        let small_needs = if memory_first {
            names + CPU_ROW_EXTRA + 3
        } else {
            names + MEM_ROW_EXTRA + 3
        };
        let small_needs = u16::try_from(small_needs).unwrap_or(u16::MAX);
        let big_width = if wide {
            area.width * 3 / 5
        } else {
            area.width.saturating_sub(small_needs).max(area.width / 2)
        };
        let big = Rect::new(area.x, area.y, big_width, height);
        let small = Rect::new(area.x + big_width, area.y, area.width - big_width, height);
        if memory_first {
            mem_box(&page, frame, big, wide, names, true);
            cpu_box(&page, frame, small, wide, names, false);
        } else {
            cpu_box(&page, frame, big, wide, names, true);
            mem_box(&page, frame, small, wide, names, wide);
        }
        Rect::new(area.x, area.y + height, area.width, area.height - height)
    } else {
        summary_line(&page, frame, Rect::new(area.x, area.y, area.width, 1));
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    };
    if wide {
        let width = (rest.width * 36 / 100).clamp(56, 80);
        let table = Rect::new(rest.x, rest.y, rest.width - width, rest.height);
        table_box(&page, frame, table);
        detail_column(
            &page,
            frame,
            Rect::new(table.right(), rest.y, width, rest.height),
        );
    } else if medium && area.height >= 32 {
        let height = (rest.height / 3).clamp(9, 12);
        let table = Rect::new(rest.x, rest.y, rest.width, rest.height - height);
        table_box(&page, frame, table);
        detail_pair(
            &page,
            frame,
            Rect::new(rest.x, table.bottom(), rest.width, height),
        );
    } else {
        table_box(&page, frame, rest);
    }
}

// ---- formatting ----------------------------------------------------------

/// `96%`, for shares and axes. Adding zero turns `-0` into `0`.
fn percent(value: f64) -> String {
    format!("{:.0}%", value + 0.0)
}

/// A CPU reading, precise where rounding would hide it: `0%` only for a
/// true zero, `<0.1%` for an idle process, one decimal under 10%, then
/// whole percents: `0.3%`, `7.4%`, `84%`.
pub fn cpu_text(value: f64) -> String {
    match value + 0.0 {
        v if v <= 0.0 => "0%".to_owned(),
        v if v < 0.1 => "<0.1%".to_owned(),
        v if v < 10.0 => format!("{v:.1}%"),
        v => format!("{v:.0}%"),
    }
}

/// Bytes with one decimal under a gigabyte too (`494.4M`), for the detail's
/// statistics where whole mebibytes would read as a flat line.
pub fn bytes_fine(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let mib = bytes as f64 / MIB;
    if (1.0..1024.0).contains(&mib) {
        format!("{mib:.1}M")
    } else {
        format_bytes(bytes)
    }
}

/// Binary units with one decimal above a gigabyte.
pub fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let mib = bytes as f64 / MIB;
    if mib >= 1024.0 {
        format!("{:.1}G", mib / 1024.0)
    } else if mib >= 1.0 {
        format!("{mib:.0}M")
    } else {
        format!("{}K", bytes / 1024)
    }
}

/// Bytes in at most four cells for a graph's axis: `12G`, `6.2G`, `440M`.
fn axis_bytes(bytes: u64) -> String {
    let gib = bytes as f64 / (1u64 << 30) as f64;
    if gib >= 9.95 {
        format!("{gib:.0}G")
    } else if gib >= 1.0 {
        format!("{gib:.1}G")
    } else {
        format!("{:.0}M", bytes as f64 / (1u64 << 20) as f64)
    }
}

/// `41s`, `2h 14m`, `3d 2h`.
fn uptime(started: Option<u64>, now: u64) -> String {
    let Some(started) = started else {
        return "-".into();
    };
    let seconds = now.saturating_sub(started) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds / 3600 % 24),
    }
}

/// Raw column values over `scale`, for a graph that draws 0 to 1.
fn scaled(values: Vec<Option<f64>>, scale: f64) -> Vec<Option<f64>> {
    let scale = scale.max(f64::MIN_POSITIVE);
    values.into_iter().map(|v| v.map(|v| v / scale)).collect()
}

/// A memory scale at or above `bytes`, a power of two from 64M up (`512M`,
/// `1.0G`, `2.0G`), so the axis reads round binary numbers and grows in steps.
fn nice_bytes(bytes: f64) -> f64 {
    let mut scale = 64.0 * 1024.0 * 1024.0;
    while scale < bytes {
        scale *= 2.0;
    }
    scale
}

/// A style with the gradient color of `load`.
fn loaded(theme: &Theme, load: f64) -> Style {
    load_color(theme, load).map_or_else(Style::new, |c| Style::new().fg(c))
}

/// Numbers under half load stay in the default color, so the table is not a
/// wall of green.
fn number_style(theme: &Theme, load: f64) -> Style {
    if load < 0.5 {
        Style::new()
    } else {
        loaded(theme, load)
    }
}

/// Unit memory load: share of total memory with the stops at 0, 10, 20 and
/// 30%, so one 8 GB process on a 32 GB machine is red.
fn memory_load(bytes: u64, total: Option<u64>) -> Option<f64> {
    total
        .filter(|t| *t > 0)
        .map(|t| bytes as f64 / t as f64 / 0.3)
}

/// The inside of a box.
fn inside(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

// ---- the top boxes -------------------------------------------------------

/// One meter row: `acme@0   ■■■■·····  40%`.
fn meter_row(
    theme: &Theme,
    name: &str,
    name_width: usize,
    share: f64,
    cells: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw(format!(
        "{:<name_width$} ",
        clip(name, name_width)
    ))];
    spans.extend(graph::meter(theme, share, cells, &|load| {
        load_color(theme, load)
    }));
    spans.push(Span::styled(
        format!(" {:>4}", percent(share * 100.0)),
        number_style(theme, share),
    ));
    Line::from(spans)
}

/// `acme sh.` for `acme (shared)` where room is short.
/// Columns a mem row takes besides the name: ` ` before, the figure, the
/// meter and its percent, and room for `available 9.6G  pressure normal`.
const MEM_ROW_EXTRA: usize = 24;

/// Width for lane and group names in the cpu and mem boxes: the longest
/// (`clickmax (shared)`, `clickmax@1`), at least `available`, at most 22.
fn group_name_width(page: &Page) -> usize {
    page.groups()
        .iter()
        .map(|(name, _, _)| text::width(name))
        .chain([text::width("available")])
        .max()
        .unwrap_or(9)
        .min(22)
}

fn short_group(name: &str, width: usize) -> String {
    if text::width(name) <= width {
        return name.to_owned();
    }
    match name.strip_suffix(" (shared)") {
        Some(project) => format!("{} sh.", clip(project, width.saturating_sub(4))),
        None => clip(name, width),
    }
}

/// The y-axis labels and the x-axis row around a graph in `area`; returns
/// the plot area.
fn axes(page: &Page, frame: &mut Frame, area: Rect, labels: [&str; 3]) -> Rect {
    let theme = page.theme;
    if area.width < 12 || area.height < 3 {
        return Rect::default();
    }
    let plot = Rect::new(area.x + 5, area.y, area.width - 5, area.height - 1);
    let ascii = theme.glyphs == Glyphs::Ascii;
    let buf = frame.buffer_mut();
    for dy in 0..plot.height {
        let label = match dy {
            0 => labels[0],
            d if d == plot.height - 1 => labels[2],
            d if d == plot.height / 2 && plot.height > 3 => labels[1],
            _ => "",
        };
        let tick = match (label.is_empty(), ascii) {
            (true, true) => "|",
            (true, false) => "│",
            (false, true) => "+",
            (false, false) => "┤",
        };
        buf.set_string(area.x, area.y + dy, format!("{label:>4}"), theme.quiet());
        buf.set_string(area.x + 4, area.y + dy, tick, theme.rule());
    }
    // `└-3m 20s                          now`: two seconds per cell.
    let width = usize::from(plot.width);
    let start = format!("-{}", span(width * 2));
    let gap = width.saturating_sub(start.len() + 3);
    let corner = if ascii { "+" } else { "└" };
    let row = format!("{corner}{start}{}now", " ".repeat(gap));
    buf.set_string(
        area.x + 4,
        plot.bottom(),
        clip(&row, width + 1),
        theme.quiet(),
    );
    plot
}

/// Columns a cpu meter row takes besides the name: the meter and its percent.
const CPU_ROW_EXTRA: usize = 9 + 7;

/// One square per core colored by its load, as rows of `width` columns
/// under ` cores`.
fn core_lines(theme: &Theme, cores: &[f64], width: usize) -> Vec<Line<'static>> {
    if cores.is_empty() {
        return Vec::new();
    }
    let per_row = (width.saturating_sub(1) / 2).max(1);
    let mut lines = vec![Line::styled(" cores", theme.quiet())];
    for row in cores.chunks(per_row) {
        let mut spans = vec![Span::raw(" ")];
        for core in row {
            let load = core / 100.0;
            // Without color the square's height tells the load instead.
            let glyph = match (load_color(theme, load), theme.glyphs) {
                (Some(_), Glyphs::Unicode) => "■".to_owned(),
                (Some(_), Glyphs::Ascii) => "#".to_owned(),
                (None, glyphs) => graph::blocks(Some(load), glyphs).to_string(),
            };
            spans.push(Span::styled(glyph, loaded(theme, load)));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The cpu box: the devlanes total as a filled area with the system line
/// over it, and a meter per lane beside it. Without `graph` (memory has the
/// big box) the meters fill it, with a square per core under them.
fn cpu_box(page: &Page, frame: &mut Frame, area: Rect, wide: bool, names: usize, graph: bool) {
    let theme = page.theme;
    let (total_cpu, _) = page.total_now();
    let cpus = page.cpus();
    let system = page.metrics.and_then(|m| m.system.as_ref());
    let right = match cpus {
        Some(cpus) if wide => {
            let mut text = format!(" devlanes {}", percent(total_cpu / cpus));
            if let Some(cpu) = system.and_then(|s| s.cpu_percent) {
                text.push_str(&format!(" · system {}", percent(cpu)));
            }
            format!("{text} ")
        }
        Some(cpus) => format!(" of {cpus:.0} cores "),
        None => String::new(),
    };
    draw_box(
        page.app,
        frame,
        area,
        vec![Span::raw(" cpu ")],
        vec![Span::styled(right, theme.quiet())],
        false,
    );
    let inner = inside(area);
    let cells = if wide && graph { 16 } else { 9 };
    // Names shrink before the graph does, down to 8 columns.
    let graph_room = if graph { 20 } else { 0 };
    let room_for_names = usize::from(inner.width).saturating_sub(cells + 7 + graph_room);
    let name_width = names.min(room_for_names).max(8);
    let meters_width = (name_width + cells + 7) as u16;
    if inner.width < meters_width + graph_room as u16 || inner.height < 3 {
        return;
    }
    // Meters: each lane and shared group, then devlanes and the machine,
    // all as a share of every core.
    let share = |cpu: f64| cpus.map_or(0.0, |c| cpu / (c * 100.0));
    let mut lines: Vec<Line> = page
        .groups()
        .into_iter()
        .map(|(name, cpu, _)| {
            meter_row(
                theme,
                &short_group(&name, name_width),
                name_width,
                share(cpu),
                cells,
            )
        })
        .collect();
    if cpus.is_some() {
        lines.push(Line::from(""));
        lines.push(meter_row(
            theme,
            "devlanes",
            name_width,
            share(total_cpu),
            cells,
        ));
        if let Some(cpu) = system.and_then(|s| s.cpu_percent) {
            lines.push(meter_row(theme, "system", name_width, cpu / 100.0, cells));
        }
    }
    if !graph {
        let cores = system.map_or(&[][..], |s| s.core_percents.as_slice());
        let cores = core_lines(theme, cores, usize::from(meters_width));
        if !cores.is_empty() && lines.len() + 1 + cores.len() <= usize::from(inner.height) {
            lines.push(Line::from(""));
            lines.extend(cores);
        }
    }
    // The busiest rows, so a spike's cause shows without scrolling.
    let room = usize::from(inner.height).saturating_sub(lines.len() + 2);
    if room > 0 {
        let mut busiest = page.entries();
        busiest.retain(|e| e.metrics.cpu_percent.is_some());
        busiest.sort_by(|a, b| {
            b.metrics
                .cpu_percent
                .partial_cmp(&a.metrics.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if !busiest.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::styled("busiest", theme.quiet()));
            let width = usize::from(meters_width) - 6;
            for entry in busiest.into_iter().take(room.min(4)) {
                let cpu = entry.metrics.cpu_percent.unwrap_or(0.0);
                lines.push(Line::from(vec![
                    Span::raw(format!(" {:<width$}", clip(&entry.label, width))),
                    Span::styled(
                        format!("{:>5}", cpu_text(cpu)),
                        number_style(theme, cpu / 100.0),
                    ),
                ]));
            }
        }
    }
    let meters = Rect::new(
        inner.right() - meters_width,
        inner.y,
        meters_width,
        inner.height,
    );
    frame.render_widget(Paragraph::new(lines), meters);
    if !graph {
        return;
    }

    let graph_area = Rect::new(
        inner.x,
        inner.y,
        inner.width - meters_width - 1,
        inner.height,
    );
    let history = page.total_history();
    // Fixed at the machine's cores; without the core count, percent of one
    // core up to the highest total seen, rounded to the next 100.
    let peak = history.iter().map(|p| p.1).fold(total_cpu, f64::max);
    let scale = page.total_cpu_scale(peak);
    let labels = match cpus {
        Some(_) => ["100%".to_owned(), "50%".to_owned(), "0%".to_owned()],
        None => [percent(scale), percent(scale / 2.0), "0%".to_owned()],
    };
    let plot = axes(
        page,
        frame,
        graph_area,
        [&labels[0], &labels[1], &labels[2]],
    );
    if plot.is_empty() {
        return;
    }
    let samples = usize::from(plot.width) * 2;
    let cpu: Points = history.iter().map(|p| (p.0, p.1)).collect();
    let values = scaled(page.columns("cpu:devlanes", &cpu, samples, true), scale);
    let overlay = system.map_or_else(Vec::new, |s| {
        let line: Points = s.history.iter().map(|p| (p.0, p.1)).collect();
        scaled(page.columns("cpu:system", &line, samples, false), 100.0)
    });
    graph::braille(
        frame.buffer_mut(),
        plot,
        &values,
        &overlay,
        &|load| load_color(theme, load),
        theme.quiet(),
        theme.glyphs,
    );
}

/// The mem box: devlanes, used and available memory and the pressure, then
/// each lane; with `graph` (wide screens, or memory in the big box) the
/// devlanes total as a graph beside them.
fn mem_box(page: &Page, frame: &mut Frame, area: Rect, wide: bool, names: usize, graph: bool) {
    let theme = page.theme;
    let total = page.memory_total();
    let system = page.metrics.and_then(|m| m.system.as_ref());
    let pressure = system.and_then(|s| s.memory_pressure.clone());
    let right = match total {
        Some(total) if wide => format!(
            " {} · pressure {} ",
            format_bytes(total),
            pressure.as_deref().unwrap_or("unknown")
        ),
        Some(total) => format!(" of {} ", format_bytes(total)),
        None => String::new(),
    };
    let right = if right.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(right, theme.quiet())]
    };
    draw_box(
        page.app,
        frame,
        area,
        vec![Span::raw(" mem ")],
        right,
        false,
    );
    let inner = inside(area);
    let (_, devlanes) = page.total_now();
    let groups = page.groups();
    // Without the machine's total, bars are relative to the largest of them.
    let largest = groups
        .iter()
        .map(|g| g.2)
        .max()
        .unwrap_or(0)
        .max(devlanes)
        .max(1);
    let base = total.unwrap_or(largest).max(1) as f64;
    // Pressure `warn` makes memory at least orange, `critical` red.
    let floor = match pressure.as_deref() {
        Some("critical") => 1.0,
        Some("warn") => 0.8,
        _ => 0.0,
    };
    let cells = 9;
    // The names as wide as the box lets them be, then rows right-aligned.
    let name_width = names
        .min(usize::from(inner.width).saturating_sub(MEM_ROW_EXTRA))
        .max(9);
    let row = |name: &str, bytes: u64| {
        let share = bytes as f64 / base;
        let mut spans = vec![Span::raw(format!(
            " {:<name_width$}{:>6} ",
            clip(name, name_width),
            format_bytes(bytes)
        ))];
        spans.extend(graph::meter(theme, share, cells, &|load| {
            load_color(theme, load.max(floor))
        }));
        spans.push(Span::styled(
            format!(" {:>4}", percent(share * 100.0)),
            number_style(theme, share.max(floor)),
        ));
        Line::from(spans)
    };
    let mut lines = vec![row("devlanes", devlanes)];
    if let (Some(total), Some(available)) = (total, system.and_then(|s| s.available_memory_bytes)) {
        lines.push(row("used", total.saturating_sub(available)));
        let mut spans = vec![Span::raw(format!(
            " {:<name_width$}{:>6}  ",
            "available",
            format_bytes(available)
        ))];
        if let Some(pressure) = &pressure {
            let tone = match pressure.as_str() {
                "critical" => Tone::Failure,
                "warn" => Tone::Attention,
                _ => Tone::Quiet,
            };
            spans.push(Span::styled(
                format!("pressure {pressure}"),
                theme.tone(tone),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    for (name, _, bytes) in &groups {
        lines.push(row(&short_group(name, name_width), *bytes));
    }
    // ` available  9.6G  pressure critical`
    let meters_width = (name_width + cells + 18) as u16;
    if !graph || inner.width < meters_width + 24 {
        // Right-aligned: the figures line up with the box's right edge.
        let content = u16::try_from(name_width + MEM_ROW_EXTRA)
            .unwrap_or(u16::MAX)
            .min(inner.width);
        let right = Rect::new(inner.right() - content, inner.y, content, inner.height);
        frame.render_widget(Paragraph::new(lines), right);
        return;
    }
    // The three largest rows, where there is room under the meters.
    let room = usize::from(inner.height).saturating_sub(lines.len() + 1);
    if room > 1 {
        let mut largest = page.entries();
        largest.sort_by_key(|e| std::cmp::Reverse(e.metrics.memory_bytes.unwrap_or(0)));
        lines.push(Line::styled(" largest", theme.quiet()));
        let width = usize::from(meters_width) - 9;
        for entry in largest.into_iter().take((room - 1).min(3)) {
            lines.push(Line::from(format!(
                "  {:<width$}{:>6}",
                clip(&entry.label, width),
                format_bytes(entry.metrics.memory_bytes.unwrap_or(0))
            )));
        }
    }
    let meters = Rect::new(
        inner.right() - meters_width,
        inner.y,
        meters_width,
        inner.height,
    );
    frame.render_widget(Paragraph::new(lines), meters);
    let graph_area = Rect::new(
        inner.x,
        inner.y,
        inner.width - meters_width - 1,
        inner.height,
    );
    let history = page.total_history();
    let peak = history
        .iter()
        .map(|p| p.2)
        .max()
        .unwrap_or(devlanes)
        .max(devlanes)
        .max(1);
    // 0 to a round step above the highest total seen plus 20%; it only grows.
    let scale = page.sticky("mem:devlanes", nice_bytes(peak as f64 * 1.2));
    let labels = [
        axis_bytes(scale as u64),
        axis_bytes((scale / 2.0) as u64),
        "0".to_owned(),
    ];
    let plot = axes(
        page,
        frame,
        graph_area,
        [&labels[0], &labels[1], &labels[2]],
    );
    if plot.is_empty() {
        return;
    }
    let samples = usize::from(plot.width) * 2;
    let memory: Points = history.iter().map(|p| (p.0, p.2 as f64)).collect();
    let values = scaled(page.columns("mem:devlanes", &memory, samples, false), scale);
    let color = |row: f64| load_color(theme, (row * scale / base).max(floor));
    graph::braille(
        frame.buffer_mut(),
        plot,
        &values,
        &[],
        &color,
        theme.quiet(),
        theme.glyphs,
    );
}

/// The one-line summary for small screens:
/// ` cpu ▂▂▅▆▅▃ 52% sys 72%  mem 9.3G 29% · avail 9.1G · normal`.
fn summary_line(page: &Page, frame: &mut Frame, area: Rect) {
    let theme = page.theme;
    let (cpu, memory) = page.total_now();
    let scale = page.cpus().map_or(100.0, |c| c * 100.0);
    let history = page.total_history();
    let points: Points = history.iter().map(|p| (p.0, p.1)).collect();
    let peak = points.iter().map(|p| p.1).fold(cpu, f64::max);
    let graph_scale = page.total_cpu_scale(peak);
    let mut spans = vec![Span::styled(" cpu ", theme.quiet())];
    for value in scaled(page.columns("cpu:devlanes", &points, 24, true), graph_scale) {
        let style = match value {
            Some(v) if v <= 0.0 => theme.rule(),
            Some(v) => loaded(theme, v),
            None => Style::new(),
        };
        spans.push(Span::styled(
            graph::blocks(value, theme.glyphs).to_string(),
            style,
        ));
    }
    spans.push(Span::raw(format!(" {}", percent(cpu / scale * 100.0))));
    let system = page.metrics.and_then(|m| m.system.as_ref());
    if let Some(sys) = system.and_then(|s| s.cpu_percent) {
        spans.push(Span::styled(
            format!(" sys {}", percent(sys)),
            theme.quiet(),
        ));
    }
    spans.push(Span::styled("  mem ", theme.quiet()));
    spans.push(Span::raw(format_bytes(memory)));
    if let Some(total) = page.memory_total().filter(|t| *t > 0) {
        spans.push(Span::raw(format!(
            " {}",
            percent(memory as f64 / total as f64 * 100.0)
        )));
    }
    if let Some(available) = system.and_then(|s| s.available_memory_bytes) {
        spans.push(Span::styled(
            format!(" · avail {}", format_bytes(available)),
            theme.quiet(),
        ));
    }
    if let Some(pressure) = system.and_then(|s| s.memory_pressure.as_deref()) {
        spans.push(Span::styled(format!(" · {pressure}"), theme.quiet()));
    }
    frame.render_widget(Paragraph::new(fit(spans, usize::from(area.width))), area);
}

// ---- the table -----------------------------------------------------------

/// The table's columns; lower ones drop first when the width runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Name,
    Lane,
    Kind,
    State,
    Cpu,
    Mem,
    Peak,
    Uptime,
    Spark,
}

/// Columns that fit a width, with the bar size and the name width.
struct Layout {
    columns: Vec<Column>,
    bar: usize,
    name: usize,
    lane: usize,
    spark: usize,
}

impl Layout {
    fn width_of(&self, column: Column) -> usize {
        match column {
            Column::Name => self.name,
            // `<0.1%` and `7.4%` take five cells.
            Column::Cpu => 6 + self.bar,
            Column::Mem => 6 + self.bar,
            Column::Lane => self.lane,
            Column::Uptime => 6,
            Column::Spark => self.spark,
            Column::Peak => 5,
            Column::Kind => 4,
            Column::State => 8,
        }
    }

    /// Cells used by every column but the sparkline, with their gaps.
    fn used(&self) -> usize {
        self.columns
            .iter()
            .filter(|c| **c != Column::Spark)
            .map(|c| self.width_of(*c) + 1)
            .sum()
    }

    /// Picks columns for `width`, in monitor.md's order; the sparkline
    /// takes what is left. Bars shrink first (down to 4), then kind and
    /// peak go, then the name shrinks to 12, then columns drop by priority:
    /// the sparkline, uptime, lane, state, memory. State stays as long as
    /// anything can give way: it is the one column that says a unit is not
    /// running.
    fn fit(width: usize, flat: bool, longest: usize, lane: usize) -> Self {
        let mut columns = vec![
            Column::Name,
            Column::Lane,
            Column::Kind,
            Column::State,
            Column::Cpu,
            Column::Mem,
            Column::Peak,
            Column::Uptime,
            Column::Spark,
        ];
        if !flat {
            columns.retain(|c| *c != Column::Lane);
        }
        let mut layout = Self {
            columns,
            bar: 8,
            name: longest.clamp(12, 28),
            // `clickmax (shared)` and `clickmax@1` whole when they fit.
            lane: lane.clamp(6, 22),
            spark: 0,
        };
        let room = |l: &Layout| l.used() + 13 <= width;
        for bar in [8, 6, 5, 4] {
            layout.bar = bar;
            if room(&layout) {
                break;
            }
        }
        // Kind and peak go before the name shrinks; the name shrinks to 12
        // before anything that matters more goes.
        for column in [Column::Kind, Column::Peak] {
            if room(&layout) {
                break;
            }
            layout.columns.retain(|c| *c != column);
        }
        while !room(&layout) && layout.name > 12 {
            layout.name -= 1;
        }
        for column in [
            Column::Spark,
            Column::Uptime,
            Column::Lane,
            Column::State,
            Column::Mem,
        ] {
            if room(&layout) {
                break;
            }
            layout.columns.retain(|c| *c != column);
        }
        let left = width.saturating_sub(layout.used());
        if layout.columns.contains(&Column::Spark) && left >= 9 {
            layout.spark = left - 1;
        } else {
            layout.columns.retain(|c| *c != Column::Spark);
        }
        layout
    }
}

/// The table box: the count, filter and grouping in its title, the
/// `<- cpu ->` selector on the right, a header row, then the rows.
fn table_box(page: &Page, frame: &mut Frame, area: Rect) {
    let app = page.app;
    let theme = page.theme;
    let view = &app.monitor;
    let shown = page.entries().len();
    let all = page.units().len().max(shown);
    let mut title = vec![Span::raw(" units")];
    if view.editing || !view.filter.is_empty() {
        title.push(Span::raw(format!(" /{}", clean(&view.filter))));
        if view.editing {
            title.push(Span::styled("▏", theme.focus()));
        }
    }
    title.push(Span::styled(format!(" {shown} of {all}"), theme.quiet()));
    title.push(Span::styled(
        if view.flat {
            " · flat "
        } else {
            " · by lane "
        },
        theme.quiet(),
    ));
    let selector = vec![
        Span::styled(" <- ", theme.quiet()),
        Span::styled(view.sort.label(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" -> ", theme.quiet()),
    ];
    draw_box(app, frame, area, title, selector, false);
    let inner = inside(area);
    if inner.height < 2 || inner.width < 20 {
        return;
    }
    let width = usize::from(inner.width);
    let entries = page.entries();
    // Unit names with their glyph, and lane rows with their branch.
    let longest = page
        .rows
        .iter()
        .map(|row| match row {
            MonitorRow::Entry(e) => text::width(&e.label) + 4,
            MonitorRow::Group(g) => {
                text::width(&g.key) + g.branch.as_deref().map_or(0, |b| text::width(b) + 2) + 2
            }
        })
        .max()
        .unwrap_or(12);
    let lane_width = entries
        .iter()
        .map(|e| text::width(&e.lane))
        .max()
        .unwrap_or(6);
    let layout = Layout::fit(width, view.flat, longest, lane_width);
    frame.render_widget(
        Paragraph::new(header_line(page, &layout)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    let height = usize::from(body.height);
    if page.rows.is_empty() {
        let text = if app.lanes.is_empty() {
            "No lanes yet."
        } else {
            "Nothing running."
        };
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {text}"), theme.quiet())),
            body,
        );
        return;
    }
    let index = view.index.min(page.rows.len() - 1);
    app.monitor_table_rows.set(height);
    let offset = view.offset.min(page.rows.len().saturating_sub(height));
    // Memory bars are relative to the largest row in view.
    let largest = entries
        .iter()
        .filter_map(|e| e.metrics.memory_bytes)
        .max()
        .unwrap_or(1)
        .max(1);
    for (dy, (row_index, row)) in page
        .rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .enumerate()
    {
        let rect = Rect::new(body.x, body.y + dy as u16, body.width, 1);
        let line = match row {
            MonitorRow::Group(group) => group_line(page, &layout, group),
            MonitorRow::Entry(entry) => entry_line(page, &layout, entry, largest),
        };
        let mut spans = fit(line.spans, width).spans;
        if row_index == index {
            // monitor.md: a rule-colored background, bold text and a paint
            // mark in the first column, so bars and sparklines keep their
            // colors; reverse video only without color.
            let background = theme.rule().fg.filter(|_| theme.tier != Tier::Plain);
            let select = |style: Style| match background {
                Some(bg) => style.bg(bg).add_modifier(Modifier::BOLD),
                None => theme.selected(style).add_modifier(Modifier::BOLD),
            };
            spans = spans
                .into_iter()
                .map(|s| Span::styled(s.content, select(s.style)))
                .collect();
            // Unit rows start with a blank cell for it; a lane row's fold
            // arrow stays.
            if let Some(first) = spans.first_mut().filter(|s| s.content.starts_with(' ')) {
                let rest: String = first.content.chars().skip(1).collect();
                let style = first.style;
                spans.splice(
                    0..1,
                    [
                        Span::styled(theme.glyph(Glyph::Marker), select(theme.focus())),
                        Span::styled(rest, style),
                    ],
                );
            }
            let pad = width.saturating_sub(spans_width(&spans));
            spans.push(Span::styled(" ".repeat(pad), select(Style::new())));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        hit(app, rect, Click::MonitorRow(row.id()));
    }
}

/// `  name   lane  kind state   cpu ▼   mem   peak uptime cpu 15m`.
fn header_line(page: &Page, layout: &Layout) -> Line<'static> {
    let view = &page.app.monitor;
    let arrow = if view.descending() { " ▼" } else { " ▲" };
    let mark = |key: SortKey, text: &str| {
        if view.sort == key {
            format!("{text}{arrow}")
        } else {
            text.to_owned()
        }
    };
    let spans = layout
        .columns
        .iter()
        .map(|column| {
            let width = layout.width_of(*column);
            let (text, right) = match column {
                Column::Name => (mark(SortKey::Name, "  name"), false),
                Column::Cpu => (mark(SortKey::Cpu, "cpu"), false),
                Column::Mem => (mark(SortKey::Mem, "mem"), false),
                Column::Lane => (mark(SortKey::Lane, "lane"), false),
                Column::Uptime => (mark(SortKey::Uptime, "uptime"), true),
                // One cell per second: `cpu -40s`.
                Column::Spark => (format!("cpu -{}", span(width)), false),
                Column::Peak => (mark(SortKey::Peak, "peak"), true),
                Column::Kind => (mark(SortKey::Kind, "kind"), false),
                Column::State => (mark(SortKey::State, "state"), false),
            };
            let text = clip(&text, width);
            let cell = if right {
                format!("{text:>width$} ")
            } else {
                format!("{text:<width$} ")
            };
            Span::styled(cell, page.theme.quiet())
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// A history's CPU as a colored sparkline, one column per cell pinned to the
/// clock like the graphs, against `scale` (one core for a unit, the machine
/// for a lane). `key` names the series for the column cache.
fn sparkline(
    page: &Page,
    key: &str,
    history: &[(u64, f64, u64)],
    width: usize,
    scale: f64,
) -> Vec<Span<'static>> {
    let points: Points = history.iter().map(|p| (p.0, p.1)).collect();
    scaled(page.columns(key, &points, width, true), scale)
        .into_iter()
        .map(|value| {
            // Zero is a dim baseline; a missing sample stays blank.
            let style = match value {
                Some(v) if v <= 0.0 => page.theme.rule(),
                Some(v) => loaded(page.theme, v),
                None => Style::new(),
            };
            Span::styled(graph::blocks(value, page.theme.glyphs).to_string(), style)
        })
        .collect()
}

/// A unit or container row.
fn entry_line(page: &Page, layout: &Layout, entry: &Entry, largest: u64) -> Line<'static> {
    let theme = page.theme;
    let (glyph, tone) = match (&entry.summary, &entry.container) {
        (Some(unit), _) => {
            let look = unit_look(theme, unit, page.app.spin);
            (look.glyph, look.tone)
        }
        (None, Some(container)) => {
            let (glyph, tone, _) = super::list::container_look(page.app, container);
            (glyph, tone)
        }
        (None, None) => (theme.glyph(Glyph::Ready), Tone::Ok),
    };
    let indent = if page.app.monitor.flat { "" } else { "  " };
    let total = page.memory_total();
    let mut spans = Vec::new();
    for column in &layout.columns {
        let width = layout.width_of(*column);
        match column {
            Column::Name => {
                let name_width = width.saturating_sub(indent.len() + 2);
                spans.push(Span::raw(indent));
                spans.push(Span::styled(format!("{glyph} "), theme.tone(tone)));
                spans.push(Span::raw(format!(
                    "{:<name_width$} ",
                    clip(&entry.label, name_width)
                )));
            }
            Column::Cpu => match entry.metrics.cpu_percent {
                Some(cpu) => {
                    // One full core fills the bar.
                    let load = cpu / 100.0;
                    spans.push(Span::styled(
                        format!("{:>5} ", cpu_text(cpu)),
                        number_style(theme, load),
                    ));
                    spans.extend(graph::meter(theme, load, layout.bar, &|l| {
                        load_color(theme, l)
                    }));
                    spans.push(Span::raw(" "));
                }
                None => spans.push(Span::styled(format!("{:>width$} ", "-"), theme.quiet())),
            },
            Column::Mem => match entry.metrics.memory_bytes {
                Some(bytes) => {
                    let style = memory_load(bytes, total)
                        .map_or_else(Style::new, |l| number_style(theme, l));
                    spans.push(Span::styled(format!("{:>5} ", format_bytes(bytes)), style));
                    let full = memory_load(largest, total);
                    spans.extend(graph::meter(
                        theme,
                        bytes as f64 / largest as f64,
                        layout.bar,
                        &|position| full.and_then(|full| load_color(theme, position * full)),
                    ));
                    spans.push(Span::raw(" "));
                }
                None => spans.push(Span::styled(format!("{:>width$} ", "-"), theme.quiet())),
            },
            Column::Lane => spans.push(Span::styled(
                format!("{:<width$} ", short_group(&entry.lane, width)),
                theme.quiet(),
            )),
            Column::Uptime => {
                let text = if entry.is_container() {
                    "-".into()
                } else {
                    uptime(entry.started_at_ms, page.now)
                };
                spans.push(Span::raw(format!("{text:>width$} ")));
            }
            Column::Spark => {
                let key = format!("spark:{}:{}", entry.lane, entry.unit);
                spans.extend(sparkline(page, &key, &entry.metrics.history, width, 100.0));
                spans.push(Span::raw(" "));
            }
            Column::Peak => {
                let text = entry.peak().map_or_else(|| "-".into(), format_bytes);
                spans.push(Span::styled(format!("{text:>width$} "), theme.quiet()));
            }
            Column::Kind => spans.push(Span::styled(
                format!("{:<width$} ", entry.kind),
                theme.quiet(),
            )),
            Column::State => {
                let style = match tone {
                    Tone::Failure | Tone::Attention => theme.tone(tone),
                    _ => theme.quiet(),
                };
                spans.push(Span::styled(
                    format!("{:<width$} ", clip(&entry.state, width)),
                    style,
                ));
            }
        }
    }
    Line::from(spans)
}

/// A lane's header row: totals in bold without bars (one core is the wrong
/// scale for a lane), and the lane's sparkline.
fn group_line(page: &Page, layout: &Layout, group: &Group) -> Line<'static> {
    let theme = page.theme;
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let arrow = theme.glyph(if group.folded {
        Glyph::Folded
    } else {
        Glyph::Open
    });
    let mut spans = Vec::new();
    for column in &layout.columns {
        let width = layout.width_of(*column);
        match column {
            Column::Name => {
                let mut name = clean(&group.key);
                if let Some(branch) = &group.branch {
                    name.push_str(&format!("  {}", clean(branch)));
                }
                if group.folded {
                    name.push_str(&format!(" · {}", group.count));
                }
                let name_width = width.saturating_sub(2);
                spans.push(Span::styled(format!("{arrow} "), theme.quiet()));
                spans.push(Span::styled(
                    format!("{:<name_width$} ", clip(&name, name_width)),
                    bold,
                ));
            }
            Column::Cpu => {
                let text = group.cpu_percent.map_or_else(|| "-".into(), cpu_text);
                spans.push(Span::styled(
                    format!("{text:>5} {} ", " ".repeat(layout.bar)),
                    bold,
                ));
            }
            Column::Mem => {
                let text = group.memory_bytes.map_or_else(|| "-".into(), format_bytes);
                spans.push(Span::styled(
                    format!("{text:>5} {} ", " ".repeat(layout.bar)),
                    bold,
                ));
            }
            Column::Spark => {
                // A lane against the whole machine; without the core count,
                // against the highest it reached, a scale that only grows.
                let key = format!("spark:{}", group.key);
                let scale = page.cpus().map_or_else(
                    || {
                        let peak = group.history.iter().map(|p| p.1).fold(100.0, f64::max);
                        page.sticky(&key, peak)
                    },
                    |cpus| cpus * 100.0,
                );
                spans.extend(sparkline(page, &key, &group.history, width, scale));
                spans.push(Span::raw(" "));
            }
            Column::State if group.shared => {
                spans.push(Span::styled(
                    format!("{:<width$} ", "shared"),
                    theme.quiet(),
                ));
            }
            _ => spans.push(Span::raw(" ".repeat(width + 1))),
        }
    }
    Line::from(spans)
}

// ---- detail graphs -------------------------------------------------------

/// The selected row's series and what the detail says about it.
struct Series {
    /// Names the series for the column cache and its sticky scales.
    key: String,
    name: String,
    place: String,
    status: Vec<String>,
    history: Vec<(u64, f64, u64)>,
    /// The current CPU and memory readings; `None` when the daemon had none.
    now: (Option<f64>, Option<u64>),
    peaks: (Option<f64>, Option<u64>),
}

fn selected_series(page: &Page) -> Option<Series> {
    let row = page.rows.get(page.app.monitor.index)?;
    Some(match row {
        MonitorRow::Entry(entry) => {
            let mut status = vec![
                match entry.kind {
                    "svc" => "service".to_owned(),
                    "ctr" => "container".to_owned(),
                    other => other.to_owned(),
                },
                entry.state.clone(),
            ];
            if let Some(pid) = entry.summary.as_ref().and_then(|u| u.pid) {
                status.push(format!("pid {pid}"));
            }
            if entry.started_at_ms.is_some() {
                status.push(format!("up {}", uptime(entry.started_at_ms, page.now)));
            }
            if let Some(name) = entry
                .metrics
                .container
                .as_ref()
                .filter(|_| entry.is_container())
            {
                status.push(clean(name));
            }
            if entry.metrics.shared {
                status.push(format!("shared by {}", entry.metrics.lanes.join(", ")));
            }
            Series {
                key: format!("{}:{}", entry.lane, entry.unit),
                name: entry.label.clone(),
                place: entry.lane.clone(),
                status,
                history: entry.metrics.history.clone(),
                now: (entry.metrics.cpu_percent, entry.metrics.memory_bytes),
                peaks: (
                    entry.metrics.peak_cpu_percent,
                    entry.metrics.peak_memory_bytes,
                ),
            }
        }
        MonitorRow::Group(group) => Series {
            key: format!("lane:{}", group.key),
            name: group.key.clone(),
            place: group.branch.clone().unwrap_or_default(),
            status: vec![
                if group.shared {
                    "shared infra"
                } else {
                    "lane total"
                }
                .to_owned(),
            ],
            history: group.history.clone(),
            now: (group.cpu_percent, group.memory_bytes),
            peaks: (group.peak_cpu_percent, group.peak_memory_bytes),
        },
    })
}

/// `now 96%  avg 41%  min 2%  max 138%` over the history; `now` is the
/// current reading, `-` when the daemon had none this second.
fn stats(values: &[f64], now: Option<f64>, show: &dyn Fn(f64) -> String) -> String {
    if values.is_empty() {
        return "Collecting samples…".into();
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    format!(
        "now {}  avg {}  min {}  max {}",
        now.map_or_else(|| "-".to_owned(), show),
        show(avg),
        show(min),
        show(max)
    )
}

fn cpu_stats(series: &Series) -> String {
    let values: Vec<f64> = series.history.iter().map(|p| p.1).collect();
    stats(&values, series.now.0, &cpu_text)
}

fn mem_stats(series: &Series) -> String {
    let values: Vec<f64> = series.history.iter().map(|p| p.2 as f64).collect();
    stats(&values, series.now.1.map(|m| m as f64), &|v| {
        bytes_fine(v as u64)
    })
}

/// A series' CPU with its axes, one sample per dot column.
fn cpu_graph(page: &Page, frame: &mut Frame, area: Rect, series: &Series) {
    let theme = page.theme;
    // One core (100%) fixed; a series that used more grows the scale in
    // whole cores, and it stays there.
    let peak = series.history.iter().map(|p| p.1).fold(0.0, f64::max);
    let scale = page.sticky(
        &format!("cpu:{}", series.key),
        ((peak / 100.0).ceil() * 100.0).max(100.0),
    );
    let labels = [percent(scale), percent(scale / 2.0), "0%".to_owned()];
    let plot = axes(page, frame, area, [&labels[0], &labels[1], &labels[2]]);
    if plot.is_empty() {
        return;
    }
    let samples = usize::from(plot.width) * 2;
    let fill: Points = series.history.iter().map(|p| (p.0, p.1)).collect();
    let key = format!("detail-cpu:{}", series.key);
    let values = scaled(page.columns(&key, &fill, samples, true), scale);
    // 100% is one full core, red, whatever the scale.
    graph::braille(
        frame.buffer_mut(),
        plot,
        &values,
        &[],
        &|row| load_color(theme, row * scale / 100.0),
        theme.quiet(),
        theme.glyphs,
    );
}

/// A series' memory from 0 to a round step above the highest it reached;
/// the scale only grows, so a column keeps its height once drawn.
fn mem_graph(page: &Page, frame: &mut Frame, area: Rect, series: &Series) {
    let theme = page.theme;
    let Some(high) = series.history.iter().map(|p| p.2).max() else {
        return;
    };
    let top = page.sticky(
        &format!("mem:{}", series.key),
        nice_bytes(high as f64 * 1.2),
    );
    let labels = [
        axis_bytes(top as u64),
        axis_bytes((top / 2.0) as u64),
        "0".to_owned(),
    ];
    let plot = axes(page, frame, area, [&labels[0], &labels[1], &labels[2]]);
    if plot.is_empty() {
        return;
    }
    let samples = usize::from(plot.width) * 2;
    let points: Points = series.history.iter().map(|p| (p.0, p.2 as f64)).collect();
    let key = format!("detail-mem:{}", series.key);
    let values = scaled(page.columns(&key, &points, samples, false), top);
    let total = page.memory_total();
    let color = |row: f64| {
        let load = memory_load((row * top) as u64, total).unwrap_or(0.0);
        load_color(theme, load)
    };
    graph::braille(
        frame.buffer_mut(),
        plot,
        &values,
        &[],
        &color,
        theme.quiet(),
        theme.glyphs,
    );
}

/// `api-search · acme@0` for a detail title.
fn detail_title(page: &Page, series: &Series, suffix: &str) -> Vec<Span<'static>> {
    let mut title = vec![Span::styled(
        format!(" {}", clean(&series.name)),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if !series.place.is_empty() {
        title.push(Span::styled(
            format!(" · {}", clean(&series.place)),
            page.theme.quiet(),
        ));
    }
    title.push(Span::styled(format!("{suffix} "), page.theme.quiet()));
    title
}

/// Text on row `y` of `area`, cut to its width.
fn text_row(frame: &mut Frame, area: Rect, y: u16, text: &str, style: Style) {
    if y < area.bottom() {
        frame
            .buffer_mut()
            .set_string(area.x, y, clip(text, usize::from(area.width)), style);
    }
}

/// The detail column (wide screens and `z`): the status, the cpu and mem
/// graphs with their stats, and the peaks since the lane started.
fn detail_column(page: &Page, frame: &mut Frame, area: Rect) {
    let theme = page.theme;
    let Some(series) = selected_series(page) else {
        draw_box(
            page.app,
            frame,
            area,
            vec![Span::raw(" detail ")],
            Vec::new(),
            false,
        );
        return;
    };
    draw_box(
        page.app,
        frame,
        area,
        detail_title(page, &series, ""),
        Vec::new(),
        false,
    );
    let inner = inside(area);
    if inner.height < 8 {
        return;
    }
    text_row(
        frame,
        inner,
        inner.y,
        &series.status.join(" · "),
        Style::new(),
    );
    // Two graphs share what the status, the stats and the peaks leave.
    let each = inner.height.saturating_sub(6) / 2;
    let cpu_top = inner.y + 2;
    text_row(
        frame,
        inner,
        cpu_top,
        &format!("cpu   {}", cpu_stats(&series)),
        Style::new(),
    );
    cpu_graph(
        page,
        frame,
        Rect::new(inner.x, cpu_top + 1, inner.width, each),
        &series,
    );
    let mem_top = cpu_top + 2 + each;
    text_row(
        frame,
        inner,
        mem_top,
        &format!("mem   {}", mem_stats(&series)),
        Style::new(),
    );
    mem_graph(
        page,
        frame,
        Rect::new(inner.x, mem_top + 1, inner.width, each),
        &series,
    );
    let (cpu, memory) = series.peaks;
    if cpu.is_some() || memory.is_some() {
        let mut line = "since start".to_owned();
        if let Some(cpu) = cpu {
            line.push_str(&format!("   cpu peak {}", cpu_text(cpu)));
        }
        if let Some(memory) = memory {
            line.push_str(&format!("   mem peak {}", bytes_fine(memory)));
        }
        text_row(frame, inner, inner.bottom() - 1, &line, theme.quiet());
    }
}

/// The detail under the table: a cpu box and a mem box side by side.
fn detail_pair(page: &Page, frame: &mut Frame, area: Rect) {
    let theme = page.theme;
    let Some(series) = selected_series(page) else {
        return;
    };
    let half = area.width / 2;
    let left = Rect::new(area.x, area.y, half, area.height);
    let right = Rect::new(area.x + half, area.y, area.width - half, area.height);
    draw_box(
        page.app,
        frame,
        left,
        detail_title(page, &series, " · cpu"),
        Vec::new(),
        false,
    );
    let state = series
        .status
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    draw_box(
        page.app,
        frame,
        right,
        vec![Span::raw(" mem ")],
        vec![Span::styled(format!(" {state} "), theme.quiet())],
        false,
    );
    for (rect, line, cpu) in [
        (left, cpu_stats(&series), true),
        (right, mem_stats(&series), false),
    ] {
        let inner = inside(rect);
        if inner.height < 4 {
            continue;
        }
        text_row(frame, inner, inner.y, &line, Style::new());
        let graph_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        if cpu {
            cpu_graph(page, frame, graph_area, &series);
        } else {
            mem_graph(page, frame, graph_area, &series);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_unit_reads_as_idle_not_as_zero() {
        assert_eq!(cpu_text(0.0), "0%");
        assert_eq!(cpu_text(0.03), "<0.1%");
        assert_eq!(cpu_text(0.34), "0.3%");
        assert_eq!(cpu_text(7.44), "7.4%");
        assert_eq!(cpu_text(84.4), "84%");
        assert_eq!(cpu_text(-0.0), "0%");
        // Memory that moves within a mebibyte still shows the movement.
        assert_eq!(bytes_fine(518_412_288), "494.4M");
        assert_eq!(bytes_fine(518_936_576), "494.9M");
        assert_eq!(bytes_fine(11 << 30), "11.0G");
        assert_eq!(format_bytes(518_412_288), "494M");
    }

    #[test]
    fn the_state_column_stays_at_80_columns() {
        // 80x24: the table's inside is 78 wide.
        let layout = Layout::fit(78, false, 16, 6);
        assert!(
            layout.columns.contains(&Column::State),
            "{:?}",
            layout.columns
        );
        assert!(layout.columns.contains(&Column::Cpu));
        assert!(layout.columns.contains(&Column::Mem));
        // Kind gave way first.
        assert!(!layout.columns.contains(&Column::Kind));
        // Narrower still, the name shrinks and the sparkline goes before state.
        let narrow = Layout::fit(60, false, 16, 6);
        assert!(
            narrow.columns.contains(&Column::State),
            "{:?}",
            narrow.columns
        );
        assert!(!narrow.columns.contains(&Column::Spark));
        // Wide enough, everything shows with full bars.
        let wide = Layout::fit(160, true, 16, 10);
        assert_eq!(wide.columns.len(), 9);
        assert_eq!(wide.bar, 8);
    }

    #[test]
    fn memory_scales_are_round_binary_steps() {
        const MIB: f64 = 1024.0 * 1024.0;
        assert_eq!(nice_bytes(10.0 * MIB), 64.0 * MIB);
        assert_eq!(nice_bytes(500.0 * MIB), 512.0 * MIB);
        assert_eq!(nice_bytes(600.0 * MIB), 1024.0 * MIB);
        assert_eq!(axis_bytes(nice_bytes(600.0 * MIB) as u64), "1.0G");
    }

    #[test]
    fn spans_name_how_far_back_a_graph_reaches() {
        assert_eq!(span(45), "45s");
        assert_eq!(span(120), "2m");
        assert_eq!(span(200), "3m 20s");
        assert_eq!(span(3720), "1h 2m");
    }
}
