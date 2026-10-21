//! Retention and disk reclamation on the database actor thread.

use crate::{Error, Store, models::from_sql_u64, queries::now, schema::*};
use diesel::{connection::SimpleConnection, dsl::exists, prelude::*};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

/// Default lifetime of finished process and task history.
pub const DEFAULT_RUN_HISTORY_RETENTION: Duration = Duration::from_secs(30 * 86_400);
/// Default metrics lifetime; the daemon overrides this from `metrics.retention`.
pub const DEFAULT_METRICS_RETENTION: Duration = Duration::from_secs(30 * 86_400);
/// Default segment metadata lifetime; the daemon overrides this from log retention.
pub const DEFAULT_LOG_RETENTION: Duration = Duration::from_secs(7 * 86_400);
/// Default lifetime since a remote host's last successful connection.
pub const DEFAULT_REMOTE_HOST_RETENTION: Duration = Duration::from_secs(90 * 86_400);
/// Default budget for the database and its SQLite sidecar files.
pub const DEFAULT_MAX_SIZE_BYTES: u64 = 512 * 1024 * 1024;

/// Age limits for finished history and a soft disk budget. Budget pressure may
/// shorten history retention, but never deletes registry, completion, or live rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Maximum age since a unit or task finished.
    pub run_history: Duration,
    /// Maximum age of metric minutes and completed-run summaries.
    pub metrics: Duration,
    /// Maximum age of closed segment metadata; does not delete log files.
    pub logs: Duration,
    /// Maximum age since last connection; hosts without a timestamp are retained.
    pub remote_hosts: Duration,
    /// Database plus WAL/shared-memory budget; protected live state may exceed it.
    pub max_size_bytes: u64,
}

impl Default for RetentionPolicy {
    /// Uses public defaults independently of configuration loading.
    fn default() -> Self {
        Self {
            run_history: DEFAULT_RUN_HISTORY_RETENTION,
            metrics: DEFAULT_METRICS_RETENTION,
            logs: DEFAULT_LOG_RETENTION,
            remote_hosts: DEFAULT_REMOTE_HOST_RETENTION,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
        }
    }
}

/// Most rows one size-budget batch deletes. Batches start at a sixteenth of this
/// and double, so a database slightly over budget loses little extra history.
pub const PRUNE_BATCH_ROWS: usize = 1024;
/// Most size-budget batches in one pass; the rest waits for the next pass
/// (see [`PruneReport::more`]).
pub const PRUNE_MAX_BATCHES: usize = 16;

/// Actual deletion counts and post-vacuum disk usage. `budget_exceeded` can
/// remain true when live data or pinned WAL readers prevent reclamation. Log
/// files themselves remain owned by the log retention service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    pub unit_runs: u64,
    pub task_runs: u64,
    pub metric_rollups: u64,
    pub run_summaries: u64,
    pub log_segments: u64,
    pub remote_hosts: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub budget_exceeded: bool,
    pub checkpoint_busy: bool,
    /// The pass stopped at [`PRUNE_MAX_BATCHES`] while over budget with
    /// deletable history left; run another pass soon.
    #[serde(default)]
    pub more: bool,
}

impl PruneReport {
    /// Adds one batch's deletions.
    fn add(&mut self, deleted: &PruneReport) {
        self.unit_runs += deleted.unit_runs;
        self.task_runs += deleted.task_runs;
        self.metric_rollups += deleted.metric_rollups;
        self.run_summaries += deleted.run_summaries;
        self.log_segments += deleted.log_segments;
        self.remote_hosts += deleted.remote_hosts;
    }
}

/// Outcome of one size-budget batch.
enum Batch {
    /// Under budget, or nothing deletable is left.
    Done,
    Deleted(PruneReport),
}

impl Store {
    /// Prunes expired records, then oldest metrics/history under budget pressure.
    /// Call periodically from the daemon; this does not spawn a scheduler.
    pub async fn prune(&self, policy: RetentionPolicy) -> Result<PruneReport, Error> {
        self.prune_at(policy, now()?).await
    }

