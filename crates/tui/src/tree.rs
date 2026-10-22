//! The rows of one lane: the whole-lane row, then services, tasks and infra.
//! Services are one alphabetical list (the color bar shows families) with
//! the ones not running folded away while the lane runs. Pure functions of
//! the lane, its task list and the fold decisions, so the list, the mouse
//! and the tests agree.

use devlanes_proto::{LaneSummary, ORPHANED, UnitSummary};

use crate::api::TaskInfo;
use crate::app::is_active;

/// The three parts of a lane's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Services,
    Tasks,
    Infra,
}

impl Section {
    /// The header word.
    pub fn name(self) -> &'static str {
        match self {
            Section::Services => "services",
            Section::Tasks => "tasks",
            Section::Infra => "infra",
        }
    }
}

/// A row by identity, so live updates and folds never move the selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    WholeLane,
    Section(Section),
    /// One of the fixed groups below, inside a section.
    Group(Section, String),
    /// A unit the daemon lists: a service, a task that ran, an exec unit or a
    /// setup step.
    Unit(String),
    /// A task from `task.list` that has not run since the daemon started.
    Task(String),
    Container(String),
    /// The blank row before a section; never selected.
    Gap(u8),
}

/// Services that are not running while the lane runs.
pub const STOPPED: &str = "not started";
/// Exec units, after every service group.
pub const AD_HOC: &str = "ad-hoc";
pub const CONTAINERS: &str = "containers";
pub const SETUP: &str = "setup";
pub const TEARDOWN: &str = "teardown";

/// One visible row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: RowId,
    /// 0 for the whole lane and sections, 1 inside a section, 2 inside a group.
    pub depth: u8,
    /// Set on rows that fold: whether they are folded now.
    pub folded: Option<bool>,
    /// The units, tasks and containers under a header, for its counts.
    pub members: Vec<RowId>,
}

/// A row with what folds under it.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: RowId,
    pub children: Vec<Node>,
    /// Folded unless the user or the first layout decided otherwise.
    pub folded_by_default: bool,
}

impl Node {
    fn leaf(id: RowId) -> Self {
        Self {
            id,
            children: Vec::new(),
            folded_by_default: false,
        }
    }

    /// Every leaf under this node.
    pub fn leaves(&self) -> Vec<RowId> {
        if self.children.is_empty() {
            return vec![self.id.clone()];
        }
        self.children.iter().flat_map(Node::leaves).collect()
    }

    /// Rows this node takes when open, itself included.
    pub fn open_len(&self) -> usize {
        1 + self.children.iter().map(Node::open_len).sum::<usize>()
    }
}

/// What a unit is for, by name until the daemon sends a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Service,
    Task,
    Exec,
    Setup,
    Teardown,
}

/// The role of a unit: the daemon's `role`, else (older daemons) the kind,
/// with `setup.` and `teardown.` names as setup and teardown steps.
pub fn role(unit: &UnitSummary) -> Role {
    match unit.role.as_deref() {
        Some("service") => return Role::Service,
        Some("task") => return Role::Task,
        Some("setup") => return Role::Setup,
        Some("teardown") => return Role::Teardown,
        Some("exec") => return Role::Exec,
        _ => {}
    }
    if unit.name.starts_with("setup.") {
        Role::Setup
    } else if unit.name.starts_with("teardown.") {
        Role::Teardown
    } else {
        match unit.kind.as_str() {
            "task" => Role::Task,
            "exec" => Role::Exec,
            _ => Role::Service,
        }
    }
}

/// A unit that needs a look: failed, left running by an earlier daemon, or a
/// task or service that exited with an error.
pub fn is_problem(unit: &UnitSummary) -> bool {
    unit.state == "failed"
        || unit.state == ORPHANED
        || (unit.state == "exited" && unit.exit_code.is_some_and(|code| code != 0))
}

/// The lane runs or is on its way: services that are not running fold away.
pub fn lane_is_live(lane: &LaneSummary) -> bool {
    matches!(
        lane.state.as_str(),
        "running" | "starting" | "failed" | "stopping"
    ) || lane
        .units
        .iter()
        .any(|u| role(u) == Role::Service && is_active(&u.state))
}

/// Whether `text` matches the filter: a `+tag` matches tags, anything else a
/// part of the name or display name, ignoring case.
pub fn matches(filter: &str, name: &str, label: &str, tags: &[String]) -> bool {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return true;
    }
    if let Some(tag) = filter.strip_prefix('+') {
        return tags.iter().any(|t| t.to_lowercase().contains(tag));
    }
    name.to_lowercase().contains(&filter) || label.to_lowercase().contains(&filter)
}

