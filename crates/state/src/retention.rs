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

/// Actual deletion counts, including cascades, and post-vacuum disk usage.
/// `budget_exceeded` can remain true when live data or pinned WAL readers prevent
/// reclamation. Log files themselves remain owned by the log retention service.
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
}

impl Store {
    /// Prunes expired records, then oldest metrics/history under budget pressure.
    /// Call periodically from the daemon; this does not spawn a scheduler.
    pub async fn prune(&self, policy: RetentionPolicy) -> Result<PruneReport, Error> {
        self.prune_at(policy, now()?).await
    }

    /// Runs retention at a supplied Unix-millisecond clock for deterministic tests
    /// and daemon scheduling. Negative clocks and zero budgets are rejected.
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
        self.call(move |c| {
            let bytes_before = disk_bytes(c, &path)?;
            let mut busy = checkpoint(c)?;
            let sidecars = disk_bytes(c, &path)?.saturating_sub(allocated_bytes(c)?);
            let page_budget = policy.max_size_bytes.saturating_sub(sidecars);
            let mut report = c.immediate_transaction(|c| {
                let before = counts(c)?;
                prune_age(c, &policy, now_ms)?;
                while used_bytes(c)? > page_budget {
                    let keys = metric_rollups::table
                        .order((metric_rollups::minute, metric_rollups::series))
                        .select((metric_rollups::series, metric_rollups::minute))
                        .limit(256)
                        .load::<(String, i64)>(c)?;
                    if keys.is_empty() {
                        break;
                    }
                    for key in keys {
                        diesel::delete(metric_rollups::table.find(key)).execute(c)?;
                        if used_bytes(c)? <= page_budget {
                            break;
                        }
                    }
                }
                while used_bytes(c)? > page_budget && evict_history(c)? {}
                let after = counts(c)?;
                Ok::<_, Error>(PruneReport {
                    unit_runs: before[0] - after[0],
                    task_runs: before[1] - after[1],
                    metric_rollups: before[2] - after[2],
                    run_summaries: before[3] - after[3],
                    log_segments: before[4] - after[4],
                    remote_hosts: before[5] - after[5],
                    bytes_before,
                    ..Default::default()
                })
            })?;
            // Incremental mode returns free pages without rewriting the whole file.
            c.batch_execute("PRAGMA incremental_vacuum;")?;
            busy |= checkpoint(c)?;
            report.bytes_after = disk_bytes(c, &path)?;
            report.budget_exceeded = report.bytes_after > policy.max_size_bytes;
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
fn prune_age(c: &mut SqliteConnection, policy: &RetentionPolicy, now_ms: i64) -> Result<(), Error> {
    // Segment rows describe closed files, even when their process is still running.
    diesel::delete(
        log_segments::table.filter(log_segments::max_ts.lt(cutoff(now_ms, policy.logs))),
    )
    .execute(c)?;
    diesel::delete(
        run_summaries::table.filter(run_summaries::ended_at.lt(cutoff(now_ms, policy.metrics))),
    )
    .execute(c)?;
    diesel::delete(
        metric_rollups::table.filter(metric_rollups::minute.lt(cutoff(now_ms, policy.metrics))),
    )
    .execute(c)?;
    diesel::delete(
        task_runs::table.filter(task_runs::ended_at.lt(cutoff(now_ms, policy.run_history))),
    )
    .execute(c)?;
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
    .execute(c)?;
    diesel::delete(
        remote_hosts::table
            .filter(remote_hosts::last_connected.lt(cutoff(now_ms, policy.remote_hosts))),
    )
    .execute(c)?;
    Ok(())
}

/// Samples counts under the writer transaction so cascade accounting is exact.
fn counts(c: &mut SqliteConnection) -> Result<[u64; 6], Error> {
    Ok([
        from_sql_u64(unit_runs::table.count().get_result(c)?),
        from_sql_u64(task_runs::table.count().get_result(c)?),
        from_sql_u64(metric_rollups::table.count().get_result(c)?),
        from_sql_u64(run_summaries::table.count().get_result(c)?),
        from_sql_u64(log_segments::table.count().get_result(c)?),
        from_sql_u64(remote_hosts::table.count().get_result(c)?),
    ])
}

/// Deletes the oldest eligible historical row across groups, including young
/// history when required by the byte budget. Unfinished process/task rows stay;
/// closed log segments and summaries are history regardless of parent run status.
fn evict_history(c: &mut SqliteConnection) -> Result<bool, Error> {
    let unit = unit_runs::table
        .filter(unit_runs::ended_at.is_not_null())
        .filter(diesel::dsl::not(exists(
            log_segments::table.filter(log_segments::unit_run_id.eq(unit_runs::id)),
        )))
        .filter(diesel::dsl::not(exists(
            run_summaries::table.filter(run_summaries::unit_run_id.eq(unit_runs::id)),
        )))
        .order((unit_runs::ended_at, unit_runs::id))
        .select((unit_runs::id, unit_runs::ended_at.assume_not_null()))
        .first::<(i64, i64)>(c)
        .optional()?;
    let task = task_runs::table
        .filter(task_runs::ended_at.is_not_null())
        .order((task_runs::ended_at, task_runs::id))
        .select((task_runs::id, task_runs::ended_at.assume_not_null()))
        .first::<(i64, i64)>(c)
        .optional()?;
    let summary = run_summaries::table
        .order((run_summaries::ended_at, run_summaries::unit_run_id))
        .select((run_summaries::unit_run_id, run_summaries::ended_at))
        .first::<(i64, i64)>(c)
        .optional()?;
    let log = log_segments::table
        .order((log_segments::max_ts, log_segments::id))
        .select((log_segments::id, log_segments::max_ts))
        .first::<(i64, i64)>(c)
        .optional()?;
    let remote = remote_hosts::table
        .filter(remote_hosts::last_connected.is_not_null())
        .order((remote_hosts::last_connected, remote_hosts::name))
        .select((
            remote_hosts::name,
            remote_hosts::last_connected.assume_not_null(),
        ))
        .first::<(String, i64)>(c)
        .optional()?;
    let mut candidates: Vec<_> = [unit, task, summary, log]
        .into_iter()
        .enumerate()
        .filter_map(|(kind, row)| row.map(|(id, time)| (time, kind, id)))
        .collect();
    if let Some((_, time)) = &remote {
        candidates.push((*time, 4, 0));
    }
    let Some((_, kind, id)) = candidates.into_iter().min() else {
        return Ok(false);
    };
    match kind {
        0 => {
            diesel::delete(unit_runs::table.find(id)).execute(c)?;
        }
        1 => {
            diesel::delete(task_runs::table.find(id)).execute(c)?;
        }
        2 => {
            diesel::delete(run_summaries::table.find(id)).execute(c)?;
        }
        3 => {
            diesel::delete(log_segments::table.find(id)).execute(c)?;
        }
        _ => {
            if let Some((name, _)) = remote {
                diesel::delete(remote_hosts::table.find(name)).execute(c)?;
            }
        }
    }
    Ok(true)
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
            Err(error) => return Err(Error::Filesystem(error.to_string())),
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
