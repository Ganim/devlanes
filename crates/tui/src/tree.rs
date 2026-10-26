//! The rows of one lane: services, tasks and infra. Services keep the order
//! of devlanes.yaml, running or not (the name's color shows the family).
//! The lane itself is selected through the list's title, not a row. Pure
//! functions of the lane, its task list and the fold decisions, so the list,
//! the mouse and the tests agree.

use devlanes_proto::{ContainerSummary, LaneSummary, ORPHANED, UnitSummary};

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
    /// The lane itself: its title in the list box, never a row.
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

/// Exec units, after the services.
pub const AD_HOC: &str = "ad-hoc";
/// The lane's own containers.
pub const CONTAINERS: &str = "containers";
/// The project's shared containers, in every lane.
pub const SHARED: &str = "shared";
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

/// What `run` takes to run `unit` again: a task's name, or `infra.setup.<entry>`
/// and `infra.teardown.<entry>` for a setup or teardown step; `None` for the rest.
pub fn run_name(unit: &UnitSummary) -> Option<String> {
    match role(unit) {
        Role::Task => Some(unit.name.clone()),
        Role::Setup | Role::Teardown => Some(format!("infra.{}", unit.name)),
        Role::Service | Role::Exec => None,
    }
}

/// The unit a `run` of `task` spawns: `setup.db` for `infra.setup.db`, the
/// task's own name otherwise.
pub fn run_unit(task: &str) -> &str {
    if task.starts_with("infra.setup.") || task.starts_with("infra.teardown.") {
        &task["infra.".len()..]
    } else {
        task
    }
}

