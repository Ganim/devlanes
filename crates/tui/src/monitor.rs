//! The monitor's model: rows from `metrics` and lane status, filtered,
//! sorted and grouped by lane, with a selection that follows a row's
//! identity while the numbers reorder the table every second.

use std::{cmp::Ordering, collections::BTreeSet};

use devlanes_proto::{ContainerSummary, LaneSummary, UnitSummary};

use crate::api::{LaneMetrics, Metrics, UnitMetrics};

/// The daemon's raw ring: one sample a second for this long. Graphs show at
/// most this much history; past it the daemon sends 10 s windows instead.
pub const RAW_RANGE_MS: u64 = 900_000;

/// Columns the table sorts by, in the order `<` and `>` walk them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Lane,
    Kind,
    State,
    #[default]
    Cpu,
    Mem,
    Peak,
    Uptime,
}

impl SortKey {
    const ALL: [Self; 8] = [
        Self::Name,
        Self::Lane,
        Self::Kind,
        Self::State,
        Self::Cpu,
        Self::Mem,
        Self::Peak,
        Self::Uptime,
    ];

    /// The column header the key sorts.
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Lane => "lane",
            Self::Kind => "kind",
            Self::State => "state",
            Self::Cpu => "cpu",
            Self::Mem => "mem",
            Self::Peak => "peak",
            Self::Uptime => "uptime",
        }
    }

    /// The next (`by` 1) or previous (`by` -1) key, wrapping.
    pub fn step(self, by: isize) -> Self {
        let at = Self::ALL.iter().position(|k| *k == self).unwrap_or(0) as isize;
        let len = Self::ALL.len() as isize;
        Self::ALL[(at + by).rem_euclid(len) as usize]
    }

    /// Numbers sort highest first, words A to Z.
    fn descending(self) -> bool {
        matches!(self, Self::Cpu | Self::Mem | Self::Peak | Self::Uptime)
    }
}

/// What a monitor row is, independent of where the sort puts it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MonitorId {
    /// A lane, or `<project> (shared)`.
    Group(String),
    Row {
        lane: String,
        unit: String,
    },
}

/// The monitor's sort, filter, grouping and selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorView {
    pub filter: String,
    /// Keys go to the filter until Enter or Esc.
    pub editing: bool,
    pub sort: SortKey,
    /// `i` flips the key's natural direction.
    pub reverse: bool,
    /// One list instead of rows grouped by lane.
    pub flat: bool,
    /// Folded group keys; they last for the session.
    pub folded: BTreeSet<String>,
    /// The detail graphs fill the body.
    pub zoom: bool,
    pub selected: Option<MonitorId>,
    /// Where the selection is in the rows, for when its row goes away.
    pub index: usize,
    /// First table row on screen.
    pub offset: usize,
}

impl MonitorView {
    /// Whether the column is sorted highest first now.
    pub fn descending(&self) -> bool {
        self.sort.descending() != self.reverse
    }

    /// Puts the selection back on its row after the rows changed; when the
    /// row went away, on the row now at the same place.
    pub fn settle(&mut self, rows: &[MonitorRow]) {
        if rows.is_empty() {
            self.index = 0;
            self.selected = None;
            return;
        }
        if let Some(index) = self
            .selected
            .as_ref()
            .and_then(|id| rows.iter().position(|row| row.id() == *id))
        {
            self.index = index;
            return;
        }
        self.index = self.index.min(rows.len() - 1);
        self.selected = Some(rows[self.index].id());
    }

    /// Selects the row at `index`, clamped.
    pub fn select_index(&mut self, rows: &[MonitorRow], index: usize) {
        if let Some(last) = rows.len().checked_sub(1) {
            self.index = index.min(last);
            self.selected = Some(rows[self.index].id());
        }
    }
}

