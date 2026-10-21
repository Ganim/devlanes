//! Projects and worktree lanes (plan 5.1-5.3, decisions 2, 3 and 9): discovery from
//! a cwd, registration of every worktree, lane index assignment with primary-branch
//! moves, port blocks, per-lane config and layout, and live watching.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use devlanes_config::{
    ExpressionContext, NewWorktrees, OpaqueExtensions, ProjectConfig, RemovedWorktrees, Templated,
};
use devlanes_core::{
    WorktreeId,
    ports::{PortName, PortPool},
};
use devlanes_proto::{RpcError, events};
use devlanes_resolve::{LaneFacts, ResolveInput, lane_name};
use devlanes_state::{
    AllocationRequest, BlockOwner, DesiredState, LaneInput, ProjectInput, WorktreeInput,
};
use serde_json::{Value, json};

use crate::{
    engine::Inner,
    integrations::{InfraPortNeed, PortScope},
    model::{
        ConfigState, EnvMap, LaneRt, LaneSt, LaneView, ProjectRt, ProjectSt, ServiceInfo,
        config_error, lane_summary, lock,
    },
};

/// Worktree as reported by git (or the single non-git root).
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    pub identity: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub present: bool,
}

/// Lanes that appeared or disappeared during one re-list.
#[derive(Default)]
pub(crate) struct SyncReport {
    pub added: Vec<Arc<LaneRt>>,
    pub removed: Vec<Arc<LaneRt>>,
}

/// Internal failures carry their message; the CLI words them.
pub(crate) fn internal(code: &str, error: impl std::fmt::Display) -> RpcError {
    RpcError::new(code, json!({"message": error.to_string()}), 1)
}

/// Maps store failures; pool exhaustion keeps its own code, and the failures a
/// user can act on get a code and a message saying what to do.
pub(crate) fn store_error(error: devlanes_state::Error) -> RpcError {
    use devlanes_state::Error as E;
    match error {
        E::Allocation(error) => allocation_error(&error),
        E::ActiveRuns { units, tasks } => RpcError::new(
            "active_runs",
            json!({
                "units": units,
                "tasks": tasks,
                "message": format!(
                    "{units} unit run(s) and {tasks} task run(s) are still recorded as running; \
                     stop them with `devlanes stop` (orphaned units included) and wait for \
                     running tasks to finish, then retry"
                ),
            }),
            1,
        ),
        E::Conflict { constraint } => RpcError::new(
            "state_conflict",
            json!({
                "constraint": constraint,
                "message": format!(
                    "state.db already has a conflicting record ({constraint}); \
                     `devlanes status` shows what is still running"
                ),
            }),
            1,
        ),
        E::Corrupt(detail) => RpcError::new(
            "state_corrupt",
            json!({
                "detail": detail,
                "message": format!(
                    "state.db holds an unreadable value ({detail}); \
                     `devlanes daemon reset-state --yes` starts over"
                ),
            }),
            1,
        ),
        other => internal("state_error", other),
    }
}

/// Port allocation failures as machine codes.
pub(crate) fn allocation_error(error: &devlanes_core::ports::AllocationError) -> RpcError {
    use devlanes_core::ports::AllocationError as E;
    let (code, data) = match error {
        E::PoolExhausted { size } => ("pool_exhausted", json!({"block_size": size})),
        E::BlockConflict => ("port_block_conflict", json!({})),
        E::OffsetsExhausted { size } => ("block_too_small", json!({"block_size": size})),
        other => (
            "port_allocation_failed",
            json!({"message": other.to_string()}),
        ),
    };
    RpcError::new(code, data, 1)
}

/// Parses a `.env` file: `KEY=VALUE`, optional `export`, quotes and comments.
pub(crate) fn parse_dotenv(text: &str) -> EnvMap {
    let mut map = EnvMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            inner.replace("\\n", "\n").replace("\\\"", "\"")
        } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            inner.to_owned()
        } else {
            // Unquoted values end at an inline comment.
            value
                .split_once(" #")
                .map_or(value, |(v, _)| v)
                .trim()
                .to_owned()
        };
        map.insert(key.to_owned(), value);
    }
    map
}

/// Reads the lane's `.env`, falling back to the primary worktree's (plan 5.1).
pub(crate) async fn read_dotenv(lane_dir: PathBuf, primary_dir: PathBuf) -> EnvMap {
    tokio::task::spawn_blocking(move || {
        for dir in [lane_dir, primary_dir] {
            if let Ok(text) = std::fs::read_to_string(dir.join(".env")) {
                return parse_dotenv(&text);
            }
        }
        EnvMap::new()
    })
    .await
    .unwrap_or_default()
}

/// Loads `devlanes.yaml` from a lane's config directory. The local override and
/// `.env` fall back to the primary worktree's when the lane has none.
/// What the composition root registered that config validation needs: preset
/// option parsers and the contributors that name each preset's ports.
#[derive(Clone)]
pub(crate) struct ConfigChecks {
    pub presets: Vec<crate::integrations::PresetEntry>,
    pub contributors: Vec<Arc<dyn devlanes_core::contrib::Contributor>>,
}

