//! Typed registry and reconciliation queries, executed only by the store actor.

use crate::{
    DesiredState, Error, Lane, LaneInput, Project, ProjectInput, Store, UnitRun, UnitRunInput,
    Worktree, WorktreeInput,
};
use devlanes_core::{
    Actor, ActorKind, ExitOutcome, LaneId, ProcessIdentity, ProjectId, UnitRunId, WorktreeId,
    refs::valid_project_name,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

impl Store {
    /// Inserts by canonical identity, or updates metadata without changing its ID.
    pub async fn upsert_project(&self, input: ProjectInput) -> Result<Project, Error> {
        if !valid_project_name(&input.name) {
            return Err(Error::Invalid("invalid project name".into()));
        }
        self.call(move |connection| {
            let identity = path_bytes(input.common_dir.as_deref().unwrap_or(&input.root_path));
            connection.query_row(
                "INSERT INTO projects(name, identity, common_dir, root_path, primary_branch) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(identity) DO UPDATE SET name=excluded.name, common_dir=excluded.common_dir, root_path=excluded.root_path, primary_branch=excluded.primary_branch
                 RETURNING id, name, common_dir, root_path, primary_branch, last_start",
                params![input.name, identity, input.common_dir.as_deref().map(path_bytes), path_bytes(&input.root_path), input.primary_branch], project_row,
            ).map_err(Error::from)
        }).await
    }

    /// Looks up a project by persistent ID.
    pub async fn project(&self, id: ProjectId) -> Result<Option<Project>, Error> {
        self.call(move |connection| Ok(connection.query_row(
            "SELECT id, name, common_dir, root_path, primary_branch, last_start FROM projects WHERE id=?1", [id.0], project_row,
        ).optional()?)).await
    }

    /// Lists projects in ID order, including duplicate display names.
    pub async fn projects(&self) -> Result<Vec<Project>, Error> {
        self.call(|connection| Ok(connection.prepare(
            "SELECT id, name, common_dir, root_path, primary_branch, last_start FROM projects ORDER BY id"
        )?.query_map([], project_row)?.collect::<Result<_, _>>()?)).await
    }

    /// Records a shell-driven start for project history, without storing its env.
    pub async fn mark_project_started(&self, id: ProjectId) -> Result<(), Error> {
        self.call(move |connection| {
            require_changed(
                connection.execute(
                    "UPDATE projects SET last_start=?1 WHERE id=?2",
                    params![now()?, id.0],
                )?,
                "project",
            )
        })
        .await
    }

    /// Explicitly removes a project and its registry/allocation/history records.
    /// Callers must stop resources and obtain any required user confirmation first.
    pub async fn delete_project(&self, id: ProjectId) -> Result<(), Error> {
        self.call(move |connection| {
            require_changed(
                connection.execute("DELETE FROM projects WHERE id=?1", [id.0])?,
                "project",
            )
        })
        .await
    }

    /// Updates a worktree by administrative identity; moves retain the same ID.
    pub async fn upsert_worktree(&self, input: WorktreeInput) -> Result<Worktree, Error> {
        if input.identity.is_empty() {
            return Err(Error::Invalid("empty worktree identity".into()));
        }
        self.call(move |connection| Ok(connection.query_row(
            "INSERT INTO worktrees(project_id, identity, path, branch, present, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(project_id, identity) DO UPDATE SET path=excluded.path, branch=excluded.branch, present=excluded.present, last_seen=excluded.last_seen
             RETURNING id, project_id, identity, path, branch, present, first_seen, last_seen",
            params![input.project_id.0, input.identity, path_bytes(&input.path), input.branch, input.present, now()?], worktree_row,
        )?)).await
    }

    /// Returns all known worktrees, retaining missing ones until explicit cleanup.
    pub async fn worktrees(&self, project: ProjectId) -> Result<Vec<Worktree>, Error> {
        self.call(move |connection| Ok(connection.prepare(
            "SELECT id, project_id, identity, path, branch, present, first_seen, last_seen FROM worktrees WHERE project_id=?1 ORDER BY id"
        )?.query_map([project.0], worktree_row)?.collect::<Result<_, _>>()?)).await
    }

    /// Finds a worktree by persistent identity.
    pub async fn worktree(&self, id: WorktreeId) -> Result<Option<Worktree>, Error> {
        self.call(move |connection| Ok(connection.query_row(
            "SELECT id, project_id, identity, path, branch, present, first_seen, last_seen FROM worktrees WHERE id=?1", [id.0], worktree_row,
        ).optional()?)).await
    }

    /// Marks absence without implicitly deleting a lane, data, or allocation.
    pub async fn mark_worktree_missing(&self, id: WorktreeId) -> Result<(), Error> {
        self.call(move |connection| {
            require_changed(
                connection.execute(
                    "UPDATE worktrees SET present=0, last_seen=?1 WHERE id=?2",
                    params![now()?, id.0],
                )?,
                "worktree",
            )
        })
        .await
    }

    /// Registers a lane or changes its index on an explicit next-start decision.
    /// Conflicting active indices and cross-project worktrees are rejected by SQL.
    pub async fn upsert_lane(&self, input: LaneInput) -> Result<Lane, Error> {
        if input.name.is_empty() {
            return Err(Error::Invalid("empty lane name".into()));
        }
        self.call(move |connection| {
            let matches: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM worktrees WHERE id=?1 AND project_id=?2)", params![input.worktree_id.0, input.project_id.0], |row| row.get(0))?;
            if !matches { return Err(Error::NotFound("project worktree")); }
            Ok(connection.query_row(
            "INSERT INTO lanes(project_id, worktree_id, lane_index, name, desired_state, profile, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(worktree_id) DO UPDATE SET lane_index=excluded.lane_index, name=excluded.name, desired_state=excluded.desired_state, profile=excluded.profile, cleaned_at=NULL
             RETURNING id, project_id, worktree_id, lane_index, name, desired_state, profile, created_at, cleaned_at",
            params![input.project_id.0, input.worktree_id.0, input.index, input.name, input.desired_state.as_str(), input.profile, now()?], lane_row,
        )?) }).await
    }

    /// Returns a lane, including cleaned history records.
    pub async fn lane(&self, id: LaneId) -> Result<Option<Lane>, Error> {
        self.call(move |connection| Ok(connection.query_row(
            "SELECT id, project_id, worktree_id, lane_index, name, desired_state, profile, created_at, cleaned_at FROM lanes WHERE id=?1", [id.0], lane_row,
        ).optional()?)).await
    }

    /// Lists lanes in project/index order. `None` selects the whole machine.
    pub async fn lanes(&self, project: Option<ProjectId>) -> Result<Vec<Lane>, Error> {
        self.call(move |connection| Ok(connection.prepare(
            "SELECT id, project_id, worktree_id, lane_index, name, desired_state, profile, created_at, cleaned_at FROM lanes WHERE (?1 IS NULL OR project_id=?1) ORDER BY project_id, lane_index, id"
        )?.query_map([project.map(|id| id.0)], lane_row)?.collect::<Result<_, _>>()?)).await
    }

    /// Changes resume intent/profile without touching live processes.
    pub async fn set_lane_desired(
        &self,
        id: LaneId,
        desired: DesiredState,
        profile: Option<String>,
    ) -> Result<(), Error> {
        self.call(move |connection| require_changed(connection.execute(
            "UPDATE lanes SET desired_state=?1, profile=?2 WHERE id=?3 AND cleaned_at IS NULL", params![desired.as_str(), profile, id.0],
        )?, "active lane")).await
    }

    /// Releases a lane's block and active index while retaining run history.
    /// Call only after clean phases finish and the lane has no live resources.
    pub async fn clean_lane(&self, id: LaneId) -> Result<(), Error> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            require_no_active_runs(&transaction, id)?;
            require_changed(
                transaction.execute(
                    "UPDATE lanes SET cleaned_at=?1, desired_state='stopped' WHERE id=?2",
                    params![now()?, id.0],
                )?,
                "lane",
            )?;
            transaction.execute("DELETE FROM port_blocks WHERE lane_id=?1", [id.0])?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Explicitly deletes lane metadata and its history after resources are stopped.
    pub async fn delete_lane(&self, id: LaneId) -> Result<(), Error> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            require_no_active_runs(&transaction, id)?;
            require_changed(
                transaction.execute("DELETE FROM lanes WHERE id=?1", [id.0])?,
                "lane",
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Records a verified leader immediately after spawn. Active unit names are
    /// unique within a lane; process birth tokens retain their full unsigned width.
    pub async fn record_unit_run(&self, input: UnitRunInput) -> Result<UnitRun, Error> {
        if input.unit.is_empty() || input.process.pid == 0 || input.process.sid == 0 {
            return Err(Error::Invalid(
                "unit name, PID and SID must be nonempty/nonzero".into(),
            ));
        }
        self.call(move |connection| {
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let active: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM lanes WHERE id=?1 AND cleaned_at IS NULL)", [input.lane_id.0], |row| row.get(0))?;
            if !active { return Err(Error::NotFound("active lane")); }
            let actor_kind = match input.actor.kind { ActorKind::Human => "human", ActorKind::Agent => "agent" };
            transaction.execute(
                "INSERT INTO unit_runs(lane_id, unit, pid, sid, process_start_time, started_at, actor_kind, actor_agent_id, actor_session_id, log_path) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![input.lane_id.0, input.unit, input.process.pid, input.process.sid, input.process.start_time.to_string(), now()?, actor_kind, input.actor.agent_id, input.actor.session_id, input.log_path.as_deref().map(path_bytes)],
            )?;
            let run = transaction.query_row("SELECT id, lane_id, unit, pid, sid, process_start_time, started_at, ended_at, exit_code, exit_signal, actor_kind, actor_agent_id, actor_session_id, log_path FROM unit_runs WHERE id=?1", [transaction.last_insert_rowid()], run_row)?;
            transaction.commit()?;
            Ok(run)
        }).await
    }

    /// Completes a run once; repeated calls preserve the original result/time.
    pub async fn finish_unit_run(&self, id: UnitRunId, outcome: ExitOutcome) -> Result<(), Error> {
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE unit_runs SET ended_at=?1, exit_code=?2, exit_signal=?3 WHERE id=?4 AND ended_at IS NULL", params![now()?, outcome.code, outcome.signal, id.0],
            )?;
            if changed == 0 && !connection.query_row("SELECT EXISTS(SELECT 1 FROM unit_runs WHERE id=?1)", [id.0], |row| row.get::<_, bool>(0))? {
                return Err(Error::NotFound("unit run"));
            }
            Ok(())
        }).await
    }

    /// Lists unfinished runs for identity-checked reconciliation, never as proof
    /// that a recorded PID still belongs to devlanes.
    pub async fn active_unit_runs(&self) -> Result<Vec<UnitRun>, Error> {
        self.call(|connection| Ok(connection.prepare(
            "SELECT id, lane_id, unit, pid, sid, process_start_time, started_at, ended_at, exit_code, exit_signal, actor_kind, actor_agent_id, actor_session_id, log_path FROM unit_runs WHERE ended_at IS NULL ORDER BY id"
        )?.query_map([], run_row)?.collect::<Result<_, _>>()?)).await
    }

    /// Returns a lane's run history, newest first, under a caller-supplied limit.
    pub async fn unit_runs(&self, lane: LaneId, limit: u32) -> Result<Vec<UnitRun>, Error> {
        self.call(move |connection| Ok(connection.prepare(
            "SELECT id, lane_id, unit, pid, sid, process_start_time, started_at, ended_at, exit_code, exit_signal, actor_kind, actor_agent_id, actor_session_id, log_path FROM unit_runs WHERE lane_id=?1 ORDER BY id DESC LIMIT ?2"
        )?.query_map(params![lane.0, limit], run_row)?.collect::<Result<_, _>>()?)).await
    }
}

