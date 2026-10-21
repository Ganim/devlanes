//! Setup completion and historical data. Environment values never enter this API.

use crate::{
    Error, Store,
    models::*,
    queries::{now, require_changed},
    schema::*,
};
use devlanes_core::{Actor, LaneId, ProjectId, UnitRunId};
use diesel::{dsl::exists, prelude::*};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persistent task-run identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskRunId(pub i64);

/// Completed setup entry and the backing container's start timestamp, if applicable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRecord {
    pub lane_id: LaneId,
    pub entry: String,
    pub done_at: i64,
    pub container_started_at: Option<String>,
}

/// Task invocation with a hook name or `manual` trigger and client attribution.
#[derive(Debug, Clone)]
pub struct TaskRunInput {
    pub lane_id: LaneId,
    pub task: String,
    pub trigger: String,
    pub actor: Actor,
}

/// Task history. An absent end time marks an unfinished task.
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

/// Rotated log metadata; log bodies remain in files.
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

/// One closed minute of a metrics series.
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

/// Resource profile of a finished run, used for baselines.
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

/// A box this machine connected to (`devlanes remote connect`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    pub name: String,
    pub transport: String,
    pub address: String,
    pub daemon_version: Option<String>,
    pub last_connected: Option<i64>,
}

/// Compatibility result for the original cutoff-based history API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    pub unit_runs: usize,
    pub task_runs: usize,
    pub rollups: usize,
}