    /// Runs retention at a supplied Unix-millisecond clock for deterministic tests
    /// and daemon scheduling. Negative clocks and zero budgets are rejected.
    ///
    /// Age pruning is one transaction; size-budget eviction then runs as separate
    /// store jobs of at most [`PRUNE_BATCH_ROWS`] rows each, so other queued
    /// store calls run between batches instead of waiting for the whole pass.
    pub async fn prune_at(
        &self,
        policy: RetentionPolicy,
        now_ms: i64,
    ) -> Result<PruneReport, Error> {
        if now_ms < 0 || policy.max_size_bytes == 0 {
            return Err(Error::Invalid(
                "retention requires a nonnegative clock and nonzero size budget".into(),
            ));
        }
        let path = self.database_path.clone();
        let max_size = policy.max_size_bytes;
        let (mut report, page_budget, mut busy) = self
            .call(move |c| {
                let bytes_before = disk_bytes(c, &path)?;
                let busy = checkpoint(c)?;
                let sidecars = disk_bytes(c, &path)?.saturating_sub(allocated_bytes(c)?);
                let page_budget = policy.max_size_bytes.saturating_sub(sidecars);
                let mut report = c.immediate_transaction(|c| prune_age(c, &policy, now_ms))?;
                report.bytes_before = bytes_before;
                Ok((report, page_budget, busy))
            })
            .await?;
        let mut rows = PRUNE_BATCH_ROWS / 16;
        let mut batches = 0;
        loop {
            if batches == PRUNE_MAX_BATCHES {
                report.more = true;
                break;
            }
            let limit = rows;
            match self
                .call(move |c| c.immediate_transaction(|c| budget_batch(c, page_budget, limit)))
                .await?
            {
                Batch::Done => break,
                Batch::Deleted(deleted) => report.add(&deleted),
            }
            batches += 1;
            rows = (rows * 2).min(PRUNE_BATCH_ROWS);
        }
        let path = self.database_path.clone();
        self.call(move |c| {
            // Incremental mode returns free pages without rewriting the whole file.
            c.batch_execute("PRAGMA incremental_vacuum;")?;
            busy |= checkpoint(c)?;
            report.bytes_after = disk_bytes(c, &path)?;
            report.budget_exceeded = report.bytes_after > max_size;
            report.more &= report.budget_exceeded;
            report.checkpoint_busy = busy;
            Ok(report)
        })
        .await
    }
}

/// Computes cutoffs without overflowing very large user-provided durations.
fn cutoff(now_ms: i64, age: Duration) -> i64 {
    now_ms.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX))
}

/// Applies each group's age. Parent run rows stay while younger child history exists.
/// Rows of a deleted unit run's own children (`unit_run_members`) cascade and are
/// not counted; every other count is exact.
fn prune_age(
    c: &mut SqliteConnection,
    policy: &RetentionPolicy,
    now_ms: i64,
) -> Result<PruneReport, Error> {
    // Segment rows describe closed files, even when their process is still running.
    let log_segments = count(
        diesel::delete(
            log_segments::table.filter(log_segments::max_ts.lt(cutoff(now_ms, policy.logs))),
        )
        .execute(c)?,
    );
    let run_summaries = count(
        diesel::delete(
            run_summaries::table.filter(run_summaries::ended_at.lt(cutoff(now_ms, policy.metrics))),
        )
        .execute(c)?,
    );
    let metric_rollups = count(
        diesel::delete(
            metric_rollups::table.filter(metric_rollups::minute.lt(cutoff(now_ms, policy.metrics))),
        )
        .execute(c)?,
    );
    // Startup peaks are metrics history too; their count per series is capped on insert.
    diesel::delete(
        startup_peaks::table.filter(startup_peaks::recorded_at.lt(cutoff(now_ms, policy.metrics))),
    )
    .execute(c)?;
    let task_runs = count(
        diesel::delete(
            task_runs::table.filter(task_runs::ended_at.lt(cutoff(now_ms, policy.run_history))),
        )
        .execute(c)?,
    );
    let unit_runs = count(
        diesel::delete(
            unit_runs::table
                .filter(unit_runs::ended_at.lt(cutoff(now_ms, policy.run_history)))
                .filter(diesel::dsl::not(exists(
                    log_segments::table.filter(log_segments::unit_run_id.eq(unit_runs::id)),
                )))
                .filter(diesel::dsl::not(exists(
                    run_summaries::table.filter(run_summaries::unit_run_id.eq(unit_runs::id)),
                ))),
        )
        .execute(c)?,
    );
    let remote_hosts = count(
        diesel::delete(
            remote_hosts::table
                .filter(remote_hosts::last_connected.lt(cutoff(now_ms, policy.remote_hosts))),
        )
        .execute(c)?,
    );
    Ok(PruneReport {
        unit_runs,
        task_runs,
        metric_rollups,
        run_summaries,
        log_segments,
        remote_hosts,
        ..PruneReport::default()
    })
}