/// Rejects metadata cleanup while reconciliation still has unfinished identities.
fn require_no_active_runs(connection: &Connection, lane: LaneId) -> Result<(), Error> {
    if connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM unit_runs WHERE lane_id=?1 AND ended_at IS NULL)",
        [lane.0],
        |row| row.get::<_, bool>(0),
    )? {
        return Err(Error::Invalid("lane has unfinished unit runs".into()));
    }
    Ok(())
}

/// Converts missing updates into a typed not-found error.
pub(crate) fn require_changed(changed: usize, record: &'static str) -> Result<(), Error> {
    if changed == 0 {
        Err(Error::NotFound(record))
    } else {
        Ok(())
    }
}

/// Uses Unix milliseconds for persisted audit timestamps.
pub(crate) fn now() -> Result<i64, Error> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Invalid(error.to_string()))?
        .as_millis();
    i64::try_from(millis).map_err(|error| Error::Invalid(error.to_string()))
}

/// Stores Unix paths losslessly, including non-UTF-8 worktree directory names.
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

/// Restores the exact bytes originally supplied by Git or the filesystem.
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    OsString::from_vec(bytes).into()
}

/// Decodes a project projection shared by insert and query paths.
fn project_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: ProjectId(row.get(0)?),
        name: row.get(1)?,
        common_dir: row.get::<_, Option<Vec<u8>>>(2)?.map(path_from_bytes),
        root_path: path_from_bytes(row.get(3)?),
        primary_branch: row.get(4)?,
        last_start: row.get(5)?,
    })
}