impl Inner {
    /// The registered presets and contributors, for [`check_config`].
    pub(crate) fn config_checks(&self) -> ConfigChecks {
        ConfigChecks {
            presets: self.integ.presets.clone(),
            contributors: self.integ.contributors.clone(),
        }
    }
}

/// Validates preset options against the registered presets and counts each
/// preset's required ports against a fixed `lanes.block_size`.
pub(crate) fn check_config(
    config: &ProjectConfig,
    checks: &ConfigChecks,
) -> Result<devlanes_config::CheckReport, devlanes_config::ConfigError> {
    let validator = |name: &str, options: Option<&Value>| -> Result<(), String> {
        match checks.presets.iter().find(|preset| preset.name == name) {
            Some(preset) => (preset.parse)(options),
            None if checks.presets.is_empty() => Ok(()),
            None => Err(format!("unknown preset `{name}`")),
        }
    };
    let ports = |preset: &str| -> Vec<String> {
        let view = devlanes_core::contrib::ServiceView {
            name: "",
            platform: None,
            preset: Some(preset),
            tags: &[],
        };
        let mut names: Vec<String> = checks
            .contributors
            .iter()
            .flat_map(|contributor| contributor.required_ports(&view))
            .collect();
        names.sort();
        names.dedup();
        names
    };
    devlanes_config::validate_with_preset_ports(config, &validator, &ports)
}

pub(crate) async fn load_config(
    lane_dir: PathBuf,
    primary_dir: PathBuf,
    checks: ConfigChecks,
) -> ConfigState {
    tokio::task::spawn_blocking(move || {
        let file = lane_dir.join("devlanes.yaml");
        let source = match std::fs::read_to_string(&file) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ConfigState::Missing;
            }
            Err(error) => {
                return ConfigState::Invalid(RpcError::new(
                    "config_error",
                    json!({"message": error.to_string(), "file": file, "line": 0, "column": 0}),
                    1,
                ));
            }
        };
        let mut local_file = lane_dir.join("devlanes.local.yaml");
        let mut local = std::fs::read_to_string(&local_file).ok();
        if local.is_none() {
            local_file = primary_dir.join("devlanes.local.yaml");
            local = std::fs::read_to_string(&local_file).ok();
        }
        let checked =
            devlanes_config::parse_project_with::<OpaqueExtensions>(&source, local.as_deref())
                .and_then(|config| check_config(&config, &checks).map(|_| config));
        match checked {
            Ok(config) => ConfigState::Loaded(Arc::new(config)),
            Err(error) => {
                let mut shaped = config_error(&error);
                // Parsing names sources generically; report the real paths.
                let real = if error.location.file == "devlanes.local.yaml" {
                    local_file
                } else {
                    file
                };
                shaped.data["file"] = json!(real);
                ConfigState::Invalid(shaped)
            }
        }
    })
    .await
    .unwrap_or_else(|error| ConfigState::Invalid(internal("config_error", error)))
}

/// Evaluates a literal-or-expression without lane context (block size, max).
fn literal<T: Clone + serde::de::DeserializeOwned>(value: &Option<Templated<T>>) -> Option<T> {
    value
        .as_ref()
        .and_then(|value| value.evaluate(&ExpressionContext::default()).ok())
}

impl Inner {
    /// Finds a registered project by a present worktree containing `cwd`, else
    /// loads the project whose `devlanes.yaml` is the nearest ancestor of `cwd`.
    pub(crate) async fn project_for_cwd(
        self: &Arc<Self>,
        cwd: &Path,
    ) -> Result<Option<Arc<ProjectRt>>, RpcError> {
        if cwd.as_os_str().is_empty() {
            return Ok(None);
        }
        for (project, lane) in self.all_lanes() {
            let st = lane.st();
            if st.present && cwd.starts_with(&st.path) {
                return Ok(Some(project));
            }
        }
        let Some(config) = devlanes_git::find_config(cwd)
            .await
            .map_err(|error| internal("io", error))?
        else {
            return Ok(None);
        };
        self.load_project(&config).await.map(Some)
    }

