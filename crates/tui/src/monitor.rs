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

/// Where a group sits, whatever the sort: by project, the project's
/// `(shared)` group first, then its lanes by number (`@0` on top), then
/// lanes without a number by name. Sorting groups by their totals made the
/// shared group and a lane swap places whenever their CPU crossed.
pub fn group_order(key: &str, lanes: &[LaneSummary]) -> (String, bool, bool, u32, String) {
    if let Some(project) = key.strip_suffix(" (shared)") {
        return (project.to_owned(), false, false, 0, key.to_owned());
    }
    let (project, index) = match lanes.iter().find(|l| l.reference == key) {
        Some(lane) => (lane.project.clone(), lane.index),
        // Metrics can name a lane that status does not list yet.
        None => match key.rsplit_once('@') {
            Some((project, index)) => (project.to_owned(), index.parse().ok()),
            None => (key.to_owned(), None),
        },
    };
    (
        project,
        true,
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