/// Decodes a worktree projection without lossy path conversion.
fn worktree_row(row: &Row<'_>) -> rusqlite::Result<Worktree> {
    Ok(Worktree {
        id: WorktreeId(row.get(0)?),
        project_id: ProjectId(row.get(1)?),
        identity: row.get(2)?,
        path: path_from_bytes(row.get(3)?),
        branch: row.get(4)?,
        present: row.get(5)?,
        first_seen: row.get(6)?,
        last_seen: row.get(7)?,
    })
}

/// Decodes desired state strictly so database corruption is not treated as stopped.
fn lane_row(row: &Row<'_>) -> rusqlite::Result<Lane> {
    let desired: String = row.get(5)?;
    let desired_state = match desired.as_str() {
        "running" => DesiredState::Running,
        "stopped" => DesiredState::Stopped,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(Lane {
        id: LaneId(row.get(0)?),
        project_id: ProjectId(row.get(1)?),
        worktree_id: WorktreeId(row.get(2)?),
        index: row.get(3)?,
        name: row.get(4)?,
        desired_state,
        profile: row.get(6)?,
        created_at: row.get(7)?,
        cleaned_at: row.get(8)?,
    })
}

/// Decodes an unsigned process birth token stored as decimal text to avoid SQLite's
/// signed-integer ceiling on platform-native identity values.
fn run_row(row: &Row<'_>) -> rusqlite::Result<UnitRun> {
    let start_time: String = row.get(5)?;
    let start_time = start_time.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let actor: String = row.get(10)?;
    let kind = match actor.as_str() {
        "human" => ActorKind::Human,
        "agent" => ActorKind::Agent,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let ended_at: Option<i64> = row.get(7)?;
    Ok(UnitRun {
        id: UnitRunId(row.get(0)?),
        lane_id: LaneId(row.get(1)?),
        unit: row.get(2)?,
        process: ProcessIdentity {
            pid: row.get(3)?,
            sid: row.get(4)?,
            start_time,
        },
        started_at: row.get(6)?,
        ended_at,
        outcome: ended_at
            .map(|_| {
                Ok::<_, rusqlite::Error>(ExitOutcome {
                    code: row.get(8)?,
                    signal: row.get(9)?,
                })
            })
            .transpose()?,
        actor: Actor {
            kind,
            agent_id: row.get(11)?,
            session_id: row.get(12)?,
        },
        log_path: row.get::<_, Option<Vec<u8>>>(13)?.map(path_from_bytes),
    })
}
