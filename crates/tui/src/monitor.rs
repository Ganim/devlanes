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
        /// The daemon's series key. Two lanes cleaned under one reference
        /// share a `(removed)` group, and without it their rows would have one
        /// id, which pins the selection to the first of them.
        series: Option<String>,
    },
    /// The devlanes daemon's own row.
    Daemon,
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
    /// `m`: memory takes the big box with the graph and cpu the small one,
    /// with a square per core; `c` puts cpu back.
    pub memory_first: bool,
    pub selected: Option<MonitorId>,
    /// Where the selection is in the rows, for when its row goes away.
    pub index: usize,
    /// First table row on screen. Keys keep the selection in view; the
    /// wheel scrolls without moving it.
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

    /// Scrolls the least that shows the selection in a table `height` rows
    /// high whose first `scrolling` rows scroll; rows after them are pinned
    /// below and always in view.
    pub fn reveal(&mut self, height: usize, scrolling: usize) {
        if height == 0 || self.index >= scrolling {
            return;
        }
        if self.index < self.offset {
            self.offset = self.index;
        } else if self.index >= self.offset + height {
            self.offset = self.index + 1 - height;
        }
    }

    /// Scrolls the table by `by` rows without moving the selection, within
    /// `rows` rows shown `height` at a time.
    pub fn scroll(&mut self, by: isize, rows: usize, height: usize) {
        let last = rows.saturating_sub(height);
        self.offset = self.offset.min(last).saturating_add_signed(by).min(last);
    }
}

/// What a removed lane's group key adds to its reference.
pub(crate) const REMOVED: &str = " (removed)";

/// The table group of a row or lane series in `lane`: the lane itself, or
/// `<lane> (removed)` once the daemon no longer has the lane, so a newer
/// lane that took the same reference gets a group of its own.
pub fn group_key(lane: &str, removed: bool) -> String {
    if removed {
        format!("{lane}{REMOVED}")
    } else {
        lane.to_owned()
    }
}

/// A unit or container row.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The row's group: its lane, `<project> (shared)` or `<lane> (removed)`
    /// (see [`group_key`]).
    pub lane: String,
    pub unit: String,
    /// The display name, else the unit name.
    pub label: String,
    /// `svc`, `task`, `exec` or `ctr`.
    pub kind: &'static str,
    /// The shown state word; containers without one read `up`, or `stopped`
    /// once ended.
    pub state: String,
    pub started_at_ms: Option<u64>,
    /// The listed unit, for its glyph and color.
    pub summary: Option<UnitSummary>,
    pub container: Option<ContainerSummary>,
    pub metrics: UnitMetrics,
}

impl Entry {
    /// True for an infra container row.
    pub fn is_container(&self) -> bool {
        self.kind == "ctr"
    }

    /// True when the row is not running now and stays only for its history.
    pub fn ended(&self) -> bool {
        self.metrics.ended
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
    /// The lane no longer exists; the key ends in ` (removed)`.
    pub removed: bool,
    /// The daemon's key of the lane series the header shows, when it sent
    /// one.
    pub series: Option<String>,
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
    /// The devlanes daemon, pinned under the table and left out of every
    /// total, filter and sort.
    Daemon(Box<Entry>),
}

impl MonitorRow {
    /// The row's identity, which the selection follows.
    pub fn id(&self) -> MonitorId {
        match self {
            Self::Group(group) => MonitorId::Group(group.key.clone()),
            Self::Entry(entry) => MonitorId::Row {
                lane: entry.lane.clone(),
                unit: entry.unit.clone(),
                series: entry.metrics.series.clone(),
            },
            Self::Daemon(_) => MonitorId::Daemon,
        }
    }

    /// True for rows pinned under the scrolling ones.
    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Daemon(_))
    }
}

