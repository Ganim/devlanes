//! Private Diesel projections and write models. Public records stay SQL-independent.

pub(crate) use crate::paths::{path_bytes, path_from_bytes};
use crate::{schema::*, *};
use devlanes_core::{
    Actor, ActorKind, ExitOutcome, LaneId, ProcessIdentity, ProjectId, UnitRunId, WorktreeId,
};
use diesel::prelude::*;

#[derive(Queryable, Selectable)]
#[diesel(table_name = projects, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub common_dir: Option<Vec<u8>>,
    pub root_path: Vec<u8>,
    pub primary_branch: String,
    pub last_start: Option<i64>,
}
#[derive(Insertable, AsChangeset)]
#[diesel(table_name = projects, treat_none_as_null = true)]
pub(crate) struct NewProject {
    pub name: String,
    pub identity: Vec<u8>,
    pub common_dir: Option<Vec<u8>>,
    pub root_path: Vec<u8>,
    pub primary_branch: String,
}
#[derive(Queryable, Selectable)]
#[diesel(table_name = worktrees, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct WorktreeRow {
    pub id: i64,
    pub project_id: i64,
    pub identity: String,
    pub path: Vec<u8>,
    pub branch: Option<String>,
    pub present: i32,
    pub first_seen: i64,
    pub last_seen: i64,
}
#[derive(Insertable)]
#[diesel(table_name = worktrees)]
pub(crate) struct NewWorktree {
    pub project_id: i64,
    pub identity: String,
    pub path: Vec<u8>,
    pub branch: Option<String>,
    pub present: i32,
    pub first_seen: i64,
    pub last_seen: i64,
}
#[derive(Queryable, Selectable)]
#[diesel(table_name = lanes, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct LaneRow {
    pub id: i64,
    pub project_id: i64,
    pub worktree_id: i64,
    pub lane_index: i64,
    pub name: String,
    pub desired_state: String,
    pub profile: Option<String>,
    pub profiles: String,
    pub created_at: i64,
    pub cleaned_at: Option<i64>,
    pub services: String,
}
#[derive(Insertable)]
#[diesel(table_name = lanes)]
pub(crate) struct NewLane {
    pub project_id: i64,
    pub worktree_id: i64,
    pub lane_index: i64,
    pub name: String,
    pub desired_state: String,
    pub profile: Option<String>,
    pub profiles: String,
    pub created_at: i64,
}
#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = port_blocks, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct BlockRow {
    pub project_id: i64,
    pub lane_id: Option<i64>,
    pub start: i32,
    pub size: i32,
}
#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = port_offsets, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct OffsetRow {
    pub project_id: i64,
    pub unit: String,
    pub port_name: String,
    pub offset: i32,
    pub reserved_by_expression: i32,
    pub last_used: i64,
}
#[derive(Queryable, Selectable)]
#[diesel(table_name = unit_runs, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct UnitRunRow {
    pub id: i64,
    pub lane_id: i64,
    pub unit: String,
    pub pid: i64,
    pub sid: i64,
    pub process_start_time: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub actor_kind: String,
    pub actor_agent_id: Option<String>,
    pub actor_session_id: Option<String>,
    pub log_path: Option<Vec<u8>>,
    pub boot_id: Option<String>,
}
#[derive(Insertable)]
#[diesel(table_name = unit_runs)]
pub(crate) struct NewUnitRun {
    pub lane_id: i64,
    pub unit: String,
    pub pid: i64,
    pub sid: i64,
    pub process_start_time: String,
    pub started_at: i64,
    pub actor_kind: String,
    pub actor_agent_id: Option<String>,
    pub actor_session_id: Option<String>,
    pub log_path: Option<Vec<u8>>,
    pub boot_id: Option<String>,
}
#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = unit_run_members, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct MemberRow {
    pub unit_run_id: i64,
    pub pid: i64,
    pub sid: i64,
    pub process_start_time: String,
}
#[derive(Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = lane_setup, primary_key(lane_id, entry), treat_none_as_null = true, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct SetupRow {
    pub lane_id: i64,
    pub entry: String,
    pub done_at: i64,
    pub container_started_at: Option<String>,
}
#[derive(Queryable, Selectable)]
#[diesel(table_name = task_runs, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct TaskRow {
    pub id: i64,
    pub lane_id: i64,
    pub task: String,
    pub trigger: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub actor_kind: String,
    pub actor_agent_id: Option<String>,
    pub actor_session_id: Option<String>,
}
#[derive(Insertable)]
#[diesel(table_name = task_runs)]
pub(crate) struct NewTask {
    pub lane_id: i64,
    pub task: String,
    pub trigger: String,
    pub started_at: i64,
    pub actor_kind: String,
    pub actor_agent_id: Option<String>,
    pub actor_session_id: Option<String>,
}
#[derive(Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = log_segments, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct SegmentRow {
    pub unit_run_id: i64,
    pub first_seq: i64,
    pub last_seq: i64,
    pub min_ts: i64,
    pub max_ts: i64,
    pub path: Vec<u8>,
    pub compressed: i32,
    pub bytes: i64,
}
#[derive(Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = metric_rollups, primary_key(series, minute), check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct MetricRow {
    pub series: String,
    pub minute: i64,
    pub samples: i64,
    pub cpu_min: f64,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    pub mem_min: i64,
    pub mem_avg: i64,
    pub mem_max: i64,
    pub rss_max: i64,
}
#[derive(Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = run_summaries, primary_key(unit_run_id), check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct SummaryRow {
    pub unit_run_id: i64,
    pub series: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub samples: i64,
    pub mem_p50: i64,
    pub mem_p95: i64,
    pub mem_peak: i64,
    pub cpu_p50: f64,
    pub cpu_p95: f64,
    pub cpu_peak: f64,
    pub rss_peak: i64,
}
#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = startup_peaks, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct PeakRow {
    pub series: String,
    pub recorded_at: i64,
    pub peak: i64,
    pub duration_ms: i64,
}
#[derive(Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = remote_hosts, primary_key(name), treat_none_as_null = true, check_for_backend(diesel::sqlite::Sqlite))]
pub(crate) struct RemoteRow {
    pub name: String,
    pub transport: String,
    pub address: String,
    pub daemon_version: Option<String>,
    pub last_connected: Option<i64>,
}