/// Converts an affected-row count.
fn count(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

/// One size-budget batch, run in its own transaction. Measures usage once, then
/// deletes up to `limit` of the oldest metric minutes or, once none are left, of
/// the oldest history rows across groups. Unfinished process/task rows stay;
/// closed log segments and summaries are history regardless of parent run status,
/// and a run whose children go in this batch becomes eligible in the next one.
fn budget_batch(c: &mut SqliteConnection, page_budget: u64, limit: usize) -> Result<Batch, Error> {
    if used_bytes(c)? <= page_budget {
        return Ok(Batch::Done);
    }
    let sql_limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let metrics = diesel::sql_query(
        "DELETE FROM metric_rollups WHERE rowid IN \
         (SELECT rowid FROM metric_rollups ORDER BY minute, series LIMIT ?)",
    )
    .bind::<diesel::sql_types::BigInt, _>(sql_limit)
    .execute(c)?;
    if metrics > 0 {
        return Ok(Batch::Deleted(PruneReport {
            metric_rollups: count(metrics),
            ..PruneReport::default()
        }));
    }
    // The oldest `limit` of each group, merged; each group query walks its index.
    let mut candidates: Vec<(i64, u8, i64, String)> = Vec::new();
    let units = unit_runs::table
        .filter(unit_runs::ended_at.is_not_null())
        .filter(diesel::dsl::not(exists(
            log_segments::table.filter(log_segments::unit_run_id.eq(unit_runs::id)),
        )))
        .filter(diesel::dsl::not(exists(
            run_summaries::table.filter(run_summaries::unit_run_id.eq(unit_runs::id)),
        )))
        .order((unit_runs::ended_at, unit_runs::id))
        .select((unit_runs::id, unit_runs::ended_at.assume_not_null()))
        .limit(sql_limit)
        .load::<(i64, i64)>(c)?;
    candidates.extend(units.into_iter().map(|(id, at)| (at, 0, id, String::new())));
    let tasks = task_runs::table
        .filter(task_runs::ended_at.is_not_null())
        .order((task_runs::ended_at, task_runs::id))
        .select((task_runs::id, task_runs::ended_at.assume_not_null()))
        .limit(sql_limit)
        .load::<(i64, i64)>(c)?;
    candidates.extend(tasks.into_iter().map(|(id, at)| (at, 1, id, String::new())));
    let summaries = run_summaries::table
        .order((run_summaries::ended_at, run_summaries::unit_run_id))
        .select((run_summaries::unit_run_id, run_summaries::ended_at))
        .limit(sql_limit)
        .load::<(i64, i64)>(c)?;
    candidates.extend(
        summaries
            .into_iter()
            .map(|(id, at)| (at, 2, id, String::new())),
    );
    let segments = log_segments::table
        .order((log_segments::max_ts, log_segments::id))
        .select((log_segments::id, log_segments::max_ts))
        .limit(sql_limit)
        .load::<(i64, i64)>(c)?;
    candidates.extend(
        segments
            .into_iter()
            .map(|(id, at)| (at, 3, id, String::new())),
    );
    let remotes = remote_hosts::table
        .filter(remote_hosts::last_connected.is_not_null())
        .order((remote_hosts::last_connected, remote_hosts::name))
        .select((
            remote_hosts::name,
            remote_hosts::last_connected.assume_not_null(),
        ))
        .limit(sql_limit)
        .load::<(String, i64)>(c)?;
    candidates.extend(remotes.into_iter().map(|(name, at)| (at, 4, 0, name)));
    if candidates.is_empty() {
        return Ok(Batch::Done);
    }
    candidates.sort_unstable();
    candidates.truncate(limit);
    let ids = |kind: u8| -> Vec<i64> {
        candidates
            .iter()
            .filter(|candidate| candidate.1 == kind)
            .map(|candidate| candidate.2)
            .collect()
    };
    let (units, tasks, summaries, segments) = (ids(0), ids(1), ids(2), ids(3));
    let remotes: Vec<&str> = candidates
        .iter()
        .filter(|candidate| candidate.1 == 4)
        .map(|candidate| candidate.3.as_str())
        .collect();
    Ok(Batch::Deleted(PruneReport {
        unit_runs: count(
            diesel::delete(unit_runs::table.filter(unit_runs::id.eq_any(units))).execute(c)?,
        ),
        task_runs: count(
            diesel::delete(task_runs::table.filter(task_runs::id.eq_any(tasks))).execute(c)?,
        ),
        run_summaries: count(
            diesel::delete(
                run_summaries::table.filter(run_summaries::unit_run_id.eq_any(summaries)),
            )
            .execute(c)?,
        ),
        log_segments: count(
            diesel::delete(log_segments::table.filter(log_segments::id.eq_any(segments)))
                .execute(c)?,
        ),
        remote_hosts: count(
            diesel::delete(remote_hosts::table.filter(remote_hosts::name.eq_any(remotes)))
                .execute(c)?,
        ),
        ..PruneReport::default()
    }))
}

#[derive(diesel::QueryableByName)]
struct PageCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    page_count: i64,
}
#[derive(diesel::QueryableByName)]
struct FreeCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    freelist_count: i64,
}
#[derive(diesel::QueryableByName)]
struct PageSize {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    page_size: i64,
}
#[derive(diesel::QueryableByName)]
struct Checkpoint {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    busy: i32,
}