    /// Discovers and registers the project of a config file (decision 9 identity).
    pub(crate) async fn load_project(
        self: &Arc<Self>,
        config_file: &Path,
    ) -> Result<Arc<ProjectRt>, RpcError> {
        let config_dir = config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let config =
            match load_config(config_dir.clone(), config_dir.clone(), self.config_checks()).await {
                ConfigState::Loaded(config) => config,
                ConfigState::Invalid(error) => return Err(error),
                ConfigState::Missing => {
                    return Err(RpcError::new(
                        "not_found",
                        json!({"reference": config_file}),
                        3,
                    ));
                }
            };
        let repository = devlanes_git::discover(&config_dir, config.lanes.primary.as_deref())
            .await
            .map_err(|error| internal("git_failed", error))?;
        let (root, common_dir, primary_branch, relative, trees) = match &repository {
            Some(repo) => {
                let relative = config_dir
                    .strip_prefix(&repo.root)
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let trees = repo.worktrees.iter().map(tree).collect();
                (
                    repo.root.clone(),
                    Some(repo.common_dir.clone()),
                    repo.primary_branch.clone(),
                    relative,
                    trees,
                )
            }
            None => (
                config_dir.clone(),
                None,
                config
                    .lanes
                    .primary
                    .clone()
                    .unwrap_or_else(|| "main".into()),
                PathBuf::new(),
                vec![Tree {
                    identity: "main".into(),
                    path: config_dir.clone(),
                    branch: None,
                    present: true,
                }],
            ),
        };
        let identity = common_dir.clone().unwrap_or_else(|| root.clone());
        let existing = self.all_projects().into_iter().find(|project| {
            let st = project.st();
            st.common_dir.as_ref().unwrap_or(&st.root) == &identity
        });
        if let Some(project) = existing {
            self.sync_worktrees(&project, trees).await?;
            return Ok(project);
        }
        let record = self
            .store
            .upsert_project(ProjectInput {
                name: config.project.clone(),
                common_dir: common_dir.clone(),
                root_path: root.clone(),
                primary_branch: primary_branch.clone(),
            })
            .await
            .map_err(store_error)?;
        self.store
            .set_project_config_dir(record.id, relative.clone())
            .await
            .map_err(store_error)?;
        let project = self
            .register_project(
                record.id,
                config.project.clone(),
                root,
                common_dir,
                primary_branch,
                relative,
            )
            .await?;
        self.sync_worktrees(&project, trees).await?;
        self.watch_project(&project, repository);
        Ok(project)
    }

    /// Creates the in-memory project and loads its persisted worktrees and lanes.
    pub(crate) async fn register_project(
        self: &Arc<Self>,
        id: devlanes_core::ProjectId,
        name: String,
        root: PathBuf,
        common_dir: Option<PathBuf>,
        primary_branch: String,
        config_dir: PathBuf,
    ) -> Result<Arc<ProjectRt>, RpcError> {
        if let Some(project) = self.project(id) {
            return Ok(project);
        }
        let worktrees = self.store.worktrees(id).await.map_err(store_error)?;
        let lanes = self.store.lanes(Some(id)).await.map_err(store_error)?;
        let mut map = BTreeMap::new();
        for worktree in worktrees {
            let mut st = LaneSt::new(
                worktree.identity.clone(),
                worktree.path.clone(),
                worktree.branch.clone(),
                worktree.present,
            );
            st.record = lanes
                .iter()
                .find(|lane| lane.worktree_id == worktree.id && lane.cleaned_at.is_none())
                .cloned();
            if let Some(record) = &st.record {
                st.allocation = self
                    .store
                    .allocation(BlockOwner::Lane(record.id))
                    .await
                    .map_err(store_error)?;
                st.profiles = record.profiles.clone();
            }
            map.insert(worktree.id, new_lane(worktree.id, st));
        }
        let project = Arc::new(ProjectRt {
            id,
            config_dir,
            git: common_dir.is_some(),
            sync: tokio::sync::Mutex::new(()),
            st: std::sync::Mutex::new(ProjectSt {
                name,
                root,
                common_dir,
                primary_branch,
                last_env: None,
                lanes: map,
            }),
            watching: std::sync::atomic::AtomicBool::new(false),
            watch: self.shutdown.child_token(),
        });
        lock(&self.projects).insert(id, project.clone());
        Ok(project)
    }

    /// The primary worktree: lane 0, else the worktree on the primary branch, else
    /// the main checkout, else the project root.
    pub(crate) fn primary_path(&self, project: &ProjectRt) -> PathBuf {
        let st = project.st();
        let lanes: Vec<_> = st.lanes.values().cloned().collect();
        let primary = st.primary_branch.clone();
        let root = st.root.clone();
        drop(st);
        let mut fallback = None;
        for lane in &lanes {
            let lane = lane.st();
            if lane.index() == Some(0) {
                return lane.path.clone();
            }
            if lane.present && lane.branch.as_deref() == Some(primary.as_str()) {
                fallback = Some(lane.path.clone());
            }
        }
        if let Some(path) = fallback {
            return path;
        }
        lanes
            .iter()
            .find(|lane| lane.st().identity == "main")
            .map_or(root, |lane| lane.st().path.clone())
    }

    /// Re-lists worktrees from git (or the single non-git root) and applies the diff.
    pub(crate) async fn relist(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
    ) -> Result<SyncReport, RpcError> {
        let (root, common_dir) = {
            let st = project.st();
            (st.root.clone(), st.common_dir.clone())
        };
        let Some(common_dir) = common_dir else {
            return Ok(SyncReport::default());
        };
        let trees = match devlanes_git::list(&root, &common_dir).await {
            Ok(list) => list.iter().map(tree).collect(),
            // The registered root may itself be a removed worktree; ask another one.
            Err(_) => {
                let alternative = project
                    .lanes()
                    .into_iter()
                    .map(|lane| lane.st().path.clone())
                    .find(|path| path.exists() && *path != root);
                match alternative {
                    Some(path) => devlanes_git::list(&path, &common_dir)
                        .await
                        .map_err(|error| internal("git_failed", error))?
                        .iter()
                        .map(tree)
                        .collect(),
                    None => return Ok(SyncReport::default()),
                }
            }
        };
        self.sync_worktrees(project, trees).await
    }

