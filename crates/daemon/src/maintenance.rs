//! Background maintenance.
//!
//! Log retention: `logs.retention.max_size` bounds every unit directory together,
//! live and finished. Each pass removes the oldest data first across all of them.
//! Finished directories lose their oldest segments and set-aside `*.corrupt` files;
//! live units are asked to trim themselves on their log thread, which owns the
//! writer. A live unit's active segment goes only when it holds more than
//! `LIVE_FLOOR` bytes, so a running unit keeps its latest output when the budget
//! is small. Directories whose writer lock is held by someone the pass does not
//! know about (a unit still starting, a run still closing) count toward the total
//! but are left alone. `logs.retention.days` applies to finished directories here;
//! live units apply it themselves as they write.
//!
//! State retention: state.db history and its size budget, from `history`,
//! `metrics`, `logs` and `state` in the user config.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use devlanes_config::UserConfig;
use devlanes_logs::{SegmentMetadata, UnitLog};
use devlanes_state::{PruneReport, RetentionPolicy};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    engine::Inner,
    model::lock,
    units::{LogRetention, UnitHost},
};

/// How often the log pass runs.
const INTERVAL: Duration = Duration::from_secs(600);
/// A live unit's active segment up to this size is never removed for the total
/// budget; a larger one goes only after everything older.
const LIVE_FLOOR: u64 = 1 << 20;
/// The lock file a log writer holds in its unit directory (`devlanes_logs`).
const WRITER_LOCK: &str = ".writer.lock";
/// The first state pass waits this long, so it never competes with startup.
const STATE_FIRST: Duration = Duration::from_secs(60);
/// How often the state pass runs afterwards.
const STATE_INTERVAL: Duration = Duration::from_secs(3600);
/// The next state pass after one stopped at its batch cap over budget.
const STATE_MORE: Duration = Duration::from_secs(5);

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
    /// A pass that hit its batch cap over budget is followed after a few seconds.
    pub(crate) async fn state_maintenance_loop(self: Arc<Self>) {
        let mut wait = STATE_FIRST;
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            let now = i64::try_from(crate::units::now_ms()).unwrap_or(i64::MAX);
            wait = match self.prune_state(now).await {
                Ok(report) if report.more => STATE_MORE,
                Ok(_) => STATE_INTERVAL,
                Err(error) => {
                    tracing::warn!(%error, "state retention failed");
                    STATE_INTERVAL
                }
            };
        }
    }
}

/// What one pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Swept {
    /// Files the pass deleted itself: finished segments and `*.corrupt` files.
    pub files: usize,
    /// Bytes freed, including what live units trimmed.
    pub bytes: u64,
    /// Live units that trimmed their log.
    pub live_units: usize,
}

/// A running unit's log as the pass sees it. Only the unit's log thread may delete
/// its segments, so the pass asks it instead of removing files.
pub(crate) trait LiveLog {
    /// The unit directory.
    fn dir(&self) -> &Path;
    /// Segment metadata, oldest first with the active segment last, or `None` once
    /// the log is closed and the directory counts as finished.
    fn segments(&self) -> Option<Vec<SegmentMetadata>>;
    /// Has the writer apply [`release`]; the bytes left, or `None` when it failed or
    /// did not answer in time.
    fn release(&self, bytes: u64, close_active: bool) -> Option<u64>;
}

/// Frees `bytes` from a live log, oldest segments first, and returns the bytes left.
/// The active segment stays unless `close_active`, even when output written since
/// the pass planned this means less is freed. Runs on the thread owning the writer.
pub(crate) fn release(
    log: &mut UnitLog,
    bytes: u64,
    close_active: bool,
) -> devlanes_logs::Result<u64> {
    let active = if close_active {
        0
    } else {
        log.segments()
            .last()
            .filter(|segment| !segment.closed)
            .map_or(0, |segment| segment.bytes)
    };
    log.trim_to(log.disk_bytes().saturating_sub(bytes).max(active))
}

/// A file the pass may delete: a segment with its sidecar index, or a `*.corrupt` file.
#[derive(Debug)]
struct Removable {
    path: PathBuf,
    sidecar: Option<PathBuf>,
    bytes: u64,
    modified: SystemTime,
}

