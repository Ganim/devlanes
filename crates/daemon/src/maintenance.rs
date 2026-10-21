//! Background maintenance. Cross-unit log retention: each live unit trims its own
//! directory as it writes; this pass enforces `logs.retention` over every unit
//! directory together, oldest segments first, so units that stopped long ago
//! cannot keep the disk full. State retention: state.db history and its size
//! budget, from `history`, `metrics`, `logs` and `state` in the user config.

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use devlanes_config::UserConfig;
use devlanes_state::{PruneReport, RetentionPolicy};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{engine::Inner, model::lock, units::LogRetention};

/// How often the log pass runs.
const INTERVAL: Duration = Duration::from_secs(600);
/// The first state pass waits this long, so it never competes with startup.
const STATE_FIRST: Duration = Duration::from_secs(60);
/// How often the state pass runs afterwards.
const STATE_INTERVAL: Duration = Duration::from_secs(3600);

/// The latest state pass, as `doctor` shows it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LastPrune {
    pub at_ms: i64,
    #[serde(flatten)]
    pub report: PruneReport,
}

/// State retention from the user config.
pub(crate) fn retention_policy(user: &UserConfig) -> RetentionPolicy {
    RetentionPolicy {
        run_history: user.history.retention.0,
        metrics: user.metrics.retention.0,
        logs: Duration::from_secs(u64::from(user.logs.retention.days).saturating_mul(86_400)),
        remote_hosts: user.history.remote_hosts_retention.0,
        max_size_bytes: user.state.max_size.0,
    }
}

/// True when the pass deleted any row.
fn deleted_any(report: &PruneReport) -> bool {
    report.unit_runs
        + report.task_runs
        + report.metric_rollups
        + report.run_summaries
        + report.log_segments
        + report.remote_hosts
        > 0
}

/// Bytes of the database and its WAL and shared-memory files.
fn database_bytes(db: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let mut path = db.as_os_str().to_owned();
            path.push(suffix);
            std::fs::metadata(path).ok()
        })
        .map(|meta| meta.len())
        .sum()
}

impl Inner {
    /// One state retention pass as of `now_ms`, remembered for `doctor`.
    pub(crate) async fn prune_state(
        &self,
        now_ms: i64,
    ) -> Result<PruneReport, devlanes_state::Error> {
        let report = self
            .store
            .prune_at(retention_policy(&self.opts.user), now_ms)
            .await?;
        if deleted_any(&report) {
            tracing::info!(?report, "pruned state history");
        }
        if report.budget_exceeded {
            tracing::warn!(
                bytes = report.bytes_after,
                budget = self.opts.user.state.max_size.0,
                "state.db stays over state.max_size"
            );
        }
        *lock(&self.last_prune) = Some(LastPrune {
            at_ms: now_ms,
            report: report.clone(),
        });
        Ok(report)
    }

    /// `doctor` facts: `{size_bytes, budget_bytes, last_prune}`.
    pub(crate) async fn state_doctor(&self) -> Value {
        let db = self.opts.db_path.clone();
        let size = tokio::task::spawn_blocking(move || database_bytes(&db))
            .await
            .unwrap_or_default();
        json!({
            "size_bytes": size,
            "budget_bytes": self.opts.user.state.max_size.0,
            "last_prune": lock(&self.last_prune).clone(),
        })
    }

    /// Runs [`Self::prune_state`] shortly after startup, then hourly, until shutdown.
    pub(crate) async fn state_maintenance_loop(self: Arc<Self>) {
        let mut wait = STATE_FIRST;
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            wait = STATE_INTERVAL;
            let now = i64::try_from(crate::units::now_ms()).unwrap_or(i64::MAX);
            if let Err(error) = self.prune_state(now).await {
                tracing::warn!(%error, "state retention failed");
            }
        }
    }
}

/// What one pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Swept {
    pub files: usize,
    pub bytes: u64,
}

/// One segment file of a unit directory.
#[derive(Debug)]
struct Segment {
    path: PathBuf,
    first_seq: u64,
    bytes: u64,
    modified: SystemTime,
}

/// Segment sequence from `<seq:020>.log` or `<seq:020>.log.zst`.
fn segment_seq(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(".log.zst")
        .or_else(|| name.strip_suffix(".log"))?;
    (stem.len() == 20).then(|| stem.parse().ok()).flatten()
}

/// Segments of one directory, oldest first, or empty when it holds none.
fn segments(dir: &Path) -> io::Result<Vec<Segment>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(first_seq) = name.to_str().and_then(segment_seq) else {
            continue;
        };
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            found.push(Segment {
                path: entry.path(),
                first_seq,
                bytes: metadata.len(),
                modified: metadata.modified()?,
            });
        }
    }
    found.sort_by_key(|segment| segment.first_seq);
    Ok(found)
}

/// Every directory below `root` that holds segment files.
fn unit_dirs(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut has_segments = false;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            unit_dirs(&entry.path(), out)?;
        } else if kind.is_file() && entry.file_name().to_str().and_then(segment_seq).is_some() {
            has_segments = true;
        }
    }
    if has_segments {
        out.push(root.to_path_buf());
    }
    Ok(())
}