    /// Applies a worktree list: registers new worktrees, marks missing ones, assigns
    /// lanes and reloads configs. Never stops anything by itself.
    pub(crate) async fn sync_worktrees(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        trees: Vec<Tree>,
    ) -> Result<SyncReport, RpcError> {
        let _sync = project.sync.lock().await;
        let mut report = SyncReport::default();
        let mut seen = BTreeSet::new();
        let mut changed: Vec<Arc<LaneRt>> = Vec::new();
        for tree in trees.into_iter().filter(|t| !t.identity.is_empty()) {
            let record = self
                .store
                .upsert_worktree(WorktreeInput {
                    project_id: project.id,
                    identity: tree.identity.clone(),
                    path: tree.path.clone(),
                    branch: tree.branch.clone(),
                    present: tree.present,
                })
                .await
                .map_err(store_error)?;
            seen.insert(record.id);
            let existing = project.st().lanes.get(&record.id).cloned();
            let lane = match existing {
                Some(lane) => lane,
                None => {
                    let lane = new_lane(
                        record.id,
                        LaneSt::new(
                            tree.identity.clone(),
                            tree.path.clone(),
                            tree.branch.clone(),
                            false,
                        ),
                    );
                    project.st().lanes.insert(record.id, lane.clone());
                    lane
                }
            };
            let (was_present, moved, rebranched) = {
                let mut st = lane.st();
                let was_present = st.present;
                let moved = st.path != tree.path;
                let rebranched = st.branch != tree.branch;
                st.path = tree.path.clone();
                st.branch = tree.branch.clone();
                st.present = tree.present;
                (was_present, moved, rebranched)
            };
            if tree.present
                && (!was_present || moved || matches!(lane.st().config, ConfigState::Missing))
            {
                self.reload_config(project, &lane).await;
            }
            if tree.present && !was_present {
                report.added.push(lane.clone());
            }
            if !tree.present && was_present {
                report.removed.push(lane.clone());
            }
            if moved || rebranched || tree.present != was_present {
                changed.push(lane.clone());
            }
        }
        for lane in project.lanes() {
            if seen.contains(&lane.worktree) {
                continue;
            }
            let was_present = {
                let mut st = lane.st();
                std::mem::replace(&mut st.present, false)
            };
            if was_present {
                let _ = self.store.mark_worktree_missing(lane.worktree).await;
                report.removed.push(lane.clone());
                changed.push(lane.clone());
            }
        }
        let assigned = self.assign_lanes(project).await?;
        // Only newly assigned lanes count as "added" for new_worktrees policies.
        report.added.retain(|lane| {
            assigned.iter().any(|a| Arc::ptr_eq(a, lane)) || lane.st().record.is_some()
        });
        changed.extend(assigned);
        self.update_moves(project);
        self.refresh_views(project).await;
        for lane in changed {
            self.emit_lane(project, &lane);
        }
        Ok(report)
    }

    /// Gives present worktrees with a config a lane index and port block
    /// (decision 2: lane 0 for the primary branch, else lowest free index >= 1).
    async fn assign_lanes(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
    ) -> Result<Vec<Arc<LaneRt>>, RpcError> {
        let primary = project.st().primary_branch.clone();
        let mut lanes = project.lanes();
        lanes.sort_by_key(|lane| lane.st().path.clone());
        let mut assigned = Vec::new();
        for lane in lanes {
            let (config, branch) = {
                let st = lane.st();
                if !st.present || st.record.is_some() {
                    continue;
                }
                let Some(config) = st.config.loaded().cloned() else {
                    continue;
                };
                (config, st.branch.clone())
            };
            let used: BTreeSet<u32> = project
                .lanes()
                .iter()
                .filter_map(|l| l.st().index())
                .collect();
            if let Some(max) = literal(&config.lanes.max)
                && used.len() >= usize::try_from(max).unwrap_or(usize::MAX)
            {
                lane.st().unassigned = Some(RpcError::new(
                    "unassigned",
                    json!({"reason": "lanes_max", "max": max}),
                    1,
                ));
                continue;
            }
            let index = if branch.as_deref() == Some(primary.as_str()) && !used.contains(&0) {
                0
            } else {
                (1..).find(|i| !used.contains(i)).unwrap_or(1)
            };
            let record = self
                .store
                .upsert_lane(LaneInput {
                    project_id: project.id,
                    worktree_id: lane.worktree,
                    index,
                    name: lane_name(index),
                    desired_state: DesiredState::Stopped,
                    profile: None,
                    profiles: Vec::new(),
                })
                .await
                .map_err(store_error)?;
            lane.st().record = Some(record.clone());
            match self.allocate_lane(project, &lane, &[]).await {
                Ok(_) => {
                    lane.st().unassigned = None;
                    assigned.push(lane);
                }
                Err(error) if error.code == "pool_exhausted" => {
                    let _ = self.store.delete_lane(record.id).await;
                    let mut st = lane.st();
                    st.record = None;
                    st.allocation = None;
                    st.unassigned = Some(RpcError::new(
                        "unassigned",
                        json!({"reason": "pool_exhausted"}),
                        1,
                    ));
                }
                Err(error) => {
                    lane.st().last_error = Some(error);
                    assigned.push(lane);
                }
            }
        }
        Ok(assigned)
    }