/// The log files of one unit directory.
#[derive(Debug, Default)]
struct Listing {
    /// Oldest first, with their first sequence.
    segments: Vec<(u64, Removable)>,
    corrupt: Vec<Removable>,
}

impl Listing {
    /// Bytes of every file listed.
    fn bytes(&self) -> u64 {
        self.segments
            .iter()
            .map(|(_, file)| file)
            .chain(&self.corrupt)
            .fold(0, |sum, file| sum.saturating_add(file.bytes))
    }
}

/// Segment sequence from `<seq:020>.log` or `<seq:020>.log.zst`.
fn segment_seq(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(".log.zst")
        .or_else(|| name.strip_suffix(".log"))?;
    (stem.len() == 20).then(|| stem.parse().ok()).flatten()
}

/// True for files the log writer set aside as damaged: `<seq:020>.<kind>.corrupt`.
fn is_corrupt(name: &str) -> bool {
    name.strip_suffix(".corrupt").is_some_and(|stem| {
        let bytes = stem.as_bytes();
        bytes.len() > 21 && bytes[20] == b'.' && bytes[..20].iter().all(u8::is_ascii_digit)
    })
}

/// Lists the log files of one unit directory.
fn list(dir: &Path) -> io::Result<Listing> {
    let mut listing = Listing::default();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let seq = segment_seq(name);
        if seq.is_none() && !is_corrupt(name) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            // A live writer compressed or retention removed it after the listing.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            continue;
        }
        let file = Removable {
            path: entry.path(),
            sidecar: seq.map(|seq| dir.join(format!("{seq:020}.meta.json"))),
            bytes: metadata.len(),
            modified: metadata.modified()?,
        };
        match seq {
            Some(seq) => listing.segments.push((seq, file)),
            None => listing.corrupt.push(file),
        }
    }
    listing.segments.sort_by_key(|(seq, _)| *seq);
    Ok(listing)
}

/// Every directory below `root` that holds segment or `*.corrupt` files. One that
/// cannot be read, often because `lane clean` removed it during the walk, is skipped.
fn unit_dirs(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return skipped(root, &error),
    };
    let mut has_logs = false;
    for entry in entries {
        let (entry, kind) =
            match entry.and_then(|entry| entry.file_type().map(|kind| (entry, kind))) {
                Ok(found) => found,
                Err(error) => {
                    skipped(root, &error);
                    continue;
                }
            };
        if kind.is_dir() {
            unit_dirs(&entry.path(), out);
        } else if kind.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| segment_seq(name).is_some() || is_corrupt(name))
        {
            has_logs = true;
        }
    }
    if has_logs {
        out.push(root.to_path_buf());
    }
}

/// Warns about a directory the pass could not use, unless it was just removed.
fn skipped(dir: &Path, error: &io::Error) {
    if error.kind() != io::ErrorKind::NotFound {
        tracing::warn!(dir = %dir.display(), %error, "log retention skipped a directory");
    }
}

/// A finished directory's writer lock, as the pass found it.
enum DirLock {
    /// A writer holds it: a unit still starting, or a run still closing its log.
    Busy,
    /// Held by the pass until dropped, so no writer opens the directory meanwhile.
    /// `None` when the directory has no lock file, so no writer ever opened it.
    Free(Option<File>),
}

