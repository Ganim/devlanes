//! Box-to-client folder sync (plan 14.5): for every `sync` entry of a running box
//! lane, a subscription streams what differs from this machine's copy and then
//! changes. Files land in `$DEVLANES_HOME/sync/<host>/<project>/<lane>/<leaf>`, a
//! devlanes-owned directory; paths from the box pass [`RelPath`] validation before
//! anything touches the disk, and files stream into temp files instead of memory.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, UNIX_EPOCH},
};

use devlanes_proto::remote::{MAX_SYNC_FILE, SyncEntry, SyncFile, SyncManifest, SyncRequest};
use devlanes_sync::{HashCache, IgnoreRules, LocalTarget, PartialFile, RelPath, SyncTarget};
use devlanes_transport::{Session as BoxSession, SyncMessage};
use serde_json::json;
use tokio::{task::JoinHandle, time::Instant};

use super::{HostConn, now_ms, sync_destination};
use crate::{engine::Inner, model::lock};

/// Quiet period after a registry change before the entries are listed again.
const RELIST_DELAY: Duration = Duration::from_millis(500);

/// Entries followed per box; the box enforces its own limit too.
const MAX_FOLLOWED: usize = 16;

/// First and longest wait before a failed subscription is retried.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Files listed in a subscription's manifest; past this the request would near the
/// transport's frame limit, so the box sends everything instead.
const MAX_MANIFEST_FILES: usize = 20_000;

/// A followed entry and when to retry it after a failure.
struct Followed {
    task: JoinHandle<bool>,
    /// Set once the task ended: whether it failed.
    failed: Option<bool>,
    delay: Duration,
    retry_at: Option<Instant>,
}