    /// Computes `moves_to` for every assigned lane (applied at its next start).
    pub(crate) fn update_moves(&self, project: &ProjectRt) {
        let primary = project.st().primary_branch.clone();
        let lanes = project.lanes();
        let used: BTreeSet<u32> = lanes.iter().filter_map(|l| l.st().index()).collect();
        let free = (1..).find(|i| !used.contains(i)).unwrap_or(1);
        for lane in lanes {
            let mut st = lane.st();
            let Some(index) = st.index() else {
                st.moves_to = None;
                continue;
            };
            let on_primary = st.branch.as_deref() == Some(primary.as_str());
            st.moves_to = match (on_primary, index) {
                (true, 0) | (false, 1..) => None,
                (true, _) => Some(0),
                (false, 0) => Some(free),
            };
        }
    }

    /// Applies a pending move before a start. Lane 0's current holder is moved out
    /// only while it runs nothing; otherwise the move waits and the start proceeds.
    pub(crate) async fn apply_move(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
    ) -> Result<(), RpcError> {
        let _sync = project.sync.lock().await;
        self.update_moves(project);
        let (target, record) = {
            let st = lane.st();
            match (st.moves_to, st.record.clone()) {
                (Some(target), Some(record)) => (target, record),
                _ => return Ok(()),
            }
        };
        if target == 0
            && let Some(holder) = project.lane_by_index(0)
            && !Arc::ptr_eq(&holder, lane)
        {
            if holder.st().any_running() {
                return Ok(());
            }
            let used: BTreeSet<u32> = project
                .lanes()
                .iter()
                .filter_map(|l| l.st().index())
                .collect();
            let free = (1..).find(|i| !used.contains(i)).unwrap_or(1);
            self.set_index(project, &holder, free).await?;
        }
        if target != 0
            && project
                .lane_by_index(target)
                .is_some_and(|other| !Arc::ptr_eq(&other, lane))
        {
            return Ok(());
        }
        let _ = record;
        self.set_index(project, lane, target).await?;
        self.update_moves(project);
        Ok(())
    }

    /// Persists a new index for a lane, keeping its block, desired state and profile.
    async fn set_index(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        index: u32,
    ) -> Result<(), RpcError> {
        let Some(record) = lane.st().record.clone() else {
            return Ok(());
        };
        let updated = self
            .store
            .upsert_lane(LaneInput {
                project_id: project.id,
                worktree_id: lane.worktree,
                index,
                name: lane_name(index),
                desired_state: record.desired_state,
                profile: record.profile.clone(),
                profiles: record.profiles.clone(),
            })
            .await
            .map_err(store_error)?;
        lane.st().record = Some(updated);
        self.emit_lane(project, lane);
        Ok(())
    }

    /// Allocates (or resizes) the lane's block for the config's ports plus `infra`
    /// needs, and the project's shared block when shared infra needs ports.
    pub(crate) async fn allocate_lane(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        infra: &[InfraPortNeed],
    ) -> Result<devlanes_state::Allocation, RpcError> {
        let (config, record, facts_env) = {
            let st = lane.st();
            let config = st
                .config
                .loaded()
                .cloned()
                .ok_or_else(|| RpcError::new("no_config", json!({"path": st.path}), 1))?;
            let record = st
                .record
                .clone()
                .ok_or_else(|| RpcError::new("unassigned", json!({"reason": "unassigned"}), 1))?;
            (config, record, st.env.clone())
        };
        let needs = devlanes_resolve::port_needs(&config, &self.integ.contributors)
            .map_err(|error| crate::resolve::resolve_error(&error))?;
        let mut names = needs.names.clone();
        for need in infra.iter().filter(|n| n.scope == PortScope::Lane) {
            names.insert(PortName::new(need.unit.clone(), need.name.clone()));
        }
        let pool = self.pool()?;
        let facts = self.facts(project, lane)?;
        let (lane_dir, primary_dir) = self.config_dirs(project, lane);
        let dotenv = read_dotenv(lane_dir, primary_dir).await;
        let client_env = facts_env.map(|env| (*env).clone()).unwrap_or_default();
        let fixed_start =
            devlanes_resolve::fixed_block_start(&config, &facts, &client_env, &dotenv)
                .map_err(|error| crate::resolve::resolve_error(&error))?;
        let allocation = self
            .store
            .allocate(AllocationRequest {
                owner: BlockOwner::Lane(record.id),
                pool,
                fixed_size: literal(&config.lanes.block_size),
                fixed_start,
                names,
                reserved: needs.reserved.clone(),
            })
            .await
            .map_err(store_error)?;
        let shared: BTreeSet<PortName> = infra
            .iter()
            .filter(|n| n.scope == PortScope::Shared)
            .map(|n| PortName::new(n.unit.clone(), n.name.clone()))
            .collect();
        if !shared.is_empty() {
            self.store
                .allocate(AllocationRequest {
                    owner: BlockOwner::Project(project.id),
                    pool,
                    fixed_size: None,
                    fixed_start: None,
                    names: shared,
                    reserved: BTreeSet::new(),
                })
                .await
                .map_err(store_error)?;
        }
        lane.st().allocation = Some(allocation.clone());
        Ok(allocation)
    }