/// Takes `dir`'s writer lock without waiting. The lock file is never created, so a
/// directory that `lane clean` is removing gains no entry.
fn lock_dir(dir: &Path) -> io::Result<DirLock> {
    match File::open(dir.join(WRITER_LOCK)) {
        Ok(file) => match file.try_lock() {
            Ok(()) => Ok(DirLock::Free(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(DirLock::Busy),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DirLock::Free(None)),
        Err(error) => Err(error),
    }
}

/// Removes a file and its sidecar index; false when someone removed it first.
fn remove(file: &Removable) -> io::Result<bool> {
    let removed = match crate::files::remove_file(&file.path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if let Some(sidecar) = &file.sidecar
        && let Err(error) = crate::files::remove_file(sidecar)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error);
    }
    Ok(removed)
}

/// Something the pass may free, keyed by when its data was written.
struct Candidate {
    key: SystemTime,
    bytes: u64,
    target: Target,
}

/// How a [`Candidate`] is freed.
enum Target {
    /// Deleted by the pass; `dir` indexes the directory list.
    File { dir: usize, file: Removable },
    /// Released by live unit `unit`; `active` for its open segment.
    Live { unit: usize, active: bool },
}

/// Applies `retention` to every unit directory below `root` and to the `live` logs,
/// which may lie anywhere (normally below `root`). Blocking. A directory that cannot
/// be read, locked or cleaned is skipped with a warning, so it never ends the pass.
pub(crate) fn sweep<L: LiveLog>(
    root: &Path,
    live: &[L],
    retention: LogRetention,
    now: SystemTime,
) -> Swept {
    let mut dirs = Vec::new();
    unit_dirs(root, &mut dirs);
    sweep_dirs(dirs, live, retention, now)
}

/// [`sweep`] over listed unit directories.
///
/// Policy: everything counts toward `retention.max_bytes`, and the oldest data goes
/// first wherever it lies (file mtime for finished files, newest record for live
/// segments). Kept regardless: each finished directory's newest segment, which
/// carries the sequence number the next run continues from; a live unit's active
/// segment up to `LIVE_FLOOR`; and directories another writer holds locked. Age
/// expiry applies to the files the pass deletes itself.
fn sweep_dirs<L: LiveLog>(
    paths: Vec<PathBuf>,
    live: &[L],
    retention: LogRetention,
    now: SystemTime,
) -> Swept {
    // Asked first, so a unit whose log closed meanwhile is swept as finished.
    let units: Vec<(&L, Vec<SegmentMetadata>)> = live
        .iter()
        .filter_map(|log| Some((log, log.segments()?)))
        .collect();
    let live_dirs: BTreeSet<&Path> = units.iter().map(|(log, _)| log.dir()).collect();
    let mut total = units
        .iter()
        .flat_map(|(_, segments)| segments)
        .fold(0u64, |sum, segment| sum.saturating_add(segment.bytes));
    let mut dirs = Vec::new();
    let mut candidates = Vec::new();
    for path in paths {
        let live = live_dirs.contains(path.as_path());
        let busy = if live {
            false
        } else {
            match lock_dir(&path) {
                Ok(lock) => matches!(lock, DirLock::Busy),
                Err(error) => {
                    skipped(&path, &error);
                    continue;
                }
            }
        };
        let mut listing = match list(&path) {
            Ok(listing) => listing,
            Err(error) => {
                skipped(&path, &error);
                continue;
            }
        };
        if live {
            // The writer's metadata already counted its segments, buffered bytes too.
            listing.segments.clear();
        }
        total = total.saturating_add(listing.bytes());
        if busy {
            continue;
        }
        if let Some(&(newest, _)) = listing.segments.last() {
            listing.segments.retain(|(seq, _)| *seq != newest);
        }
        let dir = dirs.len();
        let files = listing.segments.into_iter().map(|(_, file)| file);
        candidates.extend(files.chain(listing.corrupt).map(|file| Candidate {
            key: file.modified,
            bytes: file.bytes,
            target: Target::File { dir, file },
        }));
        dirs.push((path, live));
    }
    for (unit, (_, segments)) in units.iter().enumerate() {
        let mut key = UNIX_EPOCH;
        for segment in segments {
            // Kept monotonic, because the writer frees oldest first whatever the clock did.
            key = UNIX_EPOCH
                .checked_add(Duration::from_millis(segment.max_timestamp_ms))
                .map_or(key, |at| key.max(at));
            let active = !segment.closed;
            if segment.bytes == 0 || (active && segment.bytes <= LIVE_FLOOR) {
                continue;
            }
            candidates.push(Candidate {
                key,
                bytes: segment.bytes,
                target: Target::Live { unit, active },
            });
        }
    }
    // Stable, so a unit's segments with equal keys stay in sequence order.
    candidates.sort_by_key(|candidate| candidate.key);

    let max_age = Duration::from_millis(retention.max_age_ms);
    let mut doomed: BTreeMap<usize, Vec<Removable>> = BTreeMap::new();
    let mut releases = vec![(0u64, false); units.len()];
    for candidate in candidates {
        let expired = match &candidate.target {
            Target::File { file, .. } => now
                .duration_since(file.modified)
                .is_ok_and(|age| age > max_age),
            Target::Live { .. } => false,
        };
        if !expired && total <= retention.max_bytes {
            continue;
        }
        total = total.saturating_sub(candidate.bytes);
        match candidate.target {
            Target::File { dir, file } => doomed.entry(dir).or_default().push(file),
            Target::Live { unit, active } => {
                releases[unit].0 += candidate.bytes;
                releases[unit].1 |= active;
            }
        }
    }

    let mut swept = Swept::default();
    for (dir, files) in doomed {
        let (path, live) = &dirs[dir];
        // A live directory's lock belongs to its unit, whose writer never touches
        // `*.corrupt` files; every other directory is locked again for the deletes.
        let _lock = if *live {
            None
        } else {
            match lock_dir(path) {
                Ok(DirLock::Free(lock)) => lock,
                Ok(DirLock::Busy) => continue,
                Err(error) => {
                    skipped(path, &error);
                    continue;
                }
            }
        };
        for file in files {
            match remove(&file) {
                Ok(true) => {
                    swept.files += 1;
                    swept.bytes += file.bytes;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(path = %file.path.display(), %error, "could not remove a log file");
                }
            }
        }
    }
    for ((log, segments), (bytes, close_active)) in units.iter().zip(releases) {
        if bytes == 0 {
            continue;
        }
        let before = segments
            .iter()
            .fold(0u64, |sum, segment| sum.saturating_add(segment.bytes));
        if let Some(left) = log.release(bytes, close_active) {
            swept.bytes += before.saturating_sub(left);
            swept.live_units += 1;
        }
    }
    swept
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
            // Every registered host, exited ones too: a log still closing answers as
            // live, and a closed one reports so and is swept as finished.
            let live: Vec<UnitHost> = self
                .all_lanes()
                .iter()
                .flat_map(|(_, lane)| {
                    lane.st()
                        .units
                        .values()
                        .map(|unit| unit.host.clone())
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
                Ok(swept) if swept.bytes > 0 || swept.files > 0 => {
                    tracing::info!(
                        files = swept.files,
                        bytes = swept.bytes,
                        live_units = swept.live_units,
                        "removed old logs"
                    );
                }
                Ok(_) => {}
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

    const HOUR: Duration = Duration::from_secs(3600);

    /// A live log owned by the test thread, standing in for a unit's log thread.
    struct TestLive {
        dir: PathBuf,
        log: std::sync::Mutex<UnitLog>,
    }

    impl LiveLog for TestLive {
        /// The unit directory.
        fn dir(&self) -> &Path {
            &self.dir
        }

        /// The writer's metadata.
        fn segments(&self) -> Option<Vec<SegmentMetadata>> {
            Some(self.log.lock().ok()?.segments())
        }

        /// Applies [`release`] directly.
        fn release(&self, bytes: u64, close_active: bool) -> Option<u64> {
            let mut log = self.log.lock().ok()?;
            super::release(&mut log, bytes, close_active).ok()
        }
    }

    /// No live units.
    const NONE: &[TestLive] = &[];

    /// Milliseconds since the epoch.
    fn ms(at: SystemTime) -> u64 {
        u64::try_from(at.duration_since(UNIX_EPOCH).expect("clock").as_millis()).expect("ms")
    }

    /// Opens a live log in `dir` with one line per entry of `ages`; lines an hour or
    /// more apart land in separate segments, the last one active.
    fn live(dir: &Path, ages: &[Duration], now: SystemTime) -> TestLive {
        let options = devlanes_logs::LogOptions {
            compress: false,
            retention_bytes: u64::MAX,
            retention_age_ms: u64::MAX,
            ..devlanes_logs::LogOptions::default()
        };
        let mut log = UnitLog::open(dir, "web", options).expect("open");
        for age in ages {
            log.ingest(b"some output\n", ms(now - *age))
                .expect("ingest");
        }
        TestLive {
            dir: dir.to_owned(),
            log: std::sync::Mutex::new(log),
        }
    }

    /// Segment bytes of a live log, oldest first.
    fn sizes(log: &TestLive) -> Vec<u64> {
        log.segments()
            .expect("live")
            .iter()
            .map(|segment| segment.bytes)
            .collect()
    }

    /// Writes a file of `bytes` whose mtime is `age` in the past.
    fn file(path: &Path, bytes: usize, age: Duration, now: SystemTime) -> PathBuf {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, vec![b'x'; bytes]).expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open");
        file.set_modified(now - age).expect("mtime");
        path.to_owned()
    }

    /// Writes a segment of `bytes` whose mtime is `age` in the past.
    fn segment(dir: &Path, seq: u64, bytes: usize, age: Duration, now: SystemTime) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        std::fs::write(dir.join(format!("{seq:020}.meta.json")), "{}").expect("meta");
        file(&dir.join(format!("{seq:020}.log.zst")), bytes, age, now)
    }

    /// Holds `dir`'s writer lock the way a starting unit's `UnitLog::open` does.
    fn hold_lock(dir: &Path) -> File {
        let lock = File::create(dir.join(WRITER_LOCK)).expect("lock file");
        lock.try_lock().expect("lock");
        lock
    }

    #[test]
    /// The size bound spans every unit directory and removes the oldest segments
    /// first. Each directory's newest segment stays, and a directory whose writer
    /// lock is held (a unit starting but not registered yet) counts toward the
    /// total but is never touched, even though it holds the oldest segment.
    fn size_and_age_across_run_directories() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let a = root.path().join("acme/0/api");
        let b = root.path().join("acme/1/api");
        let starting = root.path().join("acme/0/web");
        let a1 = segment(&a, 1, 100, HOUR * 10, now);
        let a2 = segment(&a, 50, 100, HOUR * 3, now);
        let b1 = segment(&b, 1, 100, HOUR * 8, now);
        let b2 = segment(&b, 20, 100, HOUR * 2, now);
        let b3 = segment(&b, 40, 100, HOUR, now);
        let l1 = segment(&starting, 1, 100, HOUR * 20, now);
        let l2 = segment(&starting, 9, 100, HOUR, now);
        std::fs::write(root.path().join("daemon.log"), vec![b'x'; 1000]).expect("daemon log");
        let lock = hold_lock(&starting);
        let retention = LogRetention {
            max_bytes: 500,
            max_age_ms: u64::MAX,
        };
        let swept = sweep(root.path(), NONE, retention, now);
        // 700 bytes in segments: the two oldest unlocked ones (a1, b1) go.
        assert_eq!(
            swept,
            Swept {
                files: 2,
                bytes: 200,
                live_units: 0,
            }
        );
        assert!(!a1.exists() && !b1.exists());
        assert!(!a.join(format!("{:020}.meta.json", 1)).exists());
        for kept in [&a2, &b2, &b3, &l1, &l2] {
            assert!(kept.exists(), "{}", kept.display());
        }
        assert!(root.path().join("daemon.log").exists());

        drop(lock);
        let by_age = LogRetention {
            max_bytes: u64::MAX,
            max_age_ms: 90 * 60 * 1000,
        };
        let swept = sweep(root.path(), NONE, by_age, now);
        // b2 and l1 are older than 90 minutes and not their directory's newest.
        assert_eq!(swept.files, 2);
        assert!(!b2.exists() && !l1.exists());
        assert!(
            a2.exists() && b3.exists() && l2.exists(),
            "newest per directory stays"
        );
    }

    #[test]
    /// Two live units and a finished one share one budget: the oldest data goes
    /// first wherever it lies, live units free theirs through `release`, and their
    /// small active segments survive even a zero budget.
    fn budget_spans_live_and_finished_units() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let minute = Duration::from_secs(60);
        let finished = root.path().join("acme/0/worker");
        let f1 = segment(&finished, 1, 100, HOUR * 10, now);
        let f2 = segment(&finished, 2, 100, HOUR * 9, now);
        let f3 = segment(&finished, 3, 100, HOUR, now);
        let a = live(
            &root.path().join("acme/0/api"),
            &[HOUR * 7, HOUR * 6, HOUR * 5, minute],
            now,
        );
        let b = live(
            &root.path().join("acme/0/web"),
            &[HOUR * 8, HOUR * 4, minute * 2],
            now,
        );
        let (a_sizes, b_sizes) = (sizes(&a), sizes(&b));
        assert_eq!(
            (a_sizes.len(), b_sizes.len()),
            (4, 3),
            "one segment per line"
        );
        let total = 300 + a_sizes.iter().sum::<u64>() + b_sizes.iter().sum::<u64>();
        // Oldest first: f1 (10h), f2 (9h), b's first (8h), a's first (7h).
        let freed = 200 + b_sizes[0] + a_sizes[0];
        let retention = LogRetention {
            max_bytes: total - freed,
            max_age_ms: u64::MAX,
        };
        let units = [a, b];
        let swept = sweep(root.path(), &units, retention, now);
        assert_eq!(
            swept,
            Swept {
                files: 2,
                bytes: freed,
                live_units: 2,
            }
        );
        assert!(!f1.exists() && !f2.exists() && f3.exists());
        assert_eq!(sizes(&units[0]), a_sizes[1..]);
        assert_eq!(sizes(&units[1]), b_sizes[1..]);

        let nothing = LogRetention {
            max_bytes: 0,
            max_age_ms: u64::MAX,
        };
        sweep(root.path(), &units, nothing, now);
        assert!(f3.exists(), "the newest finished segment stays");
        assert_eq!(sizes(&units[0]), a_sizes[3..], "active segment stays");
        assert_eq!(sizes(&units[1]), b_sizes[2..], "active segment stays");
    }

    #[test]
    /// A directory that disappears between the walk and its listing, as one removed
    /// by `lane clean` does, is skipped and the rest of the pass still runs.
    fn removed_directory_does_not_stop_the_pass() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let gone = root.path().join("acme/0/api");
        let kept = root.path().join("acme/1/api");
        segment(&gone, 1, 100, HOUR * 9, now);
        let old = segment(&kept, 1, 100, HOUR * 8, now);
        let newest = segment(&kept, 2, 100, HOUR, now);
        let mut dirs = Vec::new();
        unit_dirs(root.path(), &mut dirs);
        dirs.sort();
        assert_eq!(dirs, [gone.clone(), kept.clone()]);
        std::fs::remove_dir_all(&gone).expect("remove");
        let retention = LogRetention {
            max_bytes: 100,
            max_age_ms: u64::MAX,
        };
        let swept = sweep_dirs(dirs, NONE, retention, now);
        assert_eq!(swept.files, 1);
        assert!(!old.exists() && newest.exists());

        // A walk that meets missing directories finds nothing and does not fail.
        unit_dirs(&root.path().join("missing"), &mut Vec::new());
    }

    #[test]
    /// Segments the writer set aside as `*.corrupt` count toward the budget and go
    /// oldest first, by size and by age, in finished and live directories.
    fn corrupt_files_count_and_go() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let dir = root.path().join("acme/0/api");
        let corrupt = file(
            &dir.join(format!("{:020}.log.corrupt", 1)),
            300,
            HOUR * 5,
            now,
        );
        let corrupt_meta = file(
            &dir.join(format!("{:020}.meta.json.corrupt", 1)),
            50,
            HOUR * 4,
            now,
        );
        let newest = segment(&dir, 9, 100, HOUR, now);
        let unit = live(&root.path().join("acme/0/web"), &[Duration::ZERO], now);
        let live_corrupt = file(
            &unit.dir.join(format!("{:020}.log.zst.corrupt", 1)),
            100,
            HOUR * 30,
            now,
        );
        let unrelated = file(&dir.join("notes.corrupt"), 10, HOUR * 30, now);

        let retention = LogRetention {
            max_bytes: 200 + sizes(&unit).iter().sum::<u64>(),
            max_age_ms: u64::MAX,
        };
        let swept = sweep(root.path(), std::slice::from_ref(&unit), retention, now);
        // 550 bytes of files: the live directory's corrupt file (oldest) and then
        // the finished one's segment go; its smaller metadata file fits.
        assert_eq!(swept.files, 2);
        assert_eq!(swept.bytes, 400);
        assert!(!live_corrupt.exists() && !corrupt.exists());
        assert!(corrupt_meta.exists() && newest.exists() && unrelated.exists());

        let by_age = LogRetention {
            max_bytes: u64::MAX,
            max_age_ms: 2 * 3_600_000,
        };
        let swept = sweep(root.path(), std::slice::from_ref(&unit), by_age, now);
        assert_eq!(swept.files, 1);
        assert!(!corrupt_meta.exists() && newest.exists());
    }
}