/// Removes a segment and its sidecar index.
fn remove(segment: &Segment) -> io::Result<()> {
    std::fs::remove_file(&segment.path)?;
    let meta = segment
        .path
        .with_file_name(format!("{:020}.meta.json", segment.first_seq));
    match std::fs::remove_file(meta) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Applies `retention` to every unit directory below `root`. Directories in `live`
/// belong to running units and are never touched, but their bytes count toward the
/// total. The newest segment of each directory stays, because it carries the
/// sequence number the next run continues from.
pub(crate) fn sweep(
    root: &Path,
    live: &BTreeSet<PathBuf>,
    retention: LogRetention,
    now: SystemTime,
) -> io::Result<Swept> {
    let mut dirs = Vec::new();
    if root.is_dir() {
        unit_dirs(root, &mut dirs)?;
    }
    let mut total: u64 = 0;
    let mut candidates = Vec::new();
    for dir in dirs {
        let mut found = segments(&dir)?;
        total = found
            .iter()
            .fold(total, |sum, segment| sum.saturating_add(segment.bytes));
        if live.contains(&dir) {
            continue;
        }
        found.pop();
        candidates.extend(found);
    }
    candidates.sort_by_key(|segment| segment.modified);
    let max_age = Duration::from_millis(retention.max_age_ms);
    let mut swept = Swept::default();
    for segment in candidates {
        let expired = now
            .duration_since(segment.modified)
            .is_ok_and(|age| age > max_age);
        if !expired && total <= retention.max_bytes {
            continue;
        }
        remove(&segment)?;
        total = total.saturating_sub(segment.bytes);
        swept.files += 1;
        swept.bytes += segment.bytes;
    }
    Ok(swept)
}

impl Inner {
    /// Runs [`sweep`] over the logs directory every ten minutes until shutdown.
    pub(crate) async fn log_maintenance_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(INTERVAL);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            let live: BTreeSet<PathBuf> = self
                .all_lanes()
                .iter()
                .flat_map(|(_, lane)| {
                    lane.st()
                        .units
                        .values()
                        .filter(|unit| unit.host.is_alive())
                        .map(|unit| unit.host.info().log_dir.clone())
                        .collect::<Vec<_>>()
                })
                .collect();
            let root = self.opts.logs.clone();
            let retention = LogRetention::from_config(&self.opts.user.logs.retention);
            let result = tokio::task::spawn_blocking(move || {
                sweep(&root, &live, retention, SystemTime::now())
            })
            .await;
            match result {
                Ok(Ok(swept)) if swept.files > 0 => {
                    tracing::info!(files = swept.files, bytes = swept.bytes, "removed old logs");
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "log retention failed"),
                Err(error) => tracing::warn!(%error, "log retention task failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Every limit comes from its user config key; the defaults match the store's.
    fn state_policy_from_user_config() {
        assert_eq!(
            retention_policy(&UserConfig::default()),
            RetentionPolicy::default()
        );
        let user = devlanes_config::parse_user(
            "history: { retention: 2d, remote_hosts_retention: 5d }\n\
             metrics: { retention: 3d }\n\
             logs: { retention: { days: 4 } }\n\
             state: { max_size: 1MiB }\n",
        )
        .expect("user config");
        let day = Duration::from_secs(86_400);
        assert_eq!(
            retention_policy(&user),
            RetentionPolicy {
                run_history: day * 2,
                metrics: day * 3,
                logs: day * 4,
                remote_hosts: day * 5,
                max_size_bytes: 1 << 20,
            }
        );
    }

    /// Writes a segment of `bytes` whose mtime is `age` in the past.
    fn segment(dir: &Path, seq: u64, bytes: usize, age: Duration, now: SystemTime) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(format!("{seq:020}.log.zst"));
        std::fs::write(&path, vec![b'x'; bytes]).expect("write");
        std::fs::write(dir.join(format!("{seq:020}.meta.json")), "{}").expect("meta");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(now - age).expect("mtime");
        path
    }

    #[test]
    /// The size bound spans every unit directory and removes the oldest segments
    /// first; live units and each directory's newest segment are kept.
    fn size_and_age_across_run_directories() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let hour = Duration::from_secs(3600);
        let a = root.path().join("acme/0/api");
        let b = root.path().join("acme/1/api");
        let live = root.path().join("acme/0/web");
        let a1 = segment(&a, 1, 100, hour * 10, now);
        let a2 = segment(&a, 50, 100, hour * 3, now);
        let b1 = segment(&b, 1, 100, hour * 8, now);
        let b2 = segment(&b, 20, 100, hour * 2, now);
        let b3 = segment(&b, 40, 100, hour, now);
        let l1 = segment(&live, 1, 100, hour * 20, now);
        let l2 = segment(&live, 9, 100, hour, now);
        std::fs::write(root.path().join("daemon.log"), vec![b'x'; 1000]).expect("daemon log");
        let retention = LogRetention {
            max_bytes: 500,
            max_age_ms: u64::MAX,
        };
        let swept =
            sweep(root.path(), &BTreeSet::from([live.clone()]), retention, now).expect("sweep");
        // 700 bytes in segments: the two oldest removable ones (a1, b1) go.
        assert_eq!(
            swept,
            Swept {
                files: 2,
                bytes: 200
            }
        );
        assert!(!a1.exists() && !b1.exists());
        assert!(!a.join(format!("{:020}.meta.json", 1)).exists());
        for kept in [&a2, &b2, &b3, &l1, &l2] {
            assert!(kept.exists(), "{}", kept.display());
        }
        assert!(root.path().join("daemon.log").exists());

        let by_age = LogRetention {
            max_bytes: u64::MAX,
            max_age_ms: 90 * 60 * 1000,
        };
        let swept = sweep(root.path(), &BTreeSet::new(), by_age, now).expect("sweep");
        // b2 and l1 are older than 90 minutes and not their directory's newest.
        assert_eq!(swept.files, 2);
        assert!(!b2.exists() && !l1.exists());
        assert!(
            a2.exists() && b3.exists() && l2.exists(),
            "newest per directory stays"
        );
    }
}