    /// The machine-wide pool from the user config.
    pub(crate) fn pool(&self) -> Result<PortPool, RpcError> {
        let (start, end) = self
            .opts
            .user
            .ports
            .bounds()
            .map_err(|error| config_error(&error))?;
        Ok(PortPool { start, end })
    }

    /// Lane facts for resolution.
    pub(crate) fn facts(&self, project: &ProjectRt, lane: &LaneRt) -> Result<LaneFacts, RpcError> {
        let primary = self.primary_path(project);
        let name = project.name();
        let st = lane.st();
        let index = st
            .index()
            .ok_or_else(|| RpcError::new("unassigned", json!({"reason": "unassigned"}), 1))?;
        Ok(LaneFacts::new(
            name,
            primary,
            index,
            st.path.clone(),
            st.branch.clone(),
        ))
    }

    /// `(lane config dir, primary config dir)`.
    pub(crate) fn config_dirs(&self, project: &ProjectRt, lane: &LaneRt) -> (PathBuf, PathBuf) {
        let primary = self.primary_path(project).join(&project.config_dir);
        (lane.st().path.join(&project.config_dir), primary)
    }

    /// Reloads one lane's config. A change while units run sets `config_changed`;
    /// running units are never touched (plan 5.8).
    pub(crate) async fn reload_config(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
    ) {
        let (lane_dir, primary_dir) = self.config_dirs(project, lane);
        let config = self
            .with_resolver_domain(load_config(lane_dir, primary_dir, self.config_checks()).await);
        let mut st = lane.st();
        let before = match &st.config {
            ConfigState::Loaded(config) => serde_json::to_value(config.as_ref()).ok(),
            _ => None,
        };
        let after = match &config {
            ConfigState::Loaded(config) => serde_json::to_value(config.as_ref()).ok(),
            _ => None,
        };
        let differs = before != after || before.is_none();
        if differs && st.any_running() {
            st.config_changed = true;
        }
        st.config = config;
    }

    /// Recomputes idle layouts (ports, URLs, routes) for every assigned lane, lane 0
    /// first so the others see `lanes.main.svc`.
    pub(crate) async fn refresh_views(self: &Arc<Self>, project: &Arc<ProjectRt>) {
        let mut lanes = project.lanes();
        lanes.sort_by_key(|lane| {
            lane.st()
                .index()
                .map_or(u32::MAX, |i| if i == 0 { 0 } else { i })
        });
        let mut main_svc: Option<Value> = None;
        for lane in lanes {
            let index = lane.st().index();
            if index.is_none() {
                lane.st().view = None;
                continue;
            }
            // A config that gained ports needs offsets before its layout resolves.
            if self.missing_offsets(&lane) {
                let _ = self.allocate_lane(project, &lane, &[]).await;
            }
            let view = self.compute_view(project, &lane, main_svc.as_ref()).await;
            match view {
                Ok(view) => {
                    if index == Some(0) {
                        main_svc = Some(view.svc.clone());
                    }
                    let mut st = lane.st();
                    st.view = Some(Arc::new(view));
                    if st
                        .last_error
                        .as_ref()
                        .is_some_and(|e| e.code == "config_error")
                    {
                        st.last_error = None;
                    }
                }
                Err(error) => {
                    let mut st = lane.st();
                    st.view = None;
                    if !matches!(st.config, ConfigState::Missing | ConfigState::Invalid(_)) {
                        st.last_error = Some(error);
                    }
                }
            }
        }
        self.publish_routes();
    }

    /// True when the lane's config names a port without a persisted offset.
    fn missing_offsets(&self, lane: &LaneRt) -> bool {
        let st = lane.st();
        let (Some(config), Some(allocation)) = (st.config.loaded(), &st.allocation) else {
            return false;
        };
        devlanes_resolve::port_needs(config, &self.integ.contributors).is_ok_and(|needs| {
            needs
                .names
                .iter()
                .any(|name| !allocation.offsets.contains_key(name))
                || needs
                    .reserved
                    .iter()
                    .any(|offset| !allocation.reserved.contains(offset))
        })
    }