/// `(name, id)` pairs as leaves in name order, ignoring case.
fn sorted(mut items: Vec<(String, RowId)>) -> Vec<Node> {
    items.sort_by_key(|item| item.0.to_lowercase());
    items.into_iter().map(|(_, id)| Node::leaf(id)).collect()
}

/// The lane's rows as a tree, before folding. With a filter only matching
/// leaves and their headers remain.
pub fn tree(lane: &LaneSummary, tasks: Option<&[TaskInfo]>, filter: Option<&str>) -> Vec<Node> {
    let filter = filter.filter(|f| !f.trim().is_empty());
    let keep = |unit: &UnitSummary| {
        filter.is_none_or(|f| matches(f, &unit.name, unit.label(), &unit.tags))
    };
    let live = lane_is_live(lane);
    let mut nodes = vec![Node::leaf(RowId::WholeLane)];

    // Services: what runs or needs a look, then what does not, then exec units.
    let mut running = Vec::new();
    let mut stopped = Vec::new();
    let mut ad_hoc = Vec::new();
    for unit in lane.units.iter().filter(|u| keep(u)) {
        let item = (unit.name.clone(), RowId::Unit(unit.name.clone()));
        match role(unit) {
            Role::Service if live && !is_active(&unit.state) && !is_problem(unit) => {
                stopped.push(item);
            }
            Role::Service => running.push(item),
            Role::Exec => ad_hoc.push(item),
            _ => {}
        }
    }
    let mut services = sorted(running);
    for (name, members, folded) in [(STOPPED, stopped, true), (AD_HOC, ad_hoc, false)] {
        if members.is_empty() {
            continue;
        }
        let mut members = members;
        members.sort_by_key(|member| member.0.to_lowercase());
        services.push(Node {
            id: RowId::Group(Section::Services, name.into()),
            children: members.into_iter().map(|(_, id)| Node::leaf(id)).collect(),
            folded_by_default: folded,
        });
    }
    if !services.is_empty() {
        nodes.push(section(Section::Services, services));
    }

    // Tasks: the ones that ran (a unit), then the rest of `task.list`.
    let ran: Vec<(String, RowId)> = lane
        .units
        .iter()
        .filter(|u| role(u) == Role::Task && keep(u))
        .map(|u| (u.name.clone(), RowId::Unit(u.name.clone())))
        .collect();
    let mut rest = Vec::new();
    for task in tasks.unwrap_or_default() {
        let setup = task.name.starts_with("setup.") || task.name.starts_with("teardown.");
        let listed = ran.iter().any(|(name, _)| *name == task.name);
        let shown = filter.is_none_or(|f| matches(f, &task.name, &task.name, &[]));
        if !setup && !listed && shown {
            rest.push((task.name.clone(), RowId::Task(task.name.clone())));
        }
    }
    if !ran.is_empty() || !rest.is_empty() {
        let mut children = sorted(ran);
        children.extend(sorted(rest));
        nodes.push(section(Section::Tasks, children));
    }

    // Infra: containers, then the setup and teardown steps that prepare them.
    let mut infra = Vec::new();
    let mut containers: Vec<&devlanes_proto::ContainerSummary> = lane
        .containers
        .iter()
        .filter(|c| !c.hidden)
        .filter(|c| filter.is_none_or(|f| matches(f, &c.name, &c.name, &[])))
        .collect();
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    if !containers.is_empty() {
        infra.push(Node {
            id: RowId::Group(Section::Infra, CONTAINERS.into()),
            children: containers
                .iter()
                .map(|c| Node::leaf(RowId::Container(c.name.clone())))
                .collect(),
            folded_by_default: false,
        });
    }
    for (name, wanted) in [(SETUP, Role::Setup), (TEARDOWN, Role::Teardown)] {
        let mut steps: Vec<&UnitSummary> = lane
            .units
            .iter()
            .filter(|u| role(u) == wanted && keep(u))
            .collect();
        if steps.is_empty() {
            continue;
        }
        steps.sort_by(|a, b| a.name.cmp(&b.name));
        // Folded unless it runs now or a step failed.
        let busy = steps.iter().any(|u| is_active(&u.state) || is_problem(u));
        infra.push(Node {
            id: RowId::Group(Section::Infra, name.into()),
            children: steps
                .iter()
                .map(|u| Node::leaf(RowId::Unit(u.name.clone())))
                .collect(),
            folded_by_default: !busy,
        });
    }
    if !infra.is_empty() {
        nodes.push(section(Section::Infra, infra));
    }
    nodes
}