impl From<ProjectRow> for Project {
    /// Restores paths without losing non-UTF-8 bytes.
    fn from(row: ProjectRow) -> Self {
        Self {
            id: ProjectId(row.id),
            name: row.name,
            common_dir: row.common_dir.map(path_from_bytes),
            root_path: path_from_bytes(row.root_path),
            primary_branch: row.primary_branch,
            last_start: row.last_start,
        }
    }
}
impl From<WorktreeRow> for Worktree {
    /// Restores stable worktree identity and observation timestamps.
    fn from(row: WorktreeRow) -> Self {
        Self {
            id: WorktreeId(row.id),
            project_id: ProjectId(row.project_id),
            identity: row.identity,
            path: path_from_bytes(row.path),
            branch: row.branch,
            present: row.present != 0,
            first_seen: row.first_seen,
            last_seen: row.last_seen,
        }
    }
}
impl TryFrom<LaneRow> for Lane {
    type Error = Error;
    /// Rejects corrupt states and indices rather than inventing defaults.
    fn try_from(row: LaneRow) -> Result<Self, Error> {
        let profiles: Vec<String> = serde_json::from_str(&row.profiles)
            .map_err(|_| Error::Corrupt(format!("lane {} profiles", row.id)))?;
        let services: Vec<String> = serde_json::from_str(&row.services)
            .map_err(|_| Error::Corrupt(format!("lane {} services", row.id)))?;
        if row.profile.as_ref() != profiles.first() {
            return Err(Error::Corrupt(format!(
                "lane {} profile does not match the first entry of its profiles",
                row.id
            )));
        }
        let desired_state = match row.desired_state.as_str() {
            "running" => DesiredState::Running,
            "stopped" => DesiredState::Stopped,
            _ => return Err(Error::Corrupt(format!("lane {} desired state", row.id))),
        };
        Ok(Self {
            id: LaneId(row.id),
            project_id: ProjectId(row.project_id),
            worktree_id: WorktreeId(row.worktree_id),
            index: checked(row.lane_index)?,
            name: row.name,
            desired_state,
            profile: row.profile,
            profiles,
            services,
            created_at: row.created_at,
            cleaned_at: row.cleaned_at,
        })
    }
}
impl TryFrom<UnitRunRow> for UnitRun {
    type Error = Error;
    /// Preserves full-width unsigned OS birth tokens stored as decimal text.
    fn try_from(row: UnitRunRow) -> Result<Self, Error> {
        Ok(Self {
            id: UnitRunId(row.id),
            lane_id: LaneId(row.lane_id),
            unit: row.unit,
            process: ProcessIdentity {
                pid: checked(row.pid)?,
                sid: checked(row.sid)?,
                start_time: birth_token(&row.process_start_time)?,
            },
            started_at: row.started_at,
            ended_at: row.ended_at,
            outcome: row.ended_at.map(|_| ExitOutcome {
                code: row.exit_code,
                signal: row.exit_signal,
            }),
            actor: actor(row.actor_kind, row.actor_agent_id, row.actor_session_id)?,
            log_path: row.log_path.map(path_from_bytes),
            boot_id: row.boot_id,
        })
    }
}
impl TryFrom<MemberRow> for ProcessIdentity {
    type Error = Error;
    /// Restores a persisted descendant identity.
    fn try_from(row: MemberRow) -> Result<Self, Error> {
        Ok(Self {
            pid: checked(row.pid)?,
            sid: checked(row.sid)?,
            start_time: birth_token(&row.process_start_time)?,
        })
    }
}
impl TryFrom<TaskRow> for TaskRun {
    type Error = Error;
    /// Validates the persisted client attribution.
    fn try_from(row: TaskRow) -> Result<Self, Error> {
        Ok(Self {
            id: TaskRunId(row.id),
            lane_id: LaneId(row.lane_id),
            task: row.task,
            trigger: row.trigger,
            started_at: row.started_at,
            ended_at: row.ended_at,
            exit_code: row.exit_code,
            actor: actor(row.actor_kind, row.actor_agent_id, row.actor_session_id)?,
        })
    }
}