    /// Layout of one lane without contributors or secrets.
    async fn compute_view(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        lanes_main: Option<&Value>,
    ) -> Result<LaneView, RpcError> {
        let (config, allocation, env, infra_ns) = {
            let st = lane.st();
            let config = match &st.config {
                ConfigState::Loaded(config) => config.clone(),
                ConfigState::Invalid(error) => return Err(error.clone()),
                ConfigState::Missing => {
                    return Err(RpcError::new("no_config", json!({"path": st.path}), 1));
                }
            };
            let allocation = st
                .allocation
                .clone()
                .ok_or_else(|| RpcError::new("unassigned", json!({"reason": "no_ports"}), 1))?;
            (config, allocation, st.env.clone(), st.infra_ns.clone())
        };
        let facts = self.facts(project, lane)?;
        let (lane_dir, primary_dir) = self.config_dirs(project, lane);
        let dotenv = read_dotenv(lane_dir, primary_dir).await;
        let client_env = env.map(|env| (*env).clone()).unwrap_or_default();
        let infra = infra_ns.unwrap_or(Value::Null);
        let mut input = ResolveInput::new(&config, &facts, allocation.block, &allocation.offsets);
        input.client_env = &client_env;
        input.dotenv = &dotenv;
        input.infra = &infra;
        input.lanes_main = lanes_main;
        input.contributors = &self.integ.contributors;
        input.ca_env = &self.integ.ca_env;
        input.public_https_port = self.integ.routes.https_port();
        let layout = devlanes_resolve::lane_layout(&input)
            .map_err(|error| crate::resolve::resolve_error(&error))?;
        let services = layout
            .services
            .values()
            .map(|service| ServiceInfo {
                name: service.name.clone(),
                unit: service.unit.clone(),
                port: service.port,
                ports: service.ports.clone(),
                url: service.url.clone(),
                public_url: service.public_url.clone(),
                tags: service.tags.clone(),
                hidden: service.hidden,
                autostart: service.autostart,
                color: service.color.clone(),
            })
            .collect();
        let infra_routes = infra_routes(&config, &facts, &layout, &infra, &client_env, &dotenv);
        Ok(LaneView {
            domain: layout.domain.clone(),
            port: layout.port,
            vars: layout.vars.clone(),
            block: (allocation.block.start, allocation.block.size),
            services,
            routes: layout.routes.clone(),
            infra_routes,
            lane_env: layout.lane_env().clone(),
            svc: layout.svc_namespace(),
        })
    }

    /// Emits `lane.changed` with a fresh summary and republishes routes.
    pub(crate) fn emit_lane(&self, project: &ProjectRt, lane: &LaneRt) {
        let mut summary = lane_summary(&project.name(), &lane.st());
        self.fill_queued(&mut summary);
        self.bus
            .emit(events::LANE_CHANGED, json!({"lane": summary}));
        self.publish_routes();
    }