fn section(section: Section, children: Vec<Node>) -> Node {
    Node {
        id: RowId::Section(section),
        children,
        folded_by_default: false,
    }
}

/// Flattens the tree into visible rows. `decision` returns a fold the user
/// or the first layout made for a header; without one the node's default
/// applies. With `open_all` (a filter is active) nothing is folded.
pub fn flatten(
    nodes: &[Node],
    decision: &dyn Fn(&Node) -> Option<bool>,
    open_all: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for node in nodes {
        push(node, 0, decision, open_all, &mut rows);
    }
    rows
}

fn push(
    node: &Node,
    depth: u8,
    decision: &dyn Fn(&Node) -> Option<bool>,
    open_all: bool,
    rows: &mut Vec<Row>,
) {
    let foldable = !node.children.is_empty();
    let folded = foldable && !open_all && decision(node).unwrap_or(node.folded_by_default);
    rows.push(Row {
        id: node.id.clone(),
        depth,
        folded: foldable.then_some(folded),
        members: if foldable { node.leaves() } else { Vec::new() },
    });
    if foldable && !folded {
        for child in &node.children {
            push(child, depth + 1, decision, open_all, rows);
        }
    }
}

/// The header that holds `id`, the innermost one first.
pub fn parents(nodes: &[Node], id: &RowId) -> Vec<RowId> {
    fn walk(node: &Node, id: &RowId, path: &mut Vec<RowId>) -> bool {
        if node.id == *id {
            return true;
        }
        path.push(node.id.clone());
        if node.children.iter().any(|child| walk(child, id, path)) {
            return true;
        }
        path.pop();
        false
    }
    for node in nodes {
        let mut path = Vec::new();
        if walk(node, id, &mut path) {
            path.reverse();
            return path;
        }
    }
    Vec::new()
}

/// Every foldable node, outermost first.
pub fn headers(nodes: &[Node]) -> Vec<&Node> {
    let mut out = Vec::new();
    fn walk<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
        if !node.children.is_empty() {
            out.push(node);
            for child in &node.children {
                walk(child, out);
            }
        }
    }
    for node in nodes {
        walk(node, &mut out);
    }
    out
}

/// Counts for a lane's summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Services and containers that are ready or running.
    pub up: usize,
    /// Every service and container.
    pub total: usize,
    pub failed: usize,
    pub orphaned: usize,
    pub starting: usize,
    pub queued: usize,
}

/// Counts over a lane's services and containers.
pub fn counts(lane: &LaneSummary) -> Counts {
    let mut counts = Counts::default();
    for unit in lane.units.iter().filter(|u| role(u) == Role::Service) {
        counts.total += 1;
        match unit.state.as_str() {
            "ready" | "running" => counts.up += 1,
            "failed" => counts.failed += 1,
            ORPHANED => counts.orphaned += 1,
            "starting" => counts.starting += 1,
            "queued" => counts.queued += 1,
            _ => {}
        }
    }
    for container in lane.containers.iter().filter(|c| !c.hidden) {
        counts.total += 1;
        match container_tone(container) {
            ContainerTone::Up => counts.up += 1,
            ContainerTone::Failed => counts.failed += 1,
            ContainerTone::Busy => counts.starting += 1,
            ContainerTone::Down => {}
        }
    }
    counts
}

/// How a container stands, from `ContainerSummary.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerTone {
    Up,
    Busy,
    Failed,
    Down,
}