/// The name a row shows: a setup or teardown step drops its `setup.` or
/// `teardown.` prefix, since the group above it says which it is.
pub fn short_name(unit: &UnitSummary) -> &str {
    let prefix = match role(unit) {
        Role::Setup => "setup.",
        Role::Teardown => "teardown.",
        _ => return unit.label(),
    };
    if unit.display_name.is_some() {
        return unit.label();
    }
    unit.name.strip_prefix(prefix).unwrap_or(&unit.name)
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

/// The lane's rows as a tree, before folding. `shared` are the project's
/// shared containers, listed in every lane. With a filter only matching
/// leaves and their headers remain.
pub fn tree(
    lane: &LaneSummary,
    shared: &[ContainerSummary],
    tasks: Option<&[TaskInfo]>,
    filter: Option<&str>,
) -> Vec<Node> {
    let filter = filter.filter(|f| !f.trim().is_empty());
    let keep = |unit: &UnitSummary| {
        filter.is_none_or(|f| matches(f, &unit.name, unit.label(), &unit.tags))
    };
    let mut nodes = Vec::new();

    // Services in devlanes.yaml order, as the daemon lists them, running or
    // not; then exec units.
    let mut services = Vec::new();
    let mut ad_hoc = Vec::new();
    for unit in lane.units.iter().filter(|u| keep(u)) {
        let leaf = Node::leaf(RowId::Unit(unit.name.clone()));
        match role(unit) {
            Role::Service => services.push(leaf),
            Role::Exec => ad_hoc.push(leaf),
            _ => {}
        }
    }
    if !ad_hoc.is_empty() {
        services.push(Node {
            id: RowId::Group(Section::Services, AD_HOC.into()),
            children: ad_hoc,
            folded_by_default: false,
        });
    }
    if !services.is_empty() {
        nodes.push(section(Section::Services, services));
    }

    // Tasks in config order (`task.list`), then any that ran without being
    // listed there (an older daemon).
    let mut tasks_rows = Vec::new();
    let ran = |name: &str| {
        lane.units
            .iter()
            .any(|u| u.name == name && role(u) == Role::Task)
    };
    for task in tasks.unwrap_or_default() {
        let setup = task.name.starts_with("setup.") || task.name.starts_with("teardown.");
        let shown = filter.is_none_or(|f| matches(f, &task.name, &task.name, &[]));
        if !setup && shown {
            let id = if ran(&task.name) {
                RowId::Unit(task.name.clone())
            } else {
                RowId::Task(task.name.clone())
            };
            tasks_rows.push(Node::leaf(id));
        }
    }
    for unit in lane
        .units
        .iter()
        .filter(|u| role(u) == Role::Task && keep(u))
    {
        let id = RowId::Unit(unit.name.clone());
        if !tasks_rows.iter().any(|node| node.id == id) {
            tasks_rows.push(Node::leaf(id));
        }
    }
    if !tasks_rows.is_empty() {
        nodes.push(section(Section::Tasks, tasks_rows));
    }

    // Infra: the lane's own containers, the project's shared ones (hidden
    // ones too, drawn dimmed), then the setup and teardown steps.
    let mut infra = Vec::new();
    let shown = |c: &&ContainerSummary| filter.is_none_or(|f| matches(f, &c.name, &c.name, &[]));
    let own: Vec<&ContainerSummary> = lane
        .containers
        .iter()
        .filter(|c| !c.shared)
        .filter(shown)
        .collect();
    let shared: Vec<&ContainerSummary> = shared.iter().filter(shown).collect();
    for (name, list) in [(CONTAINERS, own), (SHARED, shared)] {
        if list.is_empty() {
            continue;
        }
        infra.push(Node {
            id: RowId::Group(Section::Infra, name.into()),
            children: list
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
        // devlanes.yaml order; by name for daemons that do not send it.
        steps.sort_by(|a, b| {
            let key = |u: &UnitSummary| (u.order.unwrap_or(u32::MAX), u.name.clone());
            key(a).cmp(&key(b))
        });
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
        match container_tone(Some(lane), container) {
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

/// Whether `container` is paused with `lane`: its own containers are while the
/// lane is frozen, shared ones never are.
pub fn frozen_with(
    lane: Option<&LaneSummary>,
    container: &devlanes_proto::ContainerSummary,
) -> bool {
    !container.shared
        && lane
            .and_then(|lane| lane.power.as_ref())
            .is_some_and(|power| power.level == "frozen")
}

/// How a container stands in `lane`, from `ContainerSummary.state`. A container
/// without a state (older daemons) is listed because it runs.
pub fn container_tone(
    lane: Option<&LaneSummary>,
    container: &devlanes_proto::ContainerSummary,
) -> ContainerTone {
    match container.state.as_deref() {
        None | Some("running" | "healthy") => ContainerTone::Up,
        // The daemon paused it with its frozen lane, which thaws on the next
        // request; paused by hand, it serves nothing until unpaused.
        Some("paused") if frozen_with(lane, container) => ContainerTone::Up,
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

    fn container(name: &str, shared: bool, hidden: bool) -> ContainerSummary {
        ContainerSummary {
            name: name.into(),
            shared,
            hidden,
            ..ContainerSummary::default()
        }
    }

    /// A paused container is up only when its own lane is frozen; one paused
    /// by hand, or a shared one, is not.
    #[test]
    fn paused_containers_are_up_only_in_their_frozen_lane() {
        let paused = |shared| ContainerSummary {
            state: Some("paused".into()),
            ..container("db", shared, false)
        };
        let mut frozen = lane(Vec::new(), "running");
        frozen.power = Some(devlanes_proto::LanePower {
            level: "frozen".into(),
            ..devlanes_proto::LanePower::default()
        });
        let awake = lane(Vec::new(), "running");
        assert_eq!(
            container_tone(Some(&frozen), &paused(false)),
            ContainerTone::Up
        );
        assert_eq!(
            container_tone(Some(&frozen), &paused(true)),
            ContainerTone::Down
        );
        assert_eq!(
            container_tone(Some(&awake), &paused(false)),
            ContainerTone::Down
        );
        assert_eq!(container_tone(None, &paused(false)), ContainerTone::Down);
    }

    #[test]
    fn services_keep_the_daemon_order_with_stopped_ones_inline() {
        // The daemon lists services in devlanes.yaml order; the list keeps it
        // and never moves a stopped service into a group of its own.
        let lane = lane(
            vec![
                unit("web", "ready", "service"),
                unit("api-users", "idle", "service"),
                unit("api-auth", "failed", "service"),
                unit("shell", "running", "exec"),
                unit("migrate", "exited", "task"),
                unit("setup.db", "exited", "task"),
            ],
            "running",
        );
        let rows = flatten(&tree(&lane, &[], None, None), &|_| None, false);
        assert_eq!(
            ids(&rows),
            [
                "services",
                "  web",
                "  api-users",
                "  api-auth",
                "  [ad-hoc]",
                "    shell",
                "tasks",
                "  migrate",
                "infra",
                "  [setup]",
            ]
        );
        assert!(!rows.iter().any(|row| row.id == RowId::WholeLane));
    }

    #[test]
    fn shared_containers_are_a_group_in_every_lane() {
        let mut own = lane(Vec::new(), "running");
        own.containers = vec![container("cache", false, false)];
        let shared = [
            container("postgres", true, false),
            container("pgbouncer", true, true),
        ];
        let rows = flatten(&tree(&own, &shared, None, None), &|_| None, false);
        assert_eq!(
            ids(&rows),
            [
                "infra",
                "  [containers]",
                "    cache",
                "  [shared]",
                "    postgres",
                "    pgbouncer",
            ]
        );
        // A lane with no containers of its own still lists the shared ones.
        let bare = lane(Vec::new(), "idle");
        let rows = flatten(&tree(&bare, &shared, None, None), &|_| None, false);
        assert_eq!(
            ids(&rows),
            ["infra", "  [shared]", "    postgres", "    pgbouncer"]
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
        let rows = flatten(&tree(&lane, &[], Some(&tasks), None), &|_| None, false);
        assert_eq!(ids(&rows), ["tasks", "  migrate", "  lint"]);
        assert_eq!(rows[1].id, RowId::Unit("migrate".into()));
        assert_eq!(rows[2].id, RowId::Task("lint".into()));
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
        let nodes = tree(&lane, &[], None, Some("api"));
        let rows = flatten(&nodes, &|_| Some(true), true);
        assert_eq!(ids(&rows), ["services", "  api-a", "  api-b"]);
        let rows = flatten(&tree(&lane, &[], None, Some("+front")), &|_| None, true);
        assert_eq!(ids(&rows), ["services", "  web-app"]);
    }

    #[test]
    fn folds_hide_children_and_parents_are_found() {
        let lane = lane(
            vec![
                unit("api-a", "ready", "service"),
                unit("api-b", "idle", "service"),
                unit("shell", "running", "exec"),
            ],
            "running",
        );
        let nodes = tree(&lane, &[], None, None);
        let ad_hoc = RowId::Group(Section::Services, AD_HOC.into());
        let services = RowId::Section(Section::Services);
        let rows = flatten(&nodes, &|node| (node.id == services).then_some(true), false);
        assert_eq!(ids(&rows), ["services"]);
        assert_eq!(rows[0].members.len(), 3);
        assert_eq!(
            parents(&nodes, &RowId::Unit("shell".into())),
            [ad_hoc, services.clone()]
        );
        assert_eq!(parents(&nodes, &RowId::Unit("api-b".into())), [services]);
    }
}