/// Measures pages still occupied, excluding pages incremental vacuum can reclaim.
fn used_bytes(c: &mut SqliteConnection) -> Result<u64, Error> {
    let total = diesel::sql_query("PRAGMA page_count")
        .get_result::<PageCount>(c)?
        .page_count;
    let free = diesel::sql_query("PRAGMA freelist_count")
        .get_result::<FreeCount>(c)?
        .freelist_count;
    let size = diesel::sql_query("PRAGMA page_size")
        .get_result::<PageSize>(c)?
        .page_size;
    Ok(from_sql_u64(total - free).saturating_mul(from_sql_u64(size)))
}

/// Counts the database and WAL/shared-memory sidecars; in-memory stores use pages.
fn allocated_bytes(c: &mut SqliteConnection) -> Result<u64, Error> {
    let pages = diesel::sql_query("PRAGMA page_count")
        .get_result::<PageCount>(c)?
        .page_count;
    let size = diesel::sql_query("PRAGMA page_size")
        .get_result::<PageSize>(c)?
        .page_size;
    Ok(from_sql_u64(pages).saturating_mul(from_sql_u64(size)))
}

/// Measures physical files on the database thread, including SQLite sidecars.
fn disk_bytes(c: &mut SqliteConnection, path: &Path) -> Result<u64, Error> {
    if path == Path::new(":memory:") {
        return allocated_bytes(c);
    }
    let mut bytes = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_os_string();
        file.push(suffix);
        match std::fs::metadata(file) {
            Ok(metadata) => bytes = bytes.saturating_add(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::Filesystem(error)),
        }
    }
    Ok(bytes)
}

/// Truncates reclaimable WAL pages; pinned readers are reported rather than killed.
fn checkpoint(c: &mut SqliteConnection) -> Result<bool, Error> {
    Ok(diesel::sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
        .get_result::<Checkpoint>(c)?
        .busy
        != 0)
}