    /// Starts git and config watchers for a project once.
    pub(crate) fn watch_project(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        repository: Option<devlanes_git::Repository>,
    ) {
        if !self.opts.watch || !project.claim_watch() {
            return;
        }
        if let Some(repository) = repository {
            match devlanes_git::watch(repository) {
                Ok(mut updates) => {
                    let engine = self.clone();
                    let project = project.clone();
                    tokio::spawn(async move {
                        loop {
                            let update = tokio::select! {
                                () = project.watch.cancelled() => break,
                                update = updates.recv() => update,
                            };
                            let Some(update) = update else { break };
                            let Ok(list) = update else { continue };
                            let trees = list.iter().map(tree).collect();
                            match engine.sync_worktrees(&project, trees).await {
                                Ok(report) => engine.apply_policies(&project, report).await,
                                Err(error) => {
                                    tracing::warn!(code = %error.code, "worktree sync failed")
                                }
                            }
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "worktree watch unavailable"),
            }
        }
        let engine = self.clone();
        let project = project.clone();
        tokio::spawn(async move { engine.watch_configs(project).await });
    }

    /// Polls each lane's config files and reloads on change. Polling stays correct
    /// across editors that replace files and needs no per-lane OS watch handles.
    async fn watch_configs(self: Arc<Self>, project: Arc<ProjectRt>) {
        let mut stamps: BTreeMap<WorktreeId, Vec<Option<std::time::SystemTime>>> = BTreeMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                () = project.watch.cancelled() => return,
                _ = tick.tick() => {}
            }
            for lane in project.lanes() {
                let (present, dir) = {
                    let st = lane.st();
                    (st.present, st.path.join(&project.config_dir))
                };
                if !present {
                    continue;
                }
                let current = tokio::task::spawn_blocking(move || {
                    ["devlanes.yaml", "devlanes.local.yaml"]
                        .iter()
                        .map(|file| {
                            std::fs::metadata(dir.join(file))
                                .and_then(|m| m.modified())
                                .ok()
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                let previous = stamps.insert(lane.worktree, current.clone());
                if previous.is_some_and(|previous| previous != current) {
                    self.reload_config(&project, &lane).await;
                    if lane.st().record.is_none() {
                        let _ = self.sync_worktrees(&project, Vec::new()).await;
                    }
                    self.refresh_views(&project).await;
                    self.emit_lane(&project, &lane);
                }
            }
        }
    }

    /// Runs `new_worktrees` / `removed_worktrees` for one re-list.
    pub(crate) async fn apply_policies(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        report: SyncReport,
    ) {
        for lane in report.added {
            let Some(config) = lane.st().config.loaded().cloned() else {
                continue;
            };
            if lane.st().record.is_none() {
                continue;
            }
            let policy = config.lanes.new_worktrees.unwrap_or(NewWorktrees::Register);
            if matches!(policy, NewWorktrees::Register) {
                continue;
            }
            let env = project.st().last_env.clone();
            let Some(env) = env else {
                let mut st = lane.st();
                st.last_error = Some(RpcError::new(
                    "needs_env",
                    json!({"lane": st.reference(&project.name())}),
                    1,
                ));
                drop(st);
                self.emit_lane(project, &lane);
                continue;
            };
            lane.st().env = Some(env);
            let engine = self.clone();
            let project = project.clone();
            tokio::spawn(async move {
                let setup_only = matches!(policy, NewWorktrees::Setup);
                if let Err(error) = engine.auto_start(&project, &lane, setup_only).await {
                    lane.st().last_error = Some(error);
                    engine.emit_lane(&project, &lane);
                }
            });
        }
        for lane in report.removed {
            let Some(config) = lane.st().config.loaded().cloned() else {
                continue;
            };
            let policy = config
                .lanes
                .removed_worktrees
                .unwrap_or(RemovedWorktrees::Stop);
            let engine = self.clone();
            let project = project.clone();
            tokio::spawn(async move {
                let result = match policy {
                    RemovedWorktrees::Stop => engine
                        .stop_lane(&project, &lane, None, None)
                        .await
                        .map(drop),
                    RemovedWorktrees::Clean => {
                        engine.clean_lane(&project, &lane, None).await.map(drop)
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(code = %error.code, "removed worktree policy failed");
                }
            });
        }
    }
}

/// Creates a lane runtime around its state.
fn new_lane(worktree: WorktreeId, st: LaneSt) -> Arc<LaneRt> {
    Arc::new(LaneRt {
        worktree,
        op: tokio::sync::Mutex::new(()),
        st: std::sync::Mutex::new(st),
        watch: tokio_util::sync::CancellationToken::new(),
    })
}

/// Converts git's view of a worktree.
fn tree(worktree: &devlanes_git::Worktree) -> Tree {
    Tree {
        identity: worktree.identity.clone(),
        path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        present: worktree.present,
    }
}

/// Expression context with `project`, `lane` (incl. vars and port) and `env`.
pub(crate) fn lane_context(
    facts: &LaneFacts,
    domain: &str,
    port: u16,
    vars: &indexmap::IndexMap<String, devlanes_config::Scalar>,
    infra: &Value,
    env: &EnvMap,
    dotenv: &EnvMap,
) -> ExpressionContext {
    let mut lane = json!({
        "index": facts.index,
        "name": facts.name,
        "label": facts.label,
        "path": facts.path.display().to_string(),
        "domain": domain,
        "port": port,
    });
    if let Some(map) = lane.as_object_mut() {
        for (key, value) in vars {
            if let Ok(value) = serde_json::to_value(value) {
                map.insert(key.clone(), value);
            }
        }
    }
    let mut env_ns = serde_json::Map::new();
    for source in [dotenv, env] {
        for (key, value) in source {
            env_ns.insert(key.clone(), json!(value));
        }
    }
    ExpressionContext {
        project: json!({"name": facts.project, "primary_path": facts.primary_path.display().to_string()}),
        lane,
        infra: if infra.is_null() {
            json!({})
        } else {
            infra.clone()
        },
        env: Value::Object(env_ns),
        ..ExpressionContext::default()
    }
}

/// Container routes from `infra.hosts`, using known host ports.
fn infra_routes(
    config: &ProjectConfig,
    facts: &LaneFacts,
    layout: &devlanes_resolve::LaneLayout,
    infra: &Value,
    env: &EnvMap,
    dotenv: &EnvMap,
) -> Vec<(String, String, u16)> {
    let context = lane_context(
        facts,
        &layout.domain,
        layout.port,
        &layout.vars,
        infra,
        env,
        dotenv,
    );
    let mut routes = Vec::new();
    for (container, hosts) in &config.infra.hosts {
        let Some(port) = infra
            .get(container)
            .and_then(|entry| entry.get("port"))
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
        else {
            continue;
        };
        for host in hosts {
            let template = match host {
                devlanes_config::HostConfig::Name(name) => name,
                devlanes_config::HostConfig::Route(route) => &route.host,
            };
            if let Ok(host) = template.evaluate(&context) {
                routes.push((host, container.clone(), port));
            }
        }
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// `.env` parsing handles export, quotes and inline comments.
    fn dotenv_forms() {
        let env = parse_dotenv("# c\nexport A=1\nB=\"two words\"\nC='x#y'\nD=plain # note\n=bad\n");
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "two words");
        assert_eq!(env["C"], "x#y");
        assert_eq!(env["D"], "plain");
        assert_eq!(env.len(), 4);
    }
}