/// A unit or container row.
#[derive(Debug, Clone)]
pub struct Entry {
    pub lane: String,
    pub unit: String,
    /// The display name, else the unit name.
    pub label: String,
    /// `svc`, `task`, `exec` or `ctr`.
    pub kind: &'static str,
    /// The shown state word, `up` for containers without one.
    pub state: String,
    pub started_at_ms: Option<u64>,
    /// The listed unit, for its glyph and color.
    pub summary: Option<UnitSummary>,
    pub container: Option<ContainerSummary>,
    pub metrics: UnitMetrics,
}

impl Entry {
    pub fn is_container(&self) -> bool {
        self.kind == "ctr"
    }

    /// Highest memory over the range, from the history.
    pub fn peak(&self) -> Option<u64> {
        self.metrics
            .history
            .iter()
            .map(|p| p.2)
            .chain(self.metrics.memory_bytes)
            .max()
    }
}

/// A lane's header row, or the `<project> (shared)` one.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub key: String,
    pub branch: Option<String>,
    pub shared: bool,
    /// Rows in the group, before the filter.
    pub count: usize,
    pub folded: bool,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub peak_cpu_percent: Option<f64>,
    pub peak_memory_bytes: Option<u64>,
    pub history: Vec<(u64, f64, u64)>,
}

/// One monitor table row.
#[derive(Debug, Clone)]
pub enum MonitorRow {
    Group(Group),
    Entry(Box<Entry>),
}

impl MonitorRow {
    pub fn id(&self) -> MonitorId {
        match self {
            Self::Group(group) => MonitorId::Group(group.key.clone()),
            Self::Entry(entry) => MonitorId::Row {
                lane: entry.lane.clone(),
                unit: entry.unit.clone(),
            },
        }
    }
}

/// `svc`, `task`, `exec` or `ctr` from a metrics or status kind.
fn kind_word(kind: &str) -> &'static str {
    match kind {
        "task" => "task",
        "exec" => "exec",
        "container" => "ctr",
        _ => "svc",
    }
}

/// The metrics rows, or, before the daemon sent any, the running units from
/// lane status with their latest numbers.
fn sources(metrics: Option<&Metrics>, lanes: &[LaneSummary]) -> Vec<UnitMetrics> {
    match metrics.filter(|m| !m.units.is_empty()) {
        Some(metrics) => metrics.units.clone(),
        None => lanes
            .iter()
            .flat_map(|lane| lane.units.iter())
            .filter(|unit| unit.pid.is_some() || unit.cpu_percent.is_some())
            .map(|unit| UnitMetrics {
                lane: unit.lane.clone(),
                unit: unit.name.clone(),
                kind: unit.kind.clone(),
                cpu_percent: unit.cpu_percent,
                memory_bytes: unit.memory_bytes,
                ..UnitMetrics::default()
            })
            .collect(),
    }
}

/// A metrics row with what lane status knows about it.
fn entry(row: UnitMetrics, lanes: &[LaneSummary]) -> Entry {
    let lane = lanes.iter().find(|l| l.reference == row.lane);
    if row.is_container() {
        // Shared containers list in every lane that uses them; any will do.
        let name = row.unit.clone();
        let container = lanes
            .iter()
            .filter(|l| l.reference == row.lane || row.lanes.contains(&l.reference))
            .flat_map(|l| l.containers.iter())
            .find(|c| c.name == name || row.container.as_deref() == Some(c.name.as_str()))
            .cloned();
        let state = container
            .as_ref()
            .and_then(|c| c.state.clone())
            .map_or_else(
                || "up".to_owned(),
                |state| {
                    if state == "running" {
                        "up".into()
                    } else {
                        state
                    }
                },
            );
        return Entry {
            lane: row.lane.clone(),
            unit: name.clone(),
            label: name,
            kind: "ctr",
            state,
            started_at_ms: None,
            summary: None,
            container,
            metrics: row,
        };
    }
    let summary = lane
        .and_then(|l| l.units.iter().find(|u| u.name == row.unit))
        .cloned();
    let kind = if row.kind.is_empty() {
        summary.as_ref().map_or("svc", |u| kind_word(&u.kind))
    } else {
        kind_word(&row.kind)
    };
    Entry {
        lane: row.lane.clone(),
        unit: row.unit.clone(),
        label: summary
            .as_ref()
            .map_or_else(|| row.unit.clone(), |u| u.label().to_owned()),
        kind,
        state: summary
            .as_ref()
            .map_or_else(|| "running".into(), |u| u.shown_state().to_owned()),
        started_at_ms: summary.as_ref().and_then(|u| u.started_at_ms),
        summary,
        container: None,
        metrics: row,
    }
}

