//! Lifecycle records added after the registry: setup completion, task history, log
//! segments, metric rollups, run summaries and remote hosts. None of them store an
//! environment, a command line or a secret.

use crate::{Error, Store, queries::now};
use devlanes_core::{Actor, ActorKind, LaneId, ProjectId, UnitRunId};
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

/// Persistent task-run identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskRunId(pub i64);

/// One `infra.setup` entry that succeeded for a lane. `container_started_at` is the
/// backing container's `StartedAt` when the entry names a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRecord {
    pub lane_id: LaneId,
    pub entry: String,
    pub done_at: i64,
    pub container_started_at: Option<String>,
}

/// Start of a task run. `trigger` is a hook name (`lane.create`) or `manual`.
#[derive(Debug, Clone)]
pub struct TaskRunInput {
    pub lane_id: LaneId,
    pub task: String,
    pub trigger: String,
    pub actor: Actor,
}

/// Task history row; `exit_code` is `None` for killed or still-running tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: TaskRunId,
    pub lane_id: LaneId,
    pub task: String,
    pub trigger: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub actor: Actor,
}

/// A rotated log segment of one unit run; bodies stay in files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSegmentInput {
    pub unit_run_id: UnitRunId,
    pub first_seq: u64,
    pub last_seq: u64,
    pub min_ts_ms: u64,
    pub max_ts_ms: u64,
    pub path: PathBuf,
    pub compressed: bool,
    pub bytes: u64,
}

/// One closed minute of one metrics series (`devlanes_metrics::Subject::key`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollupRow {
    pub series: String,
    pub minute_ms: u64,
    pub samples: u32,
    pub cpu_min: f64,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    pub mem_min: u64,
    pub mem_avg: u64,
    pub mem_max: u64,
    pub rss_max: u64,
}

/// Resource profile of a finished unit run, for baselines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummaryRow {
    pub unit_run_id: UnitRunId,
    pub series: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub samples: u64,
    pub mem_p50: u64,
    pub mem_p95: u64,
    pub mem_peak: u64,
    pub cpu_p50: f64,
    pub cpu_p95: f64,
    pub cpu_peak: f64,
    pub rss_peak: u64,
}

/// A box this machine connected to (`devlanes connect`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    pub name: String,
    pub transport: String,
    pub address: String,
    pub daemon_version: Option<String>,
    pub last_connected: Option<i64>,
}

/// Rows removed by a retention pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    pub unit_runs: usize,
    pub task_runs: usize,
    pub rollups: usize,
}

impl Store {
    /// Remembers where `devlanes.yaml` lives relative to the worktree root, so a
    /// restarted daemon can reload a monorepo project without a client cwd.
    pub async fn set_project_config_dir(
        &self,
        project: ProjectId,
        config_dir: PathBuf,
    ) -> Result<(), Error> {
        self.call(move |connection| {
            crate::queries::require_changed(
                connection.execute(
                    "UPDATE projects SET config_dir=?1 WHERE id=?2",
                    params![config_dir.as_os_str().as_bytes(), project.0],
                )?,
                "project",
            )
        })
        .await
    }