/// Follows the box's sync entries for one session; ends with the session.
pub(super) async fn run(engine: Weak<Inner>, conn: Arc<HostConn>, session: BoxSession) {
    let Some(home) = engine.upgrade().map(|inner| inner.opts.home.clone()) else {
        return;
    };
    let mut registry = conn.registry.subscribe();
    let mut followed: BTreeMap<String, Followed> = BTreeMap::new();
    let mut entries: Vec<SyncEntry> = Vec::new();
    let mut relist = true;
    loop {
        if relist {
            match list(&session).await {
                Ok(listed) => entries = listed,
                Err(error) => tracing::debug!(host = %conn.name, %error, "cannot list box syncs"),
            }
            relist = false;
            // An entry whose stream ended cleanly starts again if it is still listed.
            followed.retain(|_, state| state.failed != Some(false));
        }
        for state in followed.values_mut() {
            if state.failed.is_none() && state.task.is_finished() {
                let failed = (&mut state.task).await.unwrap_or(true);
                state.failed = Some(failed);
                if failed {
                    state.retry_at = Some(Instant::now() + state.delay);
                    state.delay = (state.delay * 2).min(RETRY_MAX);
                }
            }
        }
        let keys: BTreeSet<String> = entries.iter().map(key).collect();
        followed.retain(|key, entry| {
            let keep = keys.contains(key);
            if !keep {
                entry.task.abort();
            }
            keep
        });
        lock(&conn.syncs).retain(|key, _| keys.contains(key));
        let now = Instant::now();
        for (position, entry) in entries.iter().enumerate() {
            let key = key(entry);
            if position >= MAX_FOLLOWED {
                let mut status = Status::new(entry, &PathBuf::new());
                status.fail(format!(
                    "not followed: at most {MAX_FOLLOWED} syncs per box"
                ));
                status.publish(&conn, &key);
                continue;
            }
            // A failure waits for its backoff; a clean end waits for the next listing.
            let start = match followed.get(&key) {
                None => true,
                Some(state) => {
                    state.failed == Some(true) && state.retry_at.is_some_and(|at| at <= now)
                }
            };
            if start {
                let delay = followed.get(&key).map_or(RETRY_FIRST, |state| state.delay);
                let task = tokio::spawn(follow(
                    conn.clone(),
                    session.clone(),
                    entry.clone(),
                    home.clone(),
                ));
                followed.insert(
                    key,
                    Followed {
                        task,
                        failed: None,
                        delay,
                        retry_at: None,
                    },
                );
            }
        }
        let next_retry = followed.values().filter_map(|state| state.retry_at).min();
        let finished = async {
            loop {
                if followed
                    .values()
                    .any(|state| state.failed.is_none() && state.task.is_finished())
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::select! {
            _ = session.closed() => break,
            () = conn.cancel.cancelled() => break,
            changed = registry.changed() => {
                if changed.is_err() {
                    break;
                }
                tokio::time::sleep(RELIST_DELAY).await;
                relist = true;
            }
            () = sleep_until(next_retry) => {}
            () = finished => {}
        }
    }
    for state in followed.into_values() {
        state.task.abort();
    }
}

/// Sleeps until `deadline`, or forever when there is none.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Status key of an entry.
fn key(entry: &SyncEntry) -> String {
    format!("{}:{}#{}", entry.lane, entry.unit, entry.index)
}

/// Lists the box's entries over a `list` sync subscription.
async fn list(session: &BoxSession) -> Result<Vec<SyncEntry>, String> {
    let request = serde_json::to_value(SyncRequest::List).map_err(|e| e.to_string())?;
    let mut subscription = session
        .subscribe_sync(request)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(message) = subscription.next().await {
        if let SyncMessage::Manifest(value) = message.map_err(|e| e.to_string())?
            && let Ok(SyncManifest::Entries { entries }) = serde_json::from_value(value)
        {
            return Ok(entries);
        }
    }
    Err("the box sent no sync entries".into())
}

/// What this machine holds for an entry, from the cached hashes.
fn have_files(cache: &HashCache) -> Vec<SyncFile> {
    let manifest = cache.manifest();
    if manifest.len() > MAX_MANIFEST_FILES {
        return Vec::new();
    }
    manifest
        .into_iter()
        .map(|(rel, entry)| SyncFile {
            mtime_ms: cache
                .mtime(&rel)
                .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                .and_then(|age| u64::try_from(age.as_millis()).ok()),
            path: rel.as_str().to_owned(),
            size: entry.size,
            mode: entry.mode,
            hash: entry.hash.to_hex(),
        })
        .collect()
}

/// One entry's subscription: applies files until the stream ends. Returns true
/// when it failed and should be retried.
async fn follow(conn: Arc<HostConn>, session: BoxSession, entry: SyncEntry, home: PathBuf) -> bool {
    let key = key(&entry);
    let destination = match sync_destination(&home, &conn.name, &entry) {
        Ok(destination) => destination,
        Err(error) => {
            let mut status = Status::new(&entry, &PathBuf::new());
            status.fail(error);
            status.publish(&conn, &key);
            return false;
        }
    };
    let mut status = Status::new(&entry, &destination);
    status.publish(&conn, &key);
    let target = match LocalTarget::owned(&destination).await {
        Ok(target) => target,
        Err(error) => {
            status.fail(error.to_string());
            status.publish(&conn, &key);
            return true;
        }
    };
    // Hashes are reused while size and mtime match, so reconnects stay cheap.
    let cached = lock(&conn.hashes).remove(&key).unwrap_or_default();
    let cache = match target.manifest_cached(cached).await {
        Ok(cache) => cache,
        Err(error) => {
            status.fail(error.to_string());
            status.publish(&conn, &key);
            return true;
        }
    };
    let request = SyncRequest::Follow {
        lane: entry.lane.clone(),
        unit: entry.unit.clone(),
        index: entry.index,
        have: have_files(&cache),
    };
    lock(&conn.hashes).insert(key.clone(), cache);
    let subscription = match serde_json::to_value(&request) {
        Ok(request) => session.subscribe_sync(request).await,
        Err(error) => {
            status.fail(error.to_string());
            status.publish(&conn, &key);
            return true;
        }
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            status.fail(error.to_string());
            status.publish(&conn, &key);
            return true;
        }
    };
    let rules = IgnoreRules::new(&entry.ignore).unwrap_or_else(|_| IgnoreRules::none());
    let mut incoming: Option<Incoming> = None;
    // The file whose chunks are being skipped, so one bad file is one issue.
    let mut rejected: Option<String> = None;
    let mut failed = false;
    while let Some(message) = subscription.next().await {
        match message {
            Ok(SyncMessage::Manifest(value)) => match serde_json::from_value(value) {
                Ok(SyncManifest::File { path, mode, size }) => {
                    incoming = None;
                    rejected = None;
                    if size > MAX_SYNC_FILE {
                        status.issue(format!("{path}: {size} bytes is over the sync limit"));
                        rejected = Some(path);
                        continue;
                    }
                    let begun = match RelPath::new(path.clone()) {
                        Ok(rel) => target
                            .begin_file(&rel, mode)
                            .await
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    match begun {
                        Ok(file) => incoming = Some(Incoming { path, size, file }),
                        Err(error) => {
                            status.issue(error);
                            rejected = Some(path);
                        }
                    }
                }
                Ok(SyncManifest::Ready { files }) => {
                    status.pruned += prune(&target, &rules, &files).await;
                    status.state = "watching";
                }
                Ok(SyncManifest::Entries { .. }) => {}
                Err(error) => status.issue(format!("unreadable manifest: {error}")),
            },
            Ok(SyncMessage::Chunk {
                path,
                offset,
                data,
                last,
            }) => {
                if rejected.as_deref() == Some(path.as_str()) {
                    continue;
                }
                let Some(file) = incoming.as_mut().filter(|file| file.path == path) else {
                    status.issue(format!("{path}: data without a file header"));
                    rejected = Some(path);
                    continue;
                };
                let written = file.file.written();
                if offset != written || written + data.len() as u64 > file.size {
                    status.issue(format!(
                        "{path}: data out of order or longer than announced"
                    ));
                    incoming = None;
                    rejected = Some(path);
                    continue;
                }
                if let Err(error) = file.file.append(data).await {
                    status.issue(error.to_string());
                    incoming = None;
                    rejected = Some(path);
                    continue;
                }
                if last && let Some(done) = incoming.take() {
                    match done.file.commit().await {
                        Ok(()) => status.written += 1,
                        Err(error) => status.issue(error.to_string()),
                    }
                }
            }
            Ok(SyncMessage::Delete { path }) => match RelPath::new(path) {
                Ok(rel) => match target.remove(&rel).await {
                    Ok(()) => status.removed += 1,
                    Err(error) => status.issue(error.to_string()),
                },
                Err(error) => status.issue(error.to_string()),
            },
            Err(error) => {
                status.fail(error.to_string());
                failed = true;
                break;
            }
        }
        status.publish(&conn, &key);
    }
    if !failed {
        status.state = "stopped";
    }
    status.publish(&conn, &key);
    failed
}

/// A file being received into a temp file.
struct Incoming {
    path: String,
    size: u64,
    file: PartialFile,
}

/// Removes destination files the box's initial pass did not leave there, keeping
/// ignored ones, which belong to this machine. Lists names only; nothing is hashed.
async fn prune(target: &LocalTarget, rules: &IgnoreRules, files: &[String]) -> u64 {
    let keep: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let Ok(present) = target.files().await else {
        return 0;
    };
    let mut removed = 0;
    for rel in present {
        if keep.contains(rel.as_str()) || rules.is_ignored(&rel) {
            continue;
        }
        if target.remove(&rel).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Per-entry status shown by `remote ls` and `remote sync`.
struct Status {
    lane: String,
    unit: String,
    index: usize,
    from: PathBuf,
    to: PathBuf,
    state: &'static str,
    written: u64,
    removed: u64,
    pruned: u64,
    errors: Vec<String>,
    error: Option<String>,
    updated_ms: u64,
}

impl Status {
    /// A syncing entry.
    fn new(entry: &SyncEntry, to: &std::path::Path) -> Self {
        Self {
            lane: entry.lane.clone(),
            unit: entry.unit.clone(),
            index: entry.index,
            from: entry.from.clone(),
            to: to.to_path_buf(),
            state: "syncing",
            written: 0,
            removed: 0,
            pruned: 0,
            errors: Vec::new(),
            error: None,
            updated_ms: now_ms(),
        }
    }

    /// Records a per-file problem; the last 16 are kept.
    fn issue(&mut self, message: String) {
        tracing::debug!(lane = %self.lane, unit = %self.unit, %message, "sync issue");
        if self.errors.len() == 16 {
            self.errors.remove(0);
        }
        self.errors.push(message);
    }

    /// Marks the subscription failed.
    fn fail(&mut self, message: String) {
        self.state = "failed";
        self.error = Some(message);
    }

    /// Stores the status on the host.
    fn publish(&mut self, conn: &HostConn, key: &str) {
        self.updated_ms = now_ms();
        let value = json!({
            "lane": self.lane,
            "unit": self.unit,
            "index": self.index,
            "from": self.from,
            "to": self.to,
            "state": self.state,
            "written": self.written,
            "removed": self.removed,
            "pruned": self.pruned,
            "errors": self.errors,
            "error": self.error,
            "updated_ms": self.updated_ms,
        });
        lock(&conn.syncs).insert(key.to_owned(), value);
    }
}
