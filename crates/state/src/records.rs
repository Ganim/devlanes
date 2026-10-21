//! Typed persistence inputs and records. No environment or launch command is stored.

use devlanes_core::{
    Actor, ExitOutcome, LaneId, ProcessIdentity, ProjectId, UnitRunId, WorktreeId,
    ports::{PortBlock, PortName, PortPool},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Project identity is its canonical git common directory, or its non-git root.
#[derive(Debug, Clone)]
pub struct ProjectInput {
    pub name: String,
    pub common_dir: Option<PathBuf>,
    pub root_path: PathBuf,
    pub primary_branch: String,
}

/// Persisted project metadata. Names may coincide until running-state validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub common_dir: Option<PathBuf>,
    pub root_path: PathBuf,
    pub primary_branch: String,
    pub last_start: Option<i64>,
}

/// Worktree identity is `main` or the stable git administrative directory name.
#[derive(Debug, Clone)]
pub struct WorktreeInput {
    pub project_id: ProjectId,
    pub identity: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub present: bool,
}

/// Worktree registration survives moves and missing working directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub identity: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub present: bool,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// Desired state survives daemon restarts; live state belongs to the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    /// Returns the stable database representation.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

/// Registers or updates the lane belonging to a worktree.
#[derive(Debug, Clone)]
pub struct LaneInput {
    pub project_id: ProjectId,
    pub worktree_id: WorktreeId,
    pub index: u32,
    pub name: String,
    pub desired_state: DesiredState,
    pub profile: Option<String>,
}

/// Lane state retained until an explicit clean/delete operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    pub id: LaneId,
    pub project_id: ProjectId,
    pub worktree_id: WorktreeId,
    pub index: u32,
    pub name: String,
    pub desired_state: DesiredState,
    pub profile: Option<String>,
    pub created_at: i64,
    pub cleaned_at: Option<i64>,
}

/// A lane block or a project's shared-infrastructure block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockOwner {
    Lane(LaneId),
    Project(ProjectId),
}

/// One machine-wide persisted range and its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRecord {
    pub project_id: ProjectId,
    pub owner: BlockOwner,
    pub block: PortBlock,
}

/// Atomic allocation request. `names` excludes literal `PORT` overrides.
/// Explicit expression reservations persist until `gc_offsets` is requested.
#[derive(Debug, Clone)]
pub struct AllocationRequest {
    pub owner: BlockOwner,
    pub pool: PortPool,
    pub fixed_size: Option<u16>,
    pub fixed_start: Option<u16>,
    pub names: BTreeSet<PortName>,
    pub reserved: BTreeSet<u16>,
}

/// Stable names include removed services until GC. `previous` signals a port change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub block: PortBlock,
    pub previous: Option<PortBlock>,
    pub offsets: BTreeMap<PortName, u16>,
    pub reserved: BTreeSet<u16>,
}

/// Spawn record for reconciliation; values must come from verified OS identities.
#[derive(Debug, Clone)]
pub struct UnitRunInput {
    pub lane_id: LaneId,
    pub unit: String,
    pub process: ProcessIdentity,
    pub actor: Actor,
    pub log_path: Option<PathBuf>,
}

/// Run history and identity; unfinished rows are reconciliation candidates only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRun {
    pub id: UnitRunId,
    pub lane_id: LaneId,
    pub unit: String,
    pub process: ProcessIdentity,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: Option<ExitOutcome>,
    pub actor: Actor,
    pub log_path: Option<PathBuf>,
}