    /// Returns the recorded config directory of every project that has one.
    pub async fn project_config_dirs(&self) -> Result<Vec<(ProjectId, PathBuf)>, Error> {
        self.call(|connection| {
            Ok(connection
                .prepare("SELECT id, config_dir FROM projects WHERE config_dir IS NOT NULL ORDER BY id")?
                .query_map([], |row| {
                    Ok((
                        ProjectId(row.get(0)?),
                        PathBuf::from(OsString::from_vec(row.get(1)?)),
                    ))
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Returns every completed setup entry of a lane, in entry order.
    pub async fn setup_records(&self, lane: LaneId) -> Result<Vec<SetupRecord>, Error> {
        self.call(move |connection| {
            Ok(connection
                .prepare("SELECT lane_id, entry, done_at, container_started_at FROM lane_setup WHERE lane_id=?1 ORDER BY entry")?
                .query_map([lane.0], setup_row)?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Marks a setup entry done, replacing an earlier record and its container timestamp.
    pub async fn record_setup(
        &self,
        lane: LaneId,
        entry: String,
        container_started_at: Option<String>,
    ) -> Result<(), Error> {
        if entry.is_empty() {
            return Err(Error::Invalid("empty setup entry".into()));
        }
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO lane_setup(lane_id, entry, done_at, container_started_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(lane_id, entry) DO UPDATE SET done_at=excluded.done_at, container_started_at=excluded.container_started_at",
                params![lane.0, entry, now()?, container_started_at],
            )?;
            Ok(())
        })
        .await
    }

    /// Forgets one entry (`Some`) or every entry (`None`) of a lane; teardown calls this.
    pub async fn clear_setup(&self, lane: LaneId, entry: Option<String>) -> Result<usize, Error> {
        self.call(move |connection| {
            Ok(connection.execute(
                "DELETE FROM lane_setup WHERE lane_id=?1 AND (?2 IS NULL OR entry=?2)",
                params![lane.0, entry],
            )?)
        })
        .await
    }

    /// Records the start of a task run with the client-supplied actor.
    pub async fn start_task_run(&self, input: TaskRunInput) -> Result<TaskRun, Error> {
        if input.task.is_empty() || input.trigger.is_empty() {
            return Err(Error::Invalid("task and trigger must be nonempty".into()));
        }
        self.call(move |connection| {
            connection.query_row(
                "INSERT INTO task_runs(lane_id, task, trigger, started_at, actor_kind, actor_agent_id, actor_session_id) VALUES (?1,?2,?3,?4,?5,?6,?7)
                 RETURNING id, lane_id, task, trigger, started_at, ended_at, exit_code, actor_kind, actor_agent_id, actor_session_id",
                params![input.lane_id.0, input.task, input.trigger, now()?, actor_kind(input.actor.kind), input.actor.agent_id, input.actor.session_id],
                task_row,
            ).map_err(Error::from)
        })
        .await
    }

    /// Completes a task run once; a second call keeps the first result.
    pub async fn finish_task_run(&self, id: TaskRunId, exit_code: Option<i32>) -> Result<(), Error> {
        self.call(move |connection| {
            connection.execute(
                "UPDATE task_runs SET ended_at=?1, exit_code=?2 WHERE id=?3 AND ended_at IS NULL",
                params![now()?, exit_code, id.0],
            )?;
            Ok(())
        })
        .await
    }

    /// True once the task exited 0 in this lane; `lane.create` hooks stop running then.
    pub async fn task_succeeded(&self, lane: LaneId, task: String) -> Result<bool, Error> {
        self.call(move |connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM task_runs WHERE lane_id=?1 AND task=?2 AND exit_code=0)",
                params![lane.0, task],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Returns a lane's task history, newest first.
    pub async fn task_runs(&self, lane: LaneId, limit: u32) -> Result<Vec<TaskRun>, Error> {
        self.call(move |connection| {
            Ok(connection
                .prepare("SELECT id, lane_id, task, trigger, started_at, ended_at, exit_code, actor_kind, actor_agent_id, actor_session_id FROM task_runs WHERE lane_id=?1 ORDER BY id DESC LIMIT ?2")?
                .query_map(params![lane.0, limit], task_row)?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Drops a cleaned lane's setup and task records so a re-created lane starts fresh.
    /// Unit run history stays until retention removes it.
    pub async fn forget_lane_lifecycle(&self, lane: LaneId) -> Result<(), Error> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute("DELETE FROM lane_setup WHERE lane_id=?1", [lane.0])?;
            transaction.execute("DELETE FROM task_runs WHERE lane_id=?1", [lane.0])?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Records or updates a rotated segment's index metadata.
    pub async fn record_log_segment(&self, input: LogSegmentInput) -> Result<(), Error> {
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO log_segments(unit_run_id, first_seq, last_seq, min_ts, max_ts, path, compressed, bytes) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(unit_run_id, path) DO UPDATE SET first_seq=excluded.first_seq, last_seq=excluded.last_seq, min_ts=excluded.min_ts, max_ts=excluded.max_ts, compressed=excluded.compressed, bytes=excluded.bytes",
                params![input.unit_run_id.0, sql_u64(input.first_seq), sql_u64(input.last_seq), sql_u64(input.min_ts_ms), sql_u64(input.max_ts_ms), input.path.as_os_str().as_bytes(), input.compressed, sql_u64(input.bytes)],
            )?;
            Ok(())
        })
        .await
    }

    /// Lists a run's segments in sequence order.
    pub async fn log_segments(&self, run: UnitRunId) -> Result<Vec<LogSegmentInput>, Error> {
        self.call(move |connection| {
            Ok(connection
                .prepare("SELECT unit_run_id, first_seq, last_seq, min_ts, max_ts, path, compressed, bytes FROM log_segments WHERE unit_run_id=?1 ORDER BY first_seq")?
                .query_map([run.0], |row| {
                    Ok(LogSegmentInput {
                        unit_run_id: UnitRunId(row.get(0)?),
                        first_seq: from_sql_u64(row.get(1)?),
                        last_seq: from_sql_u64(row.get(2)?),
                        min_ts_ms: from_sql_u64(row.get(3)?),
                        max_ts_ms: from_sql_u64(row.get(4)?),
                        path: OsString::from_vec(row.get(5)?).into(),
                        compressed: row.get(6)?,
                        bytes: from_sql_u64(row.get(7)?),
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Stores closed minutes; a repeated minute replaces the earlier row.
    pub async fn insert_rollups(&self, rows: Vec<RollupRow>) -> Result<(), Error> {
        if rows.is_empty() {
            return Ok(());
        }
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            {
                let mut statement = transaction.prepare(
                    "INSERT OR REPLACE INTO metric_rollups(series, minute, samples, cpu_min, cpu_avg, cpu_max, mem_min, mem_avg, mem_max, rss_max) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                )?;
                for row in &rows {
                    statement.execute(params![
                        row.series,
                        sql_u64(row.minute_ms),
                        row.samples,
                        row.cpu_min,
                        row.cpu_avg,
                        row.cpu_max,
                        sql_u64(row.mem_min),
                        sql_u64(row.mem_avg),
                        sql_u64(row.mem_max),
                        sql_u64(row.rss_max)
                    ])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Returns one series' minutes at or after `since_ms`, oldest first.
    pub async fn rollups(&self, series: String, since_ms: u64) -> Result<Vec<RollupRow>, Error> {
        self.call(move |connection| {
            Ok(connection
                .prepare("SELECT series, minute, samples, cpu_min, cpu_avg, cpu_max, mem_min, mem_avg, mem_max, rss_max FROM metric_rollups WHERE series=?1 AND minute>=?2 ORDER BY minute")?
                .query_map(params![series, sql_u64(since_ms)], |row| {
                    Ok(RollupRow {
                        series: row.get(0)?,
                        minute_ms: from_sql_u64(row.get(1)?),
                        samples: row.get(2)?,
                        cpu_min: row.get(3)?,
                        cpu_avg: row.get(4)?,
                        cpu_max: row.get(5)?,
                        mem_min: from_sql_u64(row.get(6)?),
                        mem_avg: from_sql_u64(row.get(7)?),
                        mem_max: from_sql_u64(row.get(8)?),
                        rss_max: from_sql_u64(row.get(9)?),
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Stores the final profile of a unit run.
    pub async fn record_run_summary(&self, row: RunSummaryRow) -> Result<(), Error> {
        self.call(move |connection| {
            connection.execute(
                "INSERT OR REPLACE INTO run_summaries(unit_run_id, series, started_at, ended_at, samples, mem_p50, mem_p95, mem_peak, cpu_p50, cpu_p95, cpu_peak, rss_peak) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![row.unit_run_id.0, row.series, sql_u64(row.started_ms), sql_u64(row.ended_ms), sql_u64(row.samples), sql_u64(row.mem_p50), sql_u64(row.mem_p95), sql_u64(row.mem_peak), row.cpu_p50, row.cpu_p95, row.cpu_peak, sql_u64(row.rss_peak)],
            )?;
            Ok(())
        })
        .await
    }

    /// Recent run profiles of a series, newest first, for baselines.
    pub async fn run_summaries(&self, series: String, limit: u32) -> Result<Vec<RunSummaryRow>, Error> {
        self.call(move |connection| {
            Ok(connection
                .prepare("SELECT unit_run_id, series, started_at, ended_at, samples, mem_p50, mem_p95, mem_peak, cpu_p50, cpu_p95, cpu_peak, rss_peak FROM run_summaries WHERE series=?1 ORDER BY ended_at DESC LIMIT ?2")?
                .query_map(params![series, limit], |row| {
                    Ok(RunSummaryRow {
                        unit_run_id: UnitRunId(row.get(0)?),
                        series: row.get(1)?,
                        started_ms: from_sql_u64(row.get(2)?),
                        ended_ms: from_sql_u64(row.get(3)?),
                        samples: from_sql_u64(row.get(4)?),
                        mem_p50: from_sql_u64(row.get(5)?),
                        mem_p95: from_sql_u64(row.get(6)?),
                        mem_peak: from_sql_u64(row.get(7)?),
                        cpu_p50: row.get(8)?,
                        cpu_p95: row.get(9)?,
                        cpu_peak: row.get(10)?,
                        rss_peak: from_sql_u64(row.get(11)?),
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Inserts or refreshes a remote host after a successful connect.
    pub async fn upsert_remote_host(&self, host: RemoteHost) -> Result<(), Error> {
        if host.name.is_empty() {
            return Err(Error::Invalid("empty remote host name".into()));
        }
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO remote_hosts(name, transport, address, daemon_version, last_connected) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(name) DO UPDATE SET transport=excluded.transport, address=excluded.address, daemon_version=excluded.daemon_version, last_connected=excluded.last_connected",
                params![host.name, host.transport, host.address, host.daemon_version, host.last_connected],
            )?;
            Ok(())
        })
        .await
    }

    /// Lists remote hosts by name.
    pub async fn remote_hosts(&self) -> Result<Vec<RemoteHost>, Error> {
        self.call(|connection| {
            Ok(connection
                .prepare("SELECT name, transport, address, daemon_version, last_connected FROM remote_hosts ORDER BY name")?
                .query_map([], |row| {
                    Ok(RemoteHost {
                        name: row.get(0)?,
                        transport: row.get(1)?,
                        address: row.get(2)?,
                        daemon_version: row.get(3)?,
                        last_connected: row.get(4)?,
                    })
                })?
                .collect::<Result<_, _>>()?)
        })
        .await
    }

    /// Applies history retention: finished unit and task runs ended before
    /// `runs_before_ms`, rollups older than `rollups_before_ms`. Unfinished runs stay,
    /// because reconciliation still needs their identities.
    pub async fn prune_history(
        &self,
        runs_before_ms: i64,
        rollups_before_ms: i64,
    ) -> Result<Pruned, Error> {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let unit_runs = transaction.execute(
                "DELETE FROM unit_runs WHERE ended_at IS NOT NULL AND ended_at < ?1",
                [runs_before_ms],
            )?;
            let task_runs = transaction.execute(
                "DELETE FROM task_runs WHERE ended_at IS NOT NULL AND ended_at < ?1",
                [runs_before_ms],
            )?;
            let rollups = transaction.execute(
                "DELETE FROM metric_rollups WHERE minute < ?1",
                [rollups_before_ms],
            )?;
            transaction.commit()?;
            Ok(Pruned {
                unit_runs,
                task_runs,
                rollups,
            })
        })
        .await
    }

    /// Deletes every row (`devlanes daemon reset-state`). The caller stops all
    /// resources first; lane numbers and ports are reassigned afterwards.
    pub async fn reset(&self) -> Result<(), Error> {
        self.call(|connection| {
            let transaction = connection.transaction()?;
            // Children first keeps this independent of cascade configuration.
            for table in [
                "log_segments",
                "run_summaries",
                "metric_rollups",
                "remote_hosts",
                "lane_setup",
                "task_runs",
                "unit_runs",
                "port_offsets",
                "port_blocks",
                "lanes",
                "worktrees",
                "projects",
            ] {
                transaction.execute(&format!("DELETE FROM {table}"), [])?;
            }
            // AUTOINCREMENT tables guarantee that sqlite_sequence exists.
            transaction.execute("DELETE FROM sqlite_sequence", [])?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }
}

/// Stable database word for an actor kind.
fn actor_kind(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Human => "human",
        ActorKind::Agent => "agent",
    }
}

/// Decodes a setup projection.
fn setup_row(row: &Row<'_>) -> rusqlite::Result<SetupRecord> {
    Ok(SetupRecord {
        lane_id: LaneId(row.get(0)?),
        entry: row.get(1)?,
        done_at: row.get(2)?,
        container_started_at: row.get(3)?,
    })
}

/// Decodes a task-run projection; unknown actor words are treated as corruption.
fn task_row(row: &Row<'_>) -> rusqlite::Result<TaskRun> {
    let kind = match row.get::<_, String>(7)?.as_str() {
        "human" => ActorKind::Human,
        "agent" => ActorKind::Agent,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(TaskRun {
        id: TaskRunId(row.get(0)?),
        lane_id: LaneId(row.get(1)?),
        task: row.get(2)?,
        trigger: row.get(3)?,
        started_at: row.get(4)?,
        ended_at: row.get(5)?,
        exit_code: row.get(6)?,
        actor: Actor {
            kind,
            agent_id: row.get(8)?,
            session_id: row.get(9)?,
        },
    })
}

/// SQLite integers are signed; saturating keeps huge counters representable.
fn sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Negative values only appear after corruption; they read back as zero.
fn from_sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