/// Checks numeric projections instead of silently wrapping identifiers.
pub(crate) fn checked<T: TryFrom<i64>>(value: i64) -> Result<T, Error> {
    T::try_from(value).map_err(|_| Error::Corrupt(format!("integer {value} out of range")))
}
/// Full-width unsigned OS birth tokens are stored as decimal text.
fn birth_token(text: &str) -> Result<u64, Error> {
    text.parse()
        .map_err(|_| Error::Corrupt(format!("process birth token {text:?}")))
}
/// Saturates counters to SQLite's signed integer range, preserving the existing API.
pub(crate) fn sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
/// Valid counters are nonnegative; legacy corrupt values read as zero.
pub(crate) fn from_sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
/// Stable actor representation used by inserts.
pub(crate) fn actor_kind(kind: ActorKind) -> String {
    match kind {
        ActorKind::Human => "human",
        ActorKind::Agent => "agent",
    }
    .into()
}
/// Rejects unknown actor kinds during projection.
fn actor(
    kind: String,
    agent_id: Option<String>,
    session_id: Option<String>,
) -> Result<Actor, Error> {
    let kind = match kind.as_str() {
        "human" => ActorKind::Human,
        "agent" => ActorKind::Agent,
        _ => return Err(Error::Corrupt(format!("actor kind {kind:?}"))),
    };
    Ok(Actor {
        kind,
        agent_id,
        session_id,
    })
}