/// Whether every word of `filter` is in one of the row's names or words.
fn matches(entry: &Entry, filter: &str) -> bool {
    let haystack = format!(
        "{} {} {} {} {} {}",
        entry.label,
        entry.unit,
        entry.lane,
        entry.kind,
        entry.state,
        entry.metrics.container.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    filter
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// Compares two optional numbers, missing ones last whatever the direction.
fn by_number(a: Option<f64>, b: Option<f64>, descending: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => {
            let order = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
            if descending { order.reverse() } else { order }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Compares two words in the view's direction.
fn by_word(a: &str, b: &str, descending: bool) -> Ordering {
    let order = a.to_lowercase().cmp(&b.to_lowercase());
    if descending { order.reverse() } else { order }
}

/// Orders two entries by the view's column, then by name and lane so equal
/// values keep a stable place.
fn compare(a: &Entry, b: &Entry, view: &MonitorView) -> Ordering {
    let descending = view.descending();
    let primary = match view.sort {
        SortKey::Name => by_word(&a.label, &b.label, descending),
        SortKey::Lane => by_word(&a.lane, &b.lane, descending),
        SortKey::Kind => by_word(a.kind, b.kind, descending),
        SortKey::State => by_word(&a.state, &b.state, descending),
        SortKey::Cpu => by_number(a.metrics.cpu_percent, b.metrics.cpu_percent, descending),
        SortKey::Mem => by_number(
            a.metrics.memory_bytes.map(|m| m as f64),
            b.metrics.memory_bytes.map(|m| m as f64),
            descending,
        ),
        SortKey::Peak => by_number(
            a.peak().map(|m| m as f64),
            b.peak().map(|m| m as f64),
            descending,
        ),
        // Longest running first: the earliest start.
        SortKey::Uptime => by_number(
            a.started_at_ms.map(|s| -(s as f64)),
            b.started_at_ms.map(|s| -(s as f64)),
            descending,
        ),
    };
    primary
        .then_with(|| by_word(&a.label, &b.label, false))
        .then_with(|| a.lane.cmp(&b.lane))
}

/// Sums a group's members point by point, by timestamp.
fn summed(members: &[&Entry]) -> Vec<(u64, f64, u64)> {
    let mut totals: std::collections::BTreeMap<u64, (f64, u64)> = std::collections::BTreeMap::new();
    for member in members {
        for (ts, cpu, memory) in &member.metrics.history {
            let total = totals.entry(*ts).or_default();
            total.0 += cpu;
            total.1 += memory;
        }
    }
    totals
        .into_iter()
        .map(|(ts, (cpu, memory))| (ts, cpu, memory))
        .collect()
}

/// A group's header from its lane's series when the daemon sends one,
/// else the sum of its members.
fn group(
    key: &str,
    members: &[&Entry],
    total: usize,
    lane: Option<&LaneMetrics>,
    lanes: &[LaneSummary],
    folded: bool,
) -> Group {
    let sum_cpu = members
        .iter()
        .filter_map(|m| m.metrics.cpu_percent)
        .reduce(|a, b| a + b);
    let sum_mem = members
        .iter()
        .filter_map(|m| m.metrics.memory_bytes)
        .reduce(|a, b| a + b);
    let summary = lanes.iter().find(|l| l.reference == key);
    Group {
        key: key.to_owned(),
        branch: summary.and_then(|l| l.branch.clone()),
        shared: key.ends_with(" (shared)"),
        count: total,
        folded,
        cpu_percent: lane.and_then(|l| l.cpu_percent).or(sum_cpu),
        memory_bytes: lane.and_then(|l| l.memory_bytes).or(sum_mem),
        peak_cpu_percent: lane.and_then(|l| l.peak_cpu_percent),
        peak_memory_bytes: lane.and_then(|l| l.peak_memory_bytes),
        history: lane.map_or_else(|| summed(members), |l| l.history.clone()),
    }
}

/// The table: filtered, sorted, grouped by lane unless flat. Groups sort by
/// their totals under the same column; folded groups keep only their header.
pub fn rows(
    metrics: Option<&Metrics>,
    lanes: &[LaneSummary],
    view: &MonitorView,
) -> Vec<MonitorRow> {
    let all: Vec<Entry> = sources(metrics, lanes)
        .into_iter()
        .map(|row| entry(row, lanes))
        .collect();
    let mut shown: Vec<&Entry> = all.iter().filter(|e| matches(e, &view.filter)).collect();
    shown.sort_by(|a, b| compare(a, b, view));
    if view.flat {
        return shown
            .into_iter()
            .map(|e| MonitorRow::Entry(Box::new(e.clone())))
            .collect();
    }
    let mut keys: Vec<&str> = shown.iter().map(|e| e.lane.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    let lane_metrics = |key: &str| metrics.and_then(|m| m.lanes.iter().find(|l| l.lane == key));
    let mut groups: Vec<(Group, Vec<&Entry>)> = keys
        .into_iter()
        .map(|key| {
            let members: Vec<&Entry> = shown.iter().copied().filter(|e| e.lane == key).collect();
            let total = all.iter().filter(|e| e.lane == key).count();
            let header = group(
                key,
                &members,
                total,
                lane_metrics(key),
                lanes,
                view.folded.contains(key),
            );
            (header, members)
        })
        .collect();
    let descending = view.descending();
    groups.sort_by(|(a, _), (b, _)| {
        let order = match view.sort {
            SortKey::Cpu => by_number(a.cpu_percent, b.cpu_percent, descending),
            SortKey::Mem | SortKey::Peak => by_number(
                a.memory_bytes.map(|m| m as f64),
                b.memory_bytes.map(|m| m as f64),
                descending,
            ),
            SortKey::Lane | SortKey::Name => by_word(&a.key, &b.key, descending),
            _ => Ordering::Equal,
        };
        order.then_with(|| a.key.cmp(&b.key))
    });
    let mut rows = Vec::new();
    for (header, members) in groups {
        let folded = header.folded;
        rows.push(MonitorRow::Group(header));
        if !folded {
            rows.extend(
                members
                    .into_iter()
                    .map(|e| MonitorRow::Entry(Box::new(e.clone()))),
            );
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(lane: &str, name: &str, cpu: f64, memory: u64) -> UnitMetrics {
        UnitMetrics {
            lane: lane.into(),
            unit: name.into(),
            kind: "service".into(),
            cpu_percent: Some(cpu),
            memory_bytes: Some(memory),
            ..UnitMetrics::default()
        }
    }

    fn metrics() -> Metrics {
        let mut postgres = unit("acme (shared)", "postgres", 6.0, 600);
        postgres.kind = "container".into();
        postgres.shared = true;
        Metrics {
            units: vec![
                unit("acme@0", "api", 96.0, 400),
                unit("acme@0", "web", 40.0, 900),
                unit("acme@1", "web", 70.0, 100),
                postgres,
            ],
            ..Metrics::default()
        }
    }

    fn names(rows: &[MonitorRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                MonitorRow::Group(g) => format!("[{}]", g.key),
                MonitorRow::Entry(e) => e.unit.clone(),
            })
            .collect()
    }

    #[test]
    fn groups_sort_by_their_totals_and_rows_inside_them() {
        let view = MonitorView::default();
        let rows = rows(Some(&metrics()), &[], &view);
        assert_eq!(
            names(&rows),
            [
                "[acme@0]",
                "api",
                "web",
                "[acme@1]",
                "web",
                "[acme (shared)]",
                "postgres"
            ]
        );
        let MonitorRow::Group(group) = &rows[0] else {
            panic!("no group");
        };
        assert_eq!(
            group.cpu_percent,
            Some(136.0),
            "members summed without lane series"
        );
    }

    #[test]
    fn the_sort_steps_and_reverses() {
        let mut view = MonitorView {
            flat: true,
            sort: SortKey::Mem,
            ..MonitorView::default()
        };
        assert_eq!(
            names(&rows(Some(&metrics()), &[], &view)),
            ["web", "postgres", "api", "web"]
        );
        view.reverse = true;
        assert_eq!(names(&rows(Some(&metrics()), &[], &view))[0], "web");
        assert_eq!(names(&rows(Some(&metrics()), &[], &view))[1], "api");
        assert_eq!(SortKey::Cpu.step(1), SortKey::Mem);
        assert_eq!(SortKey::Name.step(-1), SortKey::Uptime);
    }

    #[test]
    fn the_filter_keeps_groups_with_matches_and_needs_every_word() {
        let view = MonitorView {
            filter: "WEB acme@1".into(),
            ..MonitorView::default()
        };
        assert_eq!(
            names(&rows(Some(&metrics()), &[], &view)),
            ["[acme@1]", "web"]
        );
        let view = MonitorView {
            filter: "ctr".into(),
            ..MonitorView::default()
        };
        assert_eq!(
            names(&rows(Some(&metrics()), &[], &view)),
            ["[acme (shared)]", "postgres"]
        );
    }

    #[test]
    fn folded_groups_keep_their_header() {
        let mut view = MonitorView::default();
        view.folded.insert("acme@0".into());
        assert_eq!(
            names(&rows(Some(&metrics()), &[], &view)),
            ["[acme@0]", "[acme@1]", "web", "[acme (shared)]", "postgres"]
        );
    }

    #[test]
    fn the_selection_follows_its_row_and_falls_back_to_its_place() {
        let mut view = MonitorView {
            flat: true,
            ..MonitorView::default()
        };
        let mut data = metrics();
        let first = rows(Some(&data), &[], &view);
        view.select_index(&first, 1);
        assert_eq!(
            view.selected,
            Some(MonitorId::Row {
                lane: "acme@1".into(),
                unit: "web".into()
            })
        );
        // api calms down; web on acme@1 moves to the top and the cursor follows.
        data.units[0].cpu_percent = Some(1.0);
        let next = rows(Some(&data), &[], &view);
        view.settle(&next);
        assert_eq!(view.index, 0);
        // It stops: the row at its place takes the selection.
        data.units.remove(2);
        let last = rows(Some(&data), &[], &view);
        view.settle(&last);
        assert_eq!(view.index, 0);
        assert_eq!(view.selected, Some(last[0].id()));
    }

    #[test]
    fn without_metrics_running_units_come_from_status() {
        let mut api = UnitSummary {
            name: "api".into(),
            lane: "acme@0".into(),
            state: "ready".into(),
            pid: Some(7),
            cpu_percent: Some(3.0),
            ..UnitSummary::default()
        };
        api.display_name = Some("Gateway".into());
        let idle = UnitSummary {
            name: "docs".into(),
            lane: "acme@0".into(),
            state: "idle".into(),
            ..UnitSummary::default()
        };
        let lane = LaneSummary {
            reference: "acme@0".into(),
            units: vec![api, idle],
            ..LaneSummary::default()
        };
        let view = MonitorView {
            flat: true,
            ..MonitorView::default()
        };
        let rows = rows(None, &[lane], &view);
        assert_eq!(names(&rows), ["api"]);
        let MonitorRow::Entry(entry) = &rows[0] else {
            panic!("no entry");
        };
        assert_eq!(
            (entry.label.as_str(), entry.state.as_str()),
            ("Gateway", "ready")
        );
    }
}
