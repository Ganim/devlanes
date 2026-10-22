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
use crate::api::{Metrics, UnitMetrics, Window};
use crate::app::{App, Click};
use crate::graph::{self, load_color};
use crate::monitor::{Entry, Group, MonitorRow, RAW_RANGE_MS, SortKey};
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
    range: u64,
    label: &'static str,
}

impl Page<'_> {
    fn since(&self) -> u64 {
        self.now.saturating_sub(self.range)
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
    let (range, label) = app.monitor.range();
    let page = Page {
        app,
        theme: &app.theme,
        metrics,
        rows: app.monitor_rows(),
        now: newest(metrics),
        range: metrics.and_then(|m| m.range_ms).unwrap_or(range),
        label,
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
        let cpu_width = area.width * 3 / 5;
        cpu_box(
            &page,
            frame,
            Rect::new(area.x, area.y, cpu_width, height),
            wide,
        );
        mem_box(
            &page,
            frame,
            Rect::new(area.x + cpu_width, area.y, area.width - cpu_width, height),
            wide,
        );
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

/// `96%`. Adding zero turns `-0` into `0`.
fn percent(value: f64) -> String {
    format!("{:.0}%", value + 0.0)
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
    // `└-15m                          now`
    let width = usize::from(plot.width);
    let start = format!("-{}", page.label);
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

/// The cpu box: the devlanes total as a filled area with the system line
/// over it, and a meter per lane beside it.
fn cpu_box(page: &Page, frame: &mut Frame, area: Rect, wide: bool) {
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
            format!("{text} · {} ", page.label)
        }
        Some(cpus) => format!(" of {cpus:.0} cores · {} ", page.label),
        None => format!(" {} ", page.label),
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
    let (name_width, cells) = if wide { (11, 16) } else { (8, 9) };
    let meters_width = (name_width + cells + 7) as u16;
    if inner.width < meters_width + 20 || inner.height < 3 {
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
                        format!("{:>5}", percent(cpu)),
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

    let graph_area = Rect::new(
        inner.x,
        inner.y,
        inner.width - meters_width - 1,
        inner.height,
    );
    let history = page.total_history();
    let (scale, labels) = match cpus {
        Some(cpus) => (
            cpus * 100.0,
            ["100%".to_owned(), "50%".to_owned(), "0%".to_owned()],
        ),
        // Without the core count: percent of one core, up to the peak
        // rounded to the next 100.
        None => {
            let peak = history.iter().map(|p| p.1).fold(total_cpu, f64::max);
            let scale = ((peak / 100.0).ceil() * 100.0).max(100.0);
            (
                scale,
                [percent(scale), percent(scale / 2.0), "0%".to_owned()],
            )
        }
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
    let cpu: Vec<(u64, f64)> = history.iter().map(|p| (p.0, p.1 / scale)).collect();
    let values = graph::buckets(&cpu, page.since(), page.range, samples, true);
    let overlay = system.map_or_else(Vec::new, |s| {
        let line: Vec<(u64, f64)> = s.history.iter().map(|p| (p.0, p.1 / 100.0)).collect();
        graph::buckets(&line, page.since(), page.range, samples, false)
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
/// each lane; on wide screens the devlanes total as a graph beside them.
fn mem_box(page: &Page, frame: &mut Frame, area: Rect, wide: bool) {
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
    let (cells, name_width) = (9, 9);
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
    if !wide || inner.width < meters_width + 24 {
        frame.render_widget(Paragraph::new(lines), inner);
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
    // 0 to the peak in range plus 20%.
    let scale = peak as f64 * 1.2;
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
    let memory: Vec<(u64, f64)> = history.iter().map(|p| (p.0, p.2 as f64 / scale)).collect();
    let values = graph::buckets(&memory, page.since(), page.range, samples, false);
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
    let points: Vec<(u64, f64)> = history.iter().map(|p| (p.0, p.1 / scale)).collect();
    let mut spans = vec![Span::styled(" cpu ", theme.quiet())];
    for value in graph::buckets(&points, page.since(), page.range, 24, true) {
        spans.push(Span::styled(
            graph::blocks(value, theme.glyphs).to_string(),
            loaded(theme, value.unwrap_or(0.0)),
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
            Column::Cpu => 5 + self.bar,
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
    /// takes what is left. Bars shrink first, then the name down to 12,
    /// then columns drop by priority: state, kind, peak, the sparkline,
    /// uptime, lane, memory.
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
            lane: lane.clamp(6, 16),
            spark: 0,
        };
        let room = |l: &Layout| l.used() + 13 <= width;
        for bar in [8, 6, 5] {
            layout.bar = bar;
            if room(&layout) {
                break;
            }
        }
        // State, kind and peak go before the name shrinks; the name shrinks
        // to 12 before anything that matters more goes.
        for column in [Column::State, Column::Kind, Column::Peak] {
            if room(&layout) {
                break;
            }
            layout.columns.retain(|c| *c != column);
        }
        while !room(&layout) && layout.name > 12 {
            layout.name -= 1;
        }
        for column in [Column::Spark, Column::Uptime, Column::Lane, Column::Mem] {
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
    let offset = (index + 1).saturating_sub(height);
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
                Column::Spark => (format!("cpu {}", page.label), false),
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

/// A history's CPU as a colored sparkline, one bucket per cell, against
/// `scale` (one core for a unit, the machine for a lane).
fn sparkline(
    page: &Page,
    history: &[(u64, f64, u64)],
    width: usize,
    scale: f64,
) -> Vec<Span<'static>> {
    let points: Vec<(u64, f64)> = history.iter().map(|p| (p.0, p.1 / scale)).collect();
    graph::buckets(&points, page.since(), page.range, width, true)
        .into_iter()
        .map(|value| {
            Span::styled(
                graph::blocks(value, page.theme.glyphs).to_string(),
                loaded(page.theme, value.unwrap_or(0.0)),
            )
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
                        format!("{:>4} ", percent(cpu)),
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
                format!("{:<width$} ", clip(&entry.lane, width)),
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
                spans.extend(sparkline(page, &entry.metrics.history, width, 100.0));
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
                let text = group.cpu_percent.map_or_else(|| "-".into(), percent);
                spans.push(Span::styled(
                    format!("{text:>4} {} ", " ".repeat(layout.bar)),
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
                // against its own peak.
                let scale = page.cpus().map_or_else(
                    || group.history.iter().map(|p| p.1).fold(100.0, f64::max),
                    |cpus| cpus * 100.0,
                );
                spans.extend(sparkline(page, &group.history, width, scale));
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
    name: String,
    place: String,
    status: Vec<String>,
    history: Vec<(u64, f64, u64)>,
    windows: Vec<Window>,
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
                name: entry.label.clone(),
                place: entry.lane.clone(),
                status,
                history: entry.metrics.history.clone(),
                windows: entry.metrics.windows.clone(),
                peaks: (
                    entry.metrics.peak_cpu_percent,
                    entry.metrics.peak_memory_bytes,
                ),
            }
        }
        MonitorRow::Group(group) => Series {
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
            windows: page
                .metrics
                .and_then(|m| m.lanes.iter().find(|l| l.lane == group.key))
                .map(|l| l.windows.clone())
                .unwrap_or_default(),
            peaks: (group.peak_cpu_percent, group.peak_memory_bytes),
        },
    })
}

/// `now 96%  avg 41%  min 2%  max 138%` over the range.
fn stats(values: &[f64], show: &dyn Fn(f64) -> String) -> String {
    let Some(last) = values.last() else {
        return "Collecting samples…".into();
    };
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    format!(
        "now {}  avg {}  min {}  max {}",
        show(*last),
        show(avg),
        show(min),
        show(max)
    )
}

fn cpu_stats(series: &Series) -> String {
    let values: Vec<f64> = series.history.iter().map(|p| p.1).collect();
    stats(&values, &percent)
}

fn mem_stats(series: &Series) -> String {
    let values: Vec<f64> = series.history.iter().map(|p| p.2 as f64).collect();
    stats(&values, &|v| format_bytes(v as u64))
}

/// A series' CPU with its axes. Past the raw range the area is each
/// bucket's average and its maximum a dim dot above, so a short spike
/// shows without the graph reading as pegged.
fn cpu_graph(page: &Page, frame: &mut Frame, area: Rect, series: &Series) {
    let theme = page.theme;
    let windows = page.range > RAW_RANGE_MS && !series.windows.is_empty();
    let peak = series.history.iter().map(|p| p.1).fold(0.0, f64::max);
    let scale = ((peak / 50.0).ceil() * 50.0).max(100.0);
    let labels = [percent(scale), percent(scale / 2.0), "0%".to_owned()];
    let plot = axes(page, frame, area, [&labels[0], &labels[1], &labels[2]]);
    if plot.is_empty() {
        return;
    }
    let samples = usize::from(plot.width) * 2;
    let (fill, dots): (Points, Points) = if windows {
        (
            series
                .windows
                .iter()
                .map(|w| (w.start_ms, w.cpu_avg / scale))
                .collect(),
            series
                .windows
                .iter()
                .map(|w| (w.start_ms, w.cpu_max / scale))
                .collect(),
        )
    } else {
        (
            series.history.iter().map(|p| (p.0, p.1 / scale)).collect(),
            Vec::new(),
        )
    };
    let values = graph::buckets(&fill, page.since(), page.range, samples, !windows);
    let overlay = graph::buckets(&dots, page.since(), page.range, samples, true);
    // 100% is one full core, red, whatever the scale.
    graph::braille(
        frame.buffer_mut(),
        plot,
        &values,
        &overlay,
        &|row| load_color(theme, row * scale / 100.0),
        theme.quiet(),
        theme.glyphs,
    );
}

/// A series' memory, the scale padded around the data so steady memory
/// reads as a flat line.
fn mem_graph(page: &Page, frame: &mut Frame, area: Rect, series: &Series) {
    let theme = page.theme;
    let values: Vec<u64> = series.history.iter().map(|p| p.2).collect();
    let (Some(low), Some(high)) = (values.iter().min(), values.iter().max()) else {
        return;
    };
    let pad = ((high - low) as f64 * 0.2).max(*high as f64 * 0.05);
    let bottom = (*low as f64 - pad).max(0.0);
    let top = *high as f64 + pad;
    let span = (top - bottom).max(1.0);
    let labels = [
        axis_bytes(top as u64),
        axis_bytes(((top + bottom) / 2.0) as u64),
        axis_bytes(bottom as u64),
    ];
    let plot = axes(page, frame, area, [&labels[0], &labels[1], &labels[2]]);
    if plot.is_empty() {
        return;
    }
    let samples = usize::from(plot.width) * 2;
    let points: Vec<(u64, f64)> = series
        .history
        .iter()
        .map(|p| (p.0, (p.2 as f64 - bottom) / span))
        .collect();
    let values = graph::buckets(&points, page.since(), page.range, samples, false);
    let total = page.memory_total();
    let color = |row: f64| {
        let load = memory_load((bottom + row * span) as u64, total).unwrap_or(0.0);
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
        vec![Span::styled(format!(" {} ", page.label), theme.quiet())],
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
            line.push_str(&format!("   cpu peak {}", percent(cpu)));
        }
        if let Some(memory) = memory {
            line.push_str(&format!("   mem peak {}", format_bytes(memory)));
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
    let range = vec![Span::styled(format!(" {} ", page.label), theme.quiet())];
    draw_box(
        page.app,
        frame,
        left,
        detail_title(page, &series, " · cpu"),
        range,
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