/// How many of `rows` scroll: all but the pinned ones at the end.
pub fn scrolling(rows: &[MonitorRow]) -> usize {
    rows.iter().filter(|row| !row.is_pinned()).count()
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

/// A metrics row with what lane status knows about it. Ended rows without a
/// summary to ask read `stopped`; no ended row has an uptime.
fn entry(row: UnitMetrics, lanes: &[LaneSummary]) -> Entry {
    // A removed row's reference may already name a newer lane, whose units
    // and containers say nothing about this row.
    let lanes = if row.removed { &[][..] } else { lanes };
    let key = group_key(&row.lane, row.removed);
    let ended = row.ended;
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
                || if ended { "stopped" } else { "up" }.to_owned(),
                |state| {
                    if state == "running" {
                        "up".into()
                    } else {
                        state
                    }
                },
            );
        return Entry {
            lane: key,
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
        lane: key,
        unit: row.unit.clone(),
        label: summary
            .as_ref()
            .map_or_else(|| row.unit.clone(), |u| u.label().to_owned()),
        kind,
        state: summary.as_ref().map_or_else(
            || if ended { "stopped" } else { "running" }.into(),
            |u| u.shown_state().to_owned(),
        ),
        // A stopped unit's summary can keep the start of its last run.
        started_at_ms: summary
            .as_ref()
            .and_then(|u| u.started_at_ms)
            .filter(|_| !ended),
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
    let removed = key.ends_with(REMOVED);
    // A removed lane's reference may belong to a newer lane, whose branch is
    // not this one's.
    let summary = lanes
        .iter()
        .filter(|_| !removed)
        .find(|l| l.reference == key);
    Group {
        key: key.to_owned(),
        branch: summary.and_then(|l| l.branch.clone()),
        shared: key.ends_with(" (shared)"),
        removed,
        series: lane.and_then(|l| l.series.clone()),
        count: total,
        folded,
        cpu_percent: lane.and_then(|l| l.cpu_percent).or(sum_cpu),
        memory_bytes: lane.and_then(|l| l.memory_bytes).or(sum_mem),
        peak_cpu_percent: lane.and_then(|l| l.peak_cpu_percent),
        peak_memory_bytes: lane.and_then(|l| l.peak_memory_bytes),
        history: lane.map_or_else(|| summed(members), |l| l.history.clone()),
    }
}

/// Where a group sits, whatever the sort: by project, the project's
/// `(shared)` group first, then its lanes by number (`@0` on top), then
/// lanes without a number by name, then removed lanes in the same order.
/// Sorting groups by their totals made the shared group and a lane swap
/// places whenever their CPU crossed.
pub fn group_order(key: &str, lanes: &[LaneSummary]) -> (String, u8, bool, u32, String) {
    if let Some(project) = key.strip_suffix(" (shared)") {
        return (project.to_owned(), 0, false, 0, key.to_owned());
    }
    let (reference, rank) = match key.strip_suffix(REMOVED) {
        Some(reference) => (reference, 2),
        None => (key, 1),
    };
    // A removed lane's reference may belong to a newer lane now, so its
    // project and number come from the reference itself.
    let summary = lanes
        .iter()
        .filter(|_| rank == 1)
        .find(|l| l.reference == reference);
    let (project, index) = match summary {
        Some(lane) => (lane.project.clone(), lane.index),
        // Metrics can name a lane that status does not list yet.
        None => match reference.rsplit_once('@') {
            Some((project, index)) => (project.to_owned(), index.parse().ok()),
            None => (reference.to_owned(), None),
        },
    };
    (
        project,
        rank,
        index.is_none(),
        index.unwrap_or(0),
        key.to_owned(),
    )
}

/// The daemon's row, when the daemon sampled itself.
fn daemon_row(metrics: Option<&Metrics>) -> Option<MonitorRow> {
    let daemon = metrics?.daemon.as_ref()?;
    Some(MonitorRow::Daemon(Box::new(Entry {
        lane: daemon.metrics.lane.clone(),
        unit: daemon.metrics.unit.clone(),
        label: "devlanes daemon".into(),
        kind: "dmn",
        state: "running".into(),
        started_at_ms: daemon.started_at_ms,
        summary: None,
        container: None,
        metrics: daemon.metrics.clone(),
    })))
}

/// The table: filtered, sorted, grouped by lane unless flat, with the
/// daemon's row last. Groups keep the place [`group_order`] gives them;
/// folded groups keep only their header.
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
            .chain(daemon_row(metrics))
            .collect();
    }
    let mut keys: Vec<&str> = shown.iter().map(|e| e.lane.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    // Lane series match by group key too, so a removed lane's header shows
    // its own series and not the newer lane's under the same reference.
    let lane_metrics = |key: &str| {
        let mut found = metrics
            .into_iter()
            .flat_map(|m| m.lanes.iter())
            .filter(|l| group_key(&l.lane, l.removed) == key);
        // Two lanes cleaned under one reference share a group, and neither
        // series is its total; the header sums its rows instead.
        found.next().filter(|_| found.next().is_none())
    };
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
    groups.sort_by_cached_key(|(group, _)| group_order(&group.key, lanes));
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
    rows.extend(daemon_row(metrics));
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
                MonitorRow::Daemon(_) => "(daemon)".into(),
            })
            .collect()
    }

    #[test]
    fn groups_keep_their_place_and_rows_sort_inside_them() {
        let view = MonitorView::default();
        let expected = [
            "[acme (shared)]",
            "postgres",
            "[acme@0]",
            "api",
            "web",
            "[acme@1]",
            "web",
        ];
        let shown = rows(Some(&metrics()), &[], &view);
        assert_eq!(names(&shown), expected);
        let MonitorRow::Group(group) = &shown[2] else {
            panic!("no group");
        };
        assert_eq!(
            group.cpu_percent,
            Some(136.0),
            "members summed without lane series"
        );
        // Shared infra busier than every lane, and a lane busier than @0:
        // the groups stay where they were.
        let mut busy = metrics();
        busy.units[3].cpu_percent = Some(900.0);
        busy.units[2].cpu_percent = Some(800.0);
        assert_eq!(names(&rows(Some(&busy), &[], &view)), expected);
        // Lanes go by number, not by name: @2 before @10.
        let order = |key: &str| group_order(key, &[]);
        assert!(order("acme (shared)") < order("acme@2"));
        assert!(order("acme@2") < order("acme@10"));
        assert!(order("acme@10") < order("acme@docs"));
        assert!(order("acme@docs") < order("beta (shared)"));
        // Removed lanes follow every live lane of their project, by number.
        assert!(order("acme@docs") < order("acme@1 (removed)"));
        assert!(order("acme@1 (removed)") < order("acme@2 (removed)"));
        assert!(order("acme@2 (removed)") < order("acme@docs (removed)"));
        assert!(order("acme@docs (removed)") < order("beta (shared)"));
    }

    /// `row` ended, without readings, its series keyed `series`.
    fn ended(mut row: UnitMetrics, series: &str, removed: bool) -> UnitMetrics {
        row.cpu_percent = None;
        row.memory_bytes = None;
        row.history = vec![(1_000, 30.0, 300)];
        row.series = Some(series.into());
        row.ended = true;
        row.removed = removed;
        row
    }

    /// A lane series without a reading now.
    fn ended_lane(lane: &str, series: &str, removed: bool) -> LaneMetrics {
        LaneMetrics {
            lane: lane.into(),
            history: vec![(1_000, 30.0, 300)],
            series: Some(series.into()),
            ended: true,
            removed,
            ..LaneMetrics::default()
        }
    }

    #[test]
    fn removed_rows_group_after_the_live_lanes_without_their_status() {
        let mut data = metrics();
        // acme@1 was cleaned and made again: the old lane's web stays for
        // its history next to the new lane's web.
        data.units
            .push(ended(unit("acme@1", "web", 0.0, 0), "unit:1:2:web", true));
        data.lanes = vec![
            LaneMetrics {
                lane: "acme@1".into(),
                cpu_percent: Some(70.0),
                series: Some("lane:1:3".into()),
                ..LaneMetrics::default()
            },
            ended_lane("acme@1", "lane:1:2", true),
        ];
        let status = LaneSummary {
            reference: "acme@1".into(),
            project: "acme".into(),
            index: Some(1),
            branch: Some("feat/new".into()),
            units: vec![UnitSummary {
                name: "web".into(),
                lane: "acme@1".into(),
                state: "ready".into(),
                started_at_ms: Some(5),
                ..UnitSummary::default()
            }],
            ..LaneSummary::default()
        };
        let shown = rows(Some(&data), &[status], &MonitorView::default());
        assert_eq!(
            names(&shown),
            [
                "[acme (shared)]",
                "postgres",
                "[acme@0]",
                "api",
                "web",
                "[acme@1]",
                "web",
                "[acme@1 (removed)]",
                "web",
            ]
        );
        let group = |at: usize| match &shown[at] {
            MonitorRow::Group(group) => group.clone(),
            _ => panic!("no group at {at}"),
        };
        let live = group(5);
        assert_eq!(live.branch.as_deref(), Some("feat/new"));
        assert_eq!(live.series.as_deref(), Some("lane:1:3"));
        assert_eq!(live.cpu_percent, Some(70.0));
        // The removed lane's header has its own series, and the new lane's
        // branch is not its branch.
        let removed = group(7);
        assert!(removed.removed && !removed.shared);
        assert_eq!(removed.branch, None);
        assert_eq!(removed.series.as_deref(), Some("lane:1:2"));
        assert_eq!(removed.cpu_percent, None);
        assert_eq!(removed.history, [(1_000, 30.0, 300)]);
        let MonitorRow::Entry(entry) = &shown[8] else {
            panic!("no entry");
        };
        // The new lane's web is ready since 5; the removed one is neither.
        assert_eq!(entry.state, "stopped");
        assert!(entry.summary.is_none());
        assert_eq!(entry.started_at_ms, None);
        assert_ne!(shown[8].id(), shown[6].id());
    }

    #[test]
    fn ended_rows_take_their_state_from_status_or_read_stopped() {
        let status = LaneSummary {
            reference: "acme@0".into(),
            units: vec![UnitSummary {
                name: "api".into(),
                lane: "acme@0".into(),
                state: "failed".into(),
                started_at_ms: Some(5),
                ..UnitSummary::default()
            }],
            containers: vec![ContainerSummary {
                name: "minio".into(),
                state: Some("exited 1".into()),
                ..ContainerSummary::default()
            }],
            ..LaneSummary::default()
        };
        let container = |name: &str| UnitMetrics {
            kind: "container".into(),
            ..unit("acme@0", name, 0.0, 0)
        };
        let data = Metrics {
            units: vec![
                ended(unit("acme@0", "api", 0.0, 0), "unit:1:1:api", false),
                ended(unit("acme@0", "docs", 0.0, 0), "unit:1:1:docs", false),
                ended(container("minio"), "container:1:1:acme-minio-1", false),
                ended(container("redis"), "container:1:1:acme-redis-1", false),
            ],
            ..Metrics::default()
        };
        let view = MonitorView {
            flat: true,
            sort: SortKey::Name,
            ..MonitorView::default()
        };
        let states: Vec<(String, String, Option<u64>)> = rows(Some(&data), &[status], &view)
            .iter()
            .filter_map(|row| match row {
                MonitorRow::Entry(e) => Some((e.unit.clone(), e.state.clone(), e.started_at_ms)),
                _ => None,
            })
            .collect();
        let expected = [
            ("api", "failed", None),
            ("docs", "stopped", None),
            ("minio", "exited 1", None),
            ("redis", "stopped", None),
        ]
        .map(|(unit, state, started)| (unit.to_owned(), state.to_owned(), started));
        assert_eq!(states, expected);
    }

    #[test]
    fn a_lane_whose_units_all_ended_keeps_its_header() {
        let mut data = metrics();
        data.units[2] = ended(data.units[2].clone(), "unit:1:2:web", false);
        data.lanes = vec![ended_lane("acme@1", "lane:1:2", false)];
        let shown = rows(Some(&data), &[], &MonitorView::default());
        let Some(MonitorRow::Group(group)) = shown.get(5) else {
            panic!("no acme@1 header: {:?}", names(&shown));
        };
        assert_eq!(group.key, "acme@1");
        assert_eq!((group.cpu_percent, group.memory_bytes), (None, None));
        assert_eq!(group.series.as_deref(), Some("lane:1:2"));
    }

    #[test]
    fn the_selection_settles_when_an_ended_row_drops_out() {
        let mut data = metrics();
        data.units
            .push(ended(unit("acme@1", "web", 0.0, 0), "unit:1:2:web", true));
        let mut view = MonitorView::default();
        let before = rows(Some(&data), &[], &view);
        let last = before.len() - 1;
        view.select_index(&before, last);
        assert_eq!(
            view.selected,
            Some(MonitorId::Row {
                lane: "acme@1 (removed)".into(),
                unit: "web".into(),
                series: Some("unit:1:2:web".into()),
            })
        );
        // Its history left the range: the row and its group go, and the
        // selection takes the row now last.
        data.units.pop();
        let after = rows(Some(&data), &[], &view);
        view.settle(&after);
        assert_eq!(view.index, after.len() - 1);
        assert_eq!(view.selected, Some(after[after.len() - 1].id()));
    }

    #[test]
    fn the_daemon_row_comes_last_whatever_the_filter() {
        let mut data = metrics();
        data.daemon = Some(Box::new(crate::api::DaemonMetrics {
            pid: Some(7),
            started_at_ms: None,
            metrics: UnitMetrics {
                lane: "devlanes".into(),
                unit: "daemon".into(),
                kind: "daemon".into(),
                ..UnitMetrics::default()
            },
        }));
        for view in [
            MonitorView::default(),
            MonitorView {
                flat: true,
                filter: "nothing matches".into(),
                ..MonitorView::default()
            },
        ] {
            let rows = rows(Some(&data), &[], &view);
            let last = rows.last().map(MonitorRow::id);
            assert_eq!(last, Some(MonitorId::Daemon));
            assert_eq!(scrolling(&rows), rows.len() - 1);
        }
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
            ["[acme (shared)]", "postgres", "[acme@0]", "[acme@1]", "web"]
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
                unit: "web".into(),
                series: None,
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