impl Store {
    /// Records the configuration directory relative to the worktree root.
    pub async fn set_project_config_dir(
        &self,
        project: ProjectId,
        config_dir: PathBuf,
    ) -> Result<(), Error> {
        self.call(move |c| {
            require_changed(
                diesel::update(projects::table.find(project.0))
                    .set(projects::config_dir.eq(path_bytes(&config_dir)))
                    .execute(c)?,
                "project",
            )
        })
        .await
    }
    /// Lists project configuration locations without decoding YAML.
    pub async fn project_config_dirs(&self) -> Result<Vec<(ProjectId, PathBuf)>, Error> {
        self.call(|c| {
            Ok(projects::table
                .filter(projects::config_dir.is_not_null())
                .order(projects::id)
                .select((projects::id, projects::config_dir))
                .load::<(i64, Option<Vec<u8>>)>(c)?
                .into_iter()
                .filter_map(|(id, path)| path.map(|path| (ProjectId(id), path_from_bytes(path))))
                .collect())
        })
        .await
    }
    /// Lists completed setup entries in declaration-independent name order.
    pub async fn setup_records(&self, lane: LaneId) -> Result<Vec<SetupRecord>, Error> {
        self.call(move |c| {
            Ok(lane_setup::table
                .filter(lane_setup::lane_id.eq(lane.0))
                .order(lane_setup::entry)
                .select(SetupRow::as_select())
                .load::<SetupRow>(c)?
                .into_iter()
                .map(|row| SetupRecord {
                    lane_id: LaneId(row.lane_id),
                    entry: row.entry,
                    done_at: row.done_at,
                    container_started_at: row.container_started_at,
                })
                .collect())
        })
        .await
    }
    /// Marks an entry done, replacing an earlier container timestamp, including NULL.
    pub async fn record_setup(
        &self,
        lane: LaneId,
        entry: String,
        container_started_at: Option<String>,
    ) -> Result<(), Error> {
        if entry.is_empty() {
            return Err(Error::Invalid("empty setup entry".into()));
        }
        self.call(move |c| {
            let row = SetupRow {
                lane_id: lane.0,
                entry,
                done_at: now()?,
                container_started_at,
            };
            diesel::insert_into(lane_setup::table)
                .values(&row)
                .on_conflict((lane_setup::lane_id, lane_setup::entry))
                .do_update()
                .set(&row)
                .execute(c)?;
            Ok(())
        })
        .await
    }
    /// Clears one setup entry or all entries after teardown.
    pub async fn clear_setup(&self, lane: LaneId, entry: Option<String>) -> Result<usize, Error> {
        self.call(move |c| {
            let query = diesel::delete(lane_setup::table.filter(lane_setup::lane_id.eq(lane.0)));
            Ok(match entry {
                Some(entry) => query.filter(lane_setup::entry.eq(entry)).execute(c)?,
                None => query.execute(c)?,
            })
        })
        .await
    }
    /// Records a task invocation with the supplied client actor.
    pub async fn start_task_run(&self, input: TaskRunInput) -> Result<TaskRun, Error> {
        if input.task.is_empty() || input.trigger.is_empty() {
            return Err(Error::Invalid("task and trigger must be nonempty".into()));
        }
        self.call(move |c| {
            let row = NewTask {
                lane_id: input.lane_id.0,
                task: input.task,
                trigger: input.trigger,
                started_at: now()?,
                actor_kind: actor_kind(input.actor.kind),
                actor_agent_id: input.actor.agent_id,
                actor_session_id: input.actor.session_id,
            };
            diesel::insert_into(task_runs::table)
                .values(&row)
                .returning(TaskRow::as_returning())
                .get_result::<TaskRow>(c)?
                .try_into()
        })
        .await
    }
    /// Completes a task once and retains its successful-once marker across retention.
    pub async fn finish_task_run(
        &self,
        id: TaskRunId,
        exit_code: Option<i32>,
    ) -> Result<(), Error> {
        self.call(move |c| {
            c.immediate_transaction(|c| {
                let ended = now()?;
                let row = diesel::update(
                    task_runs::table
                        .find(id.0)
                        .filter(task_runs::ended_at.is_null()),
                )
                .set((
                    task_runs::ended_at.eq(ended),
                    task_runs::exit_code.eq(exit_code),
                ))
                .returning((task_runs::lane_id, task_runs::task))
                .get_result::<(i64, String)>(c)
                .optional()?;
                if let Some((lane, task)) = row.filter(|_| exit_code == Some(0)) {
                    diesel::insert_into(task_completions::table)
                        .values((
                            task_completions::lane_id.eq(lane),
                            task_completions::task.eq(task),
                            task_completions::succeeded_at.eq(ended),
                        ))
                        .on_conflict((task_completions::lane_id, task_completions::task))
                        .do_update()
                        .set(task_completions::succeeded_at.eq(ended))
                        .execute(c)?;
                }
                Ok(())
            })
        })
        .await
    }
    /// Reports successful-once state independently of retained task history.
    pub async fn task_succeeded(&self, lane: LaneId, task: String) -> Result<bool, Error> {
        self.call(move |c| {
            Ok(
                diesel::select(exists(task_completions::table.find((lane.0, task))))
                    .get_result(c)?,
            )
        })
        .await
    }
    /// Returns bounded task history, newest first.
    pub async fn task_runs(&self, lane: LaneId, limit: u32) -> Result<Vec<TaskRun>, Error> {
        self.call(move |c| {
            task_runs::table
                .filter(task_runs::lane_id.eq(lane.0))
                .order(task_runs::id.desc())
                .limit(i64::from(limit))
                .select(TaskRow::as_select())
                .load::<TaskRow>(c)?
                .into_iter()
                .map(TaskRun::try_from)
                .collect()
        })
        .await
    }
    /// Resets setup and task completion state after lane cleanup.
    pub async fn forget_lane_lifecycle(&self, lane: LaneId) -> Result<(), Error> {
        self.call(move |c| {
            c.immediate_transaction(|c| {
                diesel::delete(lane_setup::table.filter(lane_setup::lane_id.eq(lane.0)))
                    .execute(c)?;
                diesel::delete(
                    task_completions::table.filter(task_completions::lane_id.eq(lane.0)),
                )
                .execute(c)?;
                diesel::delete(task_runs::table.filter(task_runs::lane_id.eq(lane.0)))
                    .execute(c)?;
                Ok(())
            })
        })
        .await
    }
    /// Upserts a rotated log segment's metadata by run and path.
    pub async fn record_log_segment(&self, input: LogSegmentInput) -> Result<(), Error> {
        self.call(move |c| {
            let row = SegmentRow {
                unit_run_id: input.unit_run_id.0,
                first_seq: sql_u64(input.first_seq),
                last_seq: sql_u64(input.last_seq),
                min_ts: sql_u64(input.min_ts_ms),
                max_ts: sql_u64(input.max_ts_ms),
                path: path_bytes(&input.path),
                compressed: i32::from(input.compressed),
                bytes: sql_u64(input.bytes),
            };
            diesel::insert_into(log_segments::table)
                .values(&row)
                .on_conflict((log_segments::unit_run_id, log_segments::path))
                .do_update()
                .set(&row)
                .execute(c)?;
            Ok(())
        })
        .await
    }
    /// Lists a run's segment metadata in sequence order.
    pub async fn log_segments(&self, run: UnitRunId) -> Result<Vec<LogSegmentInput>, Error> {
        self.call(move |c| {
            Ok(log_segments::table
                .filter(log_segments::unit_run_id.eq(run.0))
                .order(log_segments::first_seq)
                .select(SegmentRow::as_select())
                .load::<SegmentRow>(c)?
                .into_iter()
                .map(|row| LogSegmentInput {
                    unit_run_id: UnitRunId(row.unit_run_id),
                    first_seq: from_sql_u64(row.first_seq),
                    last_seq: from_sql_u64(row.last_seq),
                    min_ts_ms: from_sql_u64(row.min_ts),
                    max_ts_ms: from_sql_u64(row.max_ts),
                    path: path_from_bytes(row.path),
                    compressed: row.compressed != 0,
                    bytes: from_sql_u64(row.bytes),
                })
                .collect())
        })
        .await
    }
    /// Stores closed metric minutes, replacing repeated series/minute pairs.
    pub async fn insert_rollups(&self, rows: Vec<RollupRow>) -> Result<(), Error> {
        if rows.is_empty() {
            return Ok(());
        }
        self.call(move |c| {
            c.immediate_transaction(|c| {
                for row in rows {
                    let row = MetricRow {
                        series: row.series,
                        minute: sql_u64(row.minute_ms),
                        samples: i64::from(row.samples),
                        cpu_min: row.cpu_min,
                        cpu_avg: row.cpu_avg,
                        cpu_max: row.cpu_max,
                        mem_min: sql_u64(row.mem_min),
                        mem_avg: sql_u64(row.mem_avg),
                        mem_max: sql_u64(row.mem_max),
                        rss_max: sql_u64(row.rss_max),
                    };
                    diesel::insert_into(metric_rollups::table)
                        .values(&row)
                        .on_conflict((metric_rollups::series, metric_rollups::minute))
                        .do_update()
                        .set(&row)
                        .execute(c)?;
                }
                Ok(())
            })
        })
        .await
    }
    /// Lists series minutes at or after the cutoff, oldest first.
    pub async fn rollups(&self, series: String, since_ms: u64) -> Result<Vec<RollupRow>, Error> {
        self.call(move |c| {
            metric_rollups::table
                .filter(metric_rollups::series.eq(series))
                .filter(metric_rollups::minute.ge(sql_u64(since_ms)))
                .order(metric_rollups::minute)
                .select(MetricRow::as_select())
                .load::<MetricRow>(c)?
                .into_iter()
                .map(|row| {
                    Ok(RollupRow {
                        series: row.series,
                        minute_ms: from_sql_u64(row.minute),
                        samples: checked(row.samples)?,
                        cpu_min: row.cpu_min,
                        cpu_avg: row.cpu_avg,
                        cpu_max: row.cpu_max,
                        mem_min: from_sql_u64(row.mem_min),
                        mem_avg: from_sql_u64(row.mem_avg),
                        mem_max: from_sql_u64(row.mem_max),
                        rss_max: from_sql_u64(row.rss_max),
                    })
                })
                .collect()
        })
        .await
    }
    /// Upserts a unit run's final resource profile.
    pub async fn record_run_summary(&self, row: RunSummaryRow) -> Result<(), Error> {
        self.call(move |c| {
            let row = SummaryRow {
                unit_run_id: row.unit_run_id.0,
                series: row.series,
                started_at: sql_u64(row.started_ms),
                ended_at: sql_u64(row.ended_ms),
                samples: sql_u64(row.samples),
                mem_p50: sql_u64(row.mem_p50),
                mem_p95: sql_u64(row.mem_p95),
                mem_peak: sql_u64(row.mem_peak),
                cpu_p50: row.cpu_p50,
                cpu_p95: row.cpu_p95,
                cpu_peak: row.cpu_peak,
                rss_peak: sql_u64(row.rss_peak),
            };
            diesel::insert_into(run_summaries::table)
                .values(&row)
                .on_conflict(run_summaries::unit_run_id)
                .do_update()
                .set(&row)
                .execute(c)?;
            Ok(())
        })
        .await
    }
    /// Returns recent profiles, newest first, for baseline calculation.
    pub async fn run_summaries(
        &self,
        series: String,
        limit: u32,
    ) -> Result<Vec<RunSummaryRow>, Error> {
        self.call(move |c| {
            Ok(run_summaries::table
                .filter(run_summaries::series.eq(series))
                .order(run_summaries::ended_at.desc())
                .limit(i64::from(limit))
                .select(SummaryRow::as_select())
                .load::<SummaryRow>(c)?
                .into_iter()
                .map(|row| RunSummaryRow {
                    unit_run_id: UnitRunId(row.unit_run_id),
                    series: row.series,
                    started_ms: from_sql_u64(row.started_at),
                    ended_ms: from_sql_u64(row.ended_at),
                    samples: from_sql_u64(row.samples),
                    mem_p50: from_sql_u64(row.mem_p50),
                    mem_p95: from_sql_u64(row.mem_p95),
                    mem_peak: from_sql_u64(row.mem_peak),
                    cpu_p50: row.cpu_p50,
                    cpu_p95: row.cpu_p95,
                    cpu_peak: row.cpu_peak,
                    rss_peak: from_sql_u64(row.rss_peak),
                })
                .collect())
        })
        .await
    }
    /// Updates the last successful connection metadata for a remote host.
    pub async fn upsert_remote_host(&self, host: RemoteHost) -> Result<(), Error> {
        if host.name.is_empty() {
            return Err(Error::Invalid("empty remote host name".into()));
        }
        self.call(move |c| {
            let row = RemoteRow {
                name: host.name,
                transport: host.transport,
                address: host.address,
                daemon_version: host.daemon_version,
                last_connected: host.last_connected,
            };
            diesel::insert_into(remote_hosts::table)
                .values(&row)
                .on_conflict(remote_hosts::name)
                .do_update()
                .set(&row)
                .execute(c)?;
            Ok(())
        })
        .await
    }
    /// Lists registered remote hosts by name.
    pub async fn remote_hosts(&self) -> Result<Vec<RemoteHost>, Error> {
        self.call(|c| {
            Ok(remote_hosts::table
                .order(remote_hosts::name)
                .select(RemoteRow::as_select())
                .load::<RemoteRow>(c)?
                .into_iter()
                .map(|row| RemoteHost {
                    name: row.name,
                    transport: row.transport,
                    address: row.address,
                    daemon_version: row.daemon_version,
                    last_connected: row.last_connected,
                })
                .collect())
        })
        .await
    }
    /// Compatibility cutoff-based retention. New callers should use `prune`.
    pub async fn prune_history(
        &self,
        runs_before_ms: i64,
        rollups_before_ms: i64,
    ) -> Result<Pruned, Error> {
        self.call(move |c| {
            c.immediate_transaction(|c| {
                let unit_runs =
                    diesel::delete(unit_runs::table.filter(unit_runs::ended_at.lt(runs_before_ms)))
                        .execute(c)?;
                let task_runs =
                    diesel::delete(task_runs::table.filter(task_runs::ended_at.lt(runs_before_ms)))
                        .execute(c)?;
                let rollups = diesel::delete(
                    metric_rollups::table.filter(metric_rollups::minute.lt(rollups_before_ms)),
                )
                .execute(c)?;
                Ok(Pruned {
                    unit_runs,
                    task_runs,
                    rollups,
                })
            })
        })
        .await
    }
    /// Deletes all application records after resources have been stopped.
    /// Migration history and monotonic IDs are retained.
    pub async fn reset(&self) -> Result<(), Error> {
        self.call(|c| {
            c.immediate_transaction(|c| {
                diesel::delete(log_segments::table).execute(c)?;
                diesel::delete(run_summaries::table).execute(c)?;
                diesel::delete(metric_rollups::table).execute(c)?;
                diesel::delete(remote_hosts::table).execute(c)?;
                diesel::delete(lane_setup::table).execute(c)?;
                diesel::delete(task_completions::table).execute(c)?;
                diesel::delete(task_runs::table).execute(c)?;
                diesel::delete(unit_runs::table).execute(c)?;
                diesel::delete(port_offsets::table).execute(c)?;
                diesel::delete(port_blocks::table).execute(c)?;
                diesel::delete(lanes::table).execute(c)?;
                diesel::delete(worktrees::table).execute(c)?;
                diesel::delete(projects::table).execute(c)?;
                Ok(())
            })
        })
        .await
    }
}