/// A container without a state (older daemons) is listed because it runs.
pub fn container_tone(container: &devlanes_proto::ContainerSummary) -> ContainerTone {
    match container.state.as_deref() {
        None | Some("running" | "healthy") => ContainerTone::Up,
        Some("starting" | "created") => ContainerTone::Busy,
        Some(state) if state == "unhealthy" || state == "oom" || state.starts_with("exited") => {
            ContainerTone::Failed
        }
        Some(_) => ContainerTone::Down,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, state: &str, kind: &str) -> UnitSummary {
        UnitSummary {
            name: name.into(),
            state: state.into(),
            kind: kind.into(),
            ..UnitSummary::default()
        }
    }

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let indent = "  ".repeat(usize::from(row.depth));
                let name = match &row.id {
                    RowId::WholeLane => "whole lane".to_owned(),
                    RowId::Section(s) => s.name().to_owned(),
                    RowId::Group(_, g) => format!("[{g}]"),
                    RowId::Unit(u) | RowId::Task(u) | RowId::Container(u) => u.clone(),
                    RowId::Gap(_) => String::new(),
                };
                format!("{indent}{name}")
            })
            .collect()
    }

    fn lane(units: Vec<UnitSummary>, state: &str) -> LaneSummary {
        LaneSummary {
            reference: "acme@0".into(),
            state: state.into(),
            units,
            ..LaneSummary::default()
        }
    }

    #[test]
    fn services_are_one_alphabetical_list_under_their_section() {
        let lane = lane(
            vec![
                unit("api-users", "ready", "service"),
                unit("Web", "ready", "service"),
                unit("api-auth", "ready", "service"),
                unit("proxy", "ready", "service"),
                unit("migrate", "exited", "task"),
                unit("setup.db", "exited", "task"),
            ],
            "running",
        );
        let rows = flatten(&tree(&lane, None, None), &|_| None, false);
        assert_eq!(
            ids(&rows),
            [
                "whole lane",
                "services",
                "  api-auth",
                "  api-users",
                "  proxy",
                "  Web",
                "tasks",
                "  migrate",
                "infra",
                "  [setup]",
            ]
        );
    }

    #[test]
    fn stopped_services_fold_away_only_while_the_lane_runs() {
        let units = vec![
            unit("api-a", "ready", "service"),
            unit("api-b", "idle", "service"),
            unit("web", "failed", "service"),
        ];
        let running = lane(units.clone(), "running");
        let rows = flatten(&tree(&running, None, None), &|_| None, false);
        assert_eq!(
            ids(&rows),
            [
                "whole lane",
                "services",
                "  api-a",
                "  web",
                "  [not started]"
            ]
        );
        assert_eq!(rows[4].folded, Some(true));
        let mut stopped = lane(units, "idle");
        stopped.units[0].state = "idle".into();
        stopped.units[2].state = "idle".into();
        let rows = flatten(&tree(&stopped, None, None), &|_| None, false);
        assert_eq!(
            ids(&rows),
            ["whole lane", "services", "  api-a", "  api-b", "  web"]
        );
    }

    #[test]
    fn never_run_tasks_come_from_the_task_list() {
        let lane = lane(vec![unit("migrate", "exited", "task")], "idle");
        let tasks = [
            TaskInfo {
                name: "migrate".into(),
                ..TaskInfo::default()
            },
            TaskInfo {
                name: "lint".into(),
                ..TaskInfo::default()
            },
        ];
        // Tasks that ran come first, then the ones that never did.
        let rows = flatten(&tree(&lane, Some(&tasks), None), &|_| None, false);
        assert_eq!(ids(&rows), ["whole lane", "tasks", "  migrate", "  lint"]);
        assert_eq!(rows[2].id, RowId::Unit("migrate".into()));
        assert_eq!(rows[3].id, RowId::Task("lint".into()));
    }

    #[test]
    fn a_filter_keeps_matches_and_opens_every_group() {
        let mut tagged = unit("web-app", "ready", "service");
        tagged.tags = vec!["frontend".into()];
        let lane = lane(
            vec![
                unit("api-a", "ready", "service"),
                unit("api-b", "ready", "service"),
                tagged,
                unit("setup.db", "running", "task"),
            ],
            "running",
        );
        let nodes = tree(&lane, None, Some("api"));
        let rows = flatten(&nodes, &|_| Some(true), true);
        assert_eq!(ids(&rows), ["whole lane", "services", "  api-a", "  api-b"]);
        let rows = flatten(&tree(&lane, None, Some("+front")), &|_| None, true);
        assert_eq!(ids(&rows), ["whole lane", "services", "  web-app"]);
    }

    #[test]
    fn folds_hide_children_and_parents_are_found() {
        let lane = lane(
            vec![
                unit("api-a", "ready", "service"),
                unit("api-b", "idle", "service"),
                unit("web", "ready", "service"),
            ],
            "running",
        );
        let nodes = tree(&lane, None, None);
        let stopped = RowId::Group(Section::Services, STOPPED.into());
        let services = RowId::Section(Section::Services);
        let rows = flatten(&nodes, &|node| (node.id == services).then_some(true), false);
        assert_eq!(ids(&rows), ["whole lane", "services"]);
        assert_eq!(rows[1].members.len(), 3);
        assert_eq!(
            parents(&nodes, &RowId::Unit("api-b".into())),
            [stopped, services]
        );
    }
}
