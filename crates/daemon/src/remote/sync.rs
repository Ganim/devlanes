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

use devlanes_proto::remote::{
    MANIFEST_BATCH, MAX_SYNC_FILE, ManifestBatch, SyncEntry, SyncFile, SyncManifest, SyncRequest,
};
use devlanes_sync::{CaseCollisions, HeldFile, LocalTarget, PartialFile, RelPath, SyncTarget};
use devlanes_transport::{Session as BoxSession, SyncMessage, SyncSubscription};
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
    let mut refused: BTreeMap<String, String> = BTreeMap::new();
    let mut relist = true;
    loop {
        if relist {
            match list(&session).await {
                Ok(listed) => {
                    entries = listed;
                    // Only after a listing: a failed one would drop every entry's state.
                    let keys: BTreeSet<String> = entries.iter().map(key).collect();
                    lock(&conn.names).retain(|key, _| keys.contains(key));
                    lock(&conn.hashes).retain(|key, _| keys.contains(key));
                }
                Err(error) => tracing::debug!(host = %conn.name, %error, "cannot list box syncs"),
            }
            relist = false;
            // An entry whose stream ended cleanly starts again if it is still listed.
            followed.retain(|_, state| state.failed != Some(false));
            refused = duplicate_leaves(&entries);
            for entry in entries
                .iter()
                .filter(|entry| refused.contains_key(&key(entry)))
            {
                let key = key(entry);
                let mut status = Status::new(entry, &PathBuf::new());
                status.fail(refused[&key].clone());
                status.publish(&conn, &key);
            }
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
            let keep = keys.contains(key) && !refused.contains_key(key);
            if !keep {
                entry.task.abort();
            }
            keep
        });
        lock(&conn.syncs).retain(|key, _| keys.contains(key));
        let now = Instant::now();
        let accepted = entries
            .iter()
            .filter(|entry| !refused.contains_key(&key(entry)));
        for (position, entry) in accepted.enumerate() {
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

/// Entries refused because another entry of the same lane has the same folder
/// name, by status key, with the message to show. The destination is
/// `<host>/<project>/<lane>/<leaf>`, so such entries would write into one folder
/// and prune each other's files. Replicas count as separate entries: a replicated
/// service syncs only when one replica has the entry or each names its folder
/// differently.
fn duplicate_leaves(entries: &[SyncEntry]) -> BTreeMap<String, String> {
    let mut groups: BTreeMap<(&str, &str, String), Vec<&SyncEntry>> = BTreeMap::new();
    for entry in entries {
        groups
            .entry((&entry.project, &entry.lane, leaf_key(&entry.leaf)))
            .or_default()
            .push(entry);
    }
    let mut refused = BTreeMap::new();
    for (_, group) in groups {
        if group.len() < 2 {
            continue;
        }
        let leaf = &group[0].leaf;
        let sources: Vec<String> = group
            .iter()
            .map(|entry| format!("{}:{} ({})", entry.lane, entry.unit, entry.from.display()))
            .collect();
        let message = format!(
            "not synced: {} sync folders with the same name `{leaf}` into one destination; rename one of them on the box",
            sources.join(", ")
        );
        for entry in group {
            refused.insert(key(entry), message.clone());
        }
    }
    refused
}

/// The destination folder a leaf names, as this file system compares names.
#[cfg(unix)]
fn leaf_key(leaf: &str) -> String {
    leaf.to_owned()
}

/// The destination folder a leaf names, as this file system compares names:
/// under its disk name and without letter case, so `dist` and `Dist` share one.
#[cfg(windows)]
fn leaf_key(leaf: &str) -> String {
    devlanes_sync::fold_case(&devlanes_core::paths::disk_name(leaf))
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

/// A destination file as a manifest entry; an unknown hash travels as empty.
fn sync_file(file: HeldFile) -> SyncFile {
    SyncFile {
        mtime_ms: file
            .mtime
            .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
            .and_then(|age| u64::try_from(age.as_millis()).ok()),
        path: file.path.as_str().to_owned(),
        size: file.size,
        mode: file.mode,
        hash: file.hash.map(|hash| hash.to_hex()).unwrap_or_default(),
    }
}

/// Sends the destination's manifest in batches while walking it, then the end
/// marker. Hashes come from the host's cache while size, mode and mtime match, and
/// the updated cache goes back to the host, also when the send stops early.
async fn send_manifest(
    conn: &HostConn,
    key: &str,
    target: &LocalTarget,
    subscription: &mut SyncSubscription,
    status: &mut Status,
    names: &mut CaseCollisions,
) -> Result<(), String> {
    let cached = lock(&conn.hashes).remove(key).unwrap_or_default();
    let mut walk = target.held(cached, MANIFEST_BATCH);
    let mut sent: Result<(), String> = Ok(());
    while let Some(batch) = walk.next().await {
        let count = batch.len() as u64;
        let batch = ManifestBatch {
            files: batch
                .into_iter()
                .map(|mut file| {
                    file.path = names.hold(&file.path);
                    sync_file(file)
                })
                .collect(),
        };
        let delivered = match serde_json::to_value(&batch) {
            Ok(value) => subscription.send(value).await.map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = delivered {
            sent = Err(error);
            break;
        }
        status.compared += count;
        status.publish(conn, key);
    }
    let cache = walk.finish().await;
    lock(&conn.hashes).insert(key.to_owned(), cache);
    sent?;
    names.end_listing();
    subscription.finish_input().await.map_err(|e| e.to_string())
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
    let _interrupted = Interrupted {
        conn: conn.clone(),
        key: key.clone(),
    };
    let target = match LocalTarget::owned(&destination).await {
        Ok(target) => target,
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
        read_only_modes: devlanes_sync::READ_ONLY_MODES,
    };
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
    // The box compares batch by batch and sends nothing before the end marker. A
    // send that fails mid-manifest is retried as a whole on a new subscription.
    status.state = "comparing";
    status.publish(&conn, &key);
    // Box paths that this file system would merge into one file (Windows
    // ignores letter case): the first one written wins, the others are skipped.
    let mut names = KeptNames::take(&conn, &key);
    if let Err(error) = send_manifest(
        &conn,
        &key,
        &target,
        &mut subscription,
        &mut status,
        &mut names.names,
    )
    .await
    {
        status.fail(format!("sending the manifest: {error}"));
        status.publish(&conn, &key);
        return true;
    }
    status.state = "syncing";
    let mut incoming: Option<Incoming> = None;
    // The file whose chunks are being skipped, so one bad file is one issue.
    let mut rejected: Option<String> = None;
    let mut failed = false;
    while let Some(message) = subscription.next().await {
        match message {
            Ok(SyncMessage::Manifest(value)) => match serde_json::from_value(value) {
                Ok(SyncManifest::File { path, mode, size }) => {
                    // A file the box never finished.
                    if let Some(unfinished) = incoming.take() {
                        unfinished.abandon(&mut names.names);
                    }
                    rejected = None;
                    if size > MAX_SYNC_FILE {
                        status.issue(format!("{path}: {size} bytes is over the sync limit"));
                        rejected = Some(path);
                        continue;
                    }
                    let begun = match RelPath::new(path.clone()) {
                        Ok(rel) => begin(&target, &mut names.names, rel, mode, size).await,
                        Err(error) => Err(error.to_string()),
                    };
                    match begun {
                        Ok(file) => incoming = Some(file),
                        Err(error) => {
                            status.issue(error);
                            rejected = Some(path);
                        }
                    }
                }
                Ok(SyncManifest::Ready) => status.state = "watching",
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
                let Some(file) = incoming.as_mut().filter(|file| file.rel.as_str() == path) else {
                    status.issue(format!("{path}: data without a file header"));
                    rejected = Some(path);
                    continue;
                };
                let written = file.file.written();
                let failed = if offset != written || written + data.len() as u64 > file.size {
                    Some(format!(
                        "{path}: data out of order or longer than announced"
                    ))
                } else {
                    file.file
                        .append(data)
                        .await
                        .err()
                        .map(|error| error.to_string())
                };
                if let Some(error) = failed {
                    status.issue(error);
                    if let Some(file) = incoming.take() {
                        file.abandon(&mut names.names);
                    }
                    rejected = Some(path);
                    continue;
                }
                if last && let Some(done) = incoming.take() {
                    match done.commit(&mut names.names).await {
                        Ok(()) => status.written += 1,
                        Err(error) => status.issue(error),
                    }
                }
            }
            Ok(SyncMessage::Delete { path }) => match RelPath::new(path) {
                // A path skipped for its letter case: the file on disk is the
                // other spelling's, so it stays.
                Ok(rel) if !names.names.release(&rel) => {}
                // Deletions of the initial pass remove what the source lacks.
                Ok(rel) => match target.remove(&rel).await {
                    Ok(()) if status.state == "watching" => status.removed += 1,
                    Ok(()) => status.pruned += 1,
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
    if let Some(unfinished) = incoming.take() {
        unfinished.abandon(&mut names.names);
    }
    if !failed {
        status.state = "stopped";
    }
    status.publish(&conn, &key);
    failed
}

/// Starts receiving `rel` from the box. Names this OS cannot hold and paths
/// that collide by letter case with a held file or folder are refused first,
/// because `begin_file` already changes the disk: it replaces a file standing
/// where `rel` needs a folder.
async fn begin(
    target: &LocalTarget,
    names: &mut CaseCollisions,
    rel: RelPath,
    mode: u32,
    size: u64,
) -> Result<Incoming, String> {
    devlanes_sync::refuse_unusable(&rel).map_err(|error| error.to_string())?;
    let new = names.admit(&rel).map_err(|held| {
        format!(
            "{rel}: not synced, it clashes with {held}, whose name differs only in letter case, which this file system does not tell apart"
        )
    })?;
    match target.begin_file(&rel, mode).await {
        Ok(file) => Ok(Incoming {
            rel,
            size,
            file,
            new,
        }),
        Err(error) => {
            if new {
                names.release(&rel);
            }
            Err(error.to_string())
        }
    }
}

/// One entry's names, taken from the host for a subscription and put back when
/// it ends (an abort included), so the next connection reports files under the
/// spellings the box used.
struct KeptNames {
    conn: Arc<HostConn>,
    key: String,
    names: CaseCollisions,
}

impl KeptNames {
    /// The names kept for `key`, with the files of the last listing forgotten.
    fn take(conn: &Arc<HostConn>, key: &str) -> Self {
        let mut names = lock(&conn.names).remove(key).unwrap_or_default();
        names.forget_files();
        Self {
            conn: conn.clone(),
            key: key.to_owned(),
            names,
        }
    }
}

impl Drop for KeptNames {
    /// Hands the names back to the host.
    fn drop(&mut self) {
        let names = std::mem::take(&mut self.names);
        lock(&self.conn.names).insert(self.key.clone(), names);
    }
}

/// Marks a sync whose task was aborted (the session ended) as interrupted, so
/// its status does not stay at `comparing` or `syncing` until the next session.
struct Interrupted {
    conn: Arc<HostConn>,
    key: String,
}

impl Drop for Interrupted {
    /// Rewrites an in-progress state; a finished task already published its own.
    fn drop(&mut self) {
        if let Some(status) = lock(&self.conn.syncs).get_mut(&self.key)
            && matches!(
                status["state"].as_str(),
                Some("comparing" | "syncing" | "watching")
            )
        {
            status["state"] = json!("interrupted");
        }
    }
}

/// A file being received into a temp file.
struct Incoming {
    rel: RelPath,
    size: u64,
    file: PartialFile,
    /// Whether `rel` was not held before this file, so it is released when the
    /// file does not land: nothing of it is on disk then, and keeping it would
    /// refuse the other spellings of the path for no file.
    new: bool,
}

impl Incoming {
    /// Drops the temp file and releases a new path.
    fn abandon(self, names: &mut CaseCollisions) {
        if self.new {
            names.release(&self.rel);
        }
    }

    /// Moves the file into place; a refused or failed commit releases a new path.
    async fn commit(self, names: &mut CaseCollisions) -> Result<(), String> {
        let Self { rel, file, new, .. } = self;
        file.commit().await.map_err(|error| {
            if new {
                names.release(&rel);
            }
            error.to_string()
        })
    }
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
    /// Manifest entries sent to the box in this subscription.
    compared: u64,
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
            compared: 0,
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
            "compared": self.compared,
            "errors": self.errors,
            "error": self.error,
            "updated_ms": self.updated_ms,
        });
        lock(&conn.syncs).insert(key.to_owned(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(lane: &str, unit: &str, from: &str) -> SyncEntry {
        let from = PathBuf::from(from);
        SyncEntry {
            lane: lane.into(),
            project: "app".into(),
            unit: unit.into(),
            index: 0,
            leaf: from
                .file_name()
                .map(|leaf| leaf.to_string_lossy().into_owned())
                .unwrap_or_default(),
            from,
            ignore: Vec::new(),
        }
    }

    #[test]
    fn duplicate_leaves_in_a_lane_are_refused() {
        let entries = [
            entry("app@0", "web", "/box/web/dist"),
            entry("app@0", "admin", "/box/admin/dist"),
            SyncEntry {
                index: 1,
                ..entry("app@0", "web", "/box/web/ext")
            },
            entry("app@1", "web", "/box/w1/dist"),
            // Replicas syncing the same folder name collide too.
            entry("app@2", "web-1", "/box/w/dist"),
            entry("app@2", "web-2", "/box/w/dist"),
            // A replicated service where only one replica syncs is fine.
            entry("app@3", "web-1", "/box/w/dist"),
        ];
        let refused = duplicate_leaves(&entries);
        let keys: Vec<&str> = refused.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "app@0:admin#0",
                "app@0:web#0",
                "app@2:web-1#0",
                "app@2:web-2#0"
            ]
        );
        // Windows does not tell `dist` from `Dist`; Unix keeps both folders.
        let cased = duplicate_leaves(&[
            entry("app@4", "web", "/box/web/dist"),
            entry("app@4", "admin", "/box/admin/Dist"),
        ]);
        #[cfg(windows)]
        assert_eq!(cased.len(), 2, "{cased:?}");
        #[cfg(unix)]
        assert!(cased.is_empty(), "{cased:?}");
        let message = &refused["app@0:web#0"];
        assert!(
            message.contains("app@0:web (/box/web/dist)")
                && message.contains("app@0:admin (/box/admin/dist)")
                && message.contains("`dist`"),
            "{message}"
        );
        // Folded one to one, like the sync names: `ß` is not `SS`.
        let eszett = duplicate_leaves(&[
            entry("app@5", "web", "/box/web/straße"),
            entry("app@5", "admin", "/box/admin/STRASSE"),
        ]);
        assert!(eszett.is_empty(), "{eszett:?}");
    }

    fn rel(path: &str) -> RelPath {
        RelPath::new(path).expect("path")
    }

    /// On Windows a box folder `dist/` next to a box file `Dist` is refused
    /// before anything on disk changes, so the file stays; a name Windows cannot
    /// hold is refused without being recorded. Unix keeps both and takes every
    /// name.
    #[tokio::test]
    async fn collisions_are_refused_before_the_disk_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("out");
        let target = LocalTarget::owned(&root).await.expect("claim");
        let mut names = CaseCollisions::new();
        let mut file = begin(&target, &mut names, rel("Dist"), 0o644, 4)
            .await
            .expect("begin");
        file.file
            .append(bytes::Bytes::from_static(b"file"))
            .await
            .expect("append");
        file.commit(&mut names).await.expect("commit");
        let folder = begin(&target, &mut names, rel("dist/x.js"), 0o644, 0).await;
        let unusable = begin(&target, &mut names, rel("aux.js"), 0o644, 0).await;
        #[cfg(windows)]
        {
            let error = folder.err().unwrap_or_default();
            assert!(error.contains("letter case"), "{error}");
            assert_eq!(std::fs::read(root.join("Dist")).expect("read"), b"file");
            assert!(unusable.is_err());
            assert_eq!(names.admit(&rel("AUX.js")), Ok(true), "aux.js was recorded");
        }
        #[cfg(unix)]
        {
            assert!(folder.is_ok());
            assert!(unusable.is_ok());
        }
    }

    /// A new path whose write fails (it could not begin, was abandoned, or its
    /// commit failed) is forgotten, so a later file in another case is not
    /// refused for a file that never landed. A held file keeps its name.
    #[tokio::test]
    async fn failed_writes_release_their_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where the destination folder should be: nothing can begin.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"").expect("file");
        let mut names = CaseCollisions::new();
        let begun = begin(
            &LocalTarget::new(&blocked),
            &mut names,
            rel("a.js"),
            0o644,
            0,
        )
        .await;
        assert!(begun.is_err());
        assert_eq!(names.admit(&rel("A.js")), Ok(true), "a.js stayed held");

        let root = dir.path().join("out");
        let target = LocalTarget::new(&root);
        let mut names = CaseCollisions::new();
        let file = begin(&target, &mut names, rel("b.js"), 0o644, 1)
            .await
            .expect("begin");
        file.abandon(&mut names);
        assert_eq!(names.admit(&rel("B.js")), Ok(true), "b.js stayed held");

        // A folder with a file in it where the file should go: an unowned
        // destination refuses to remove it, so the commit fails.
        std::fs::create_dir_all(root.join("c.js")).expect("dir");
        std::fs::write(root.join("c.js/keep"), b"keep").expect("keep");
        let file = begin(&target, &mut names, rel("c.js"), 0o644, 0)
            .await
            .expect("begin");
        assert!(file.commit(&mut names).await.is_err());
        assert_eq!(names.admit(&rel("C.js")), Ok(true), "c.js stayed held");

        // A held file whose update fails is still on disk and stays held.
        let mut names = CaseCollisions::new();
        names.hold(&rel("d.js"));
        let file = begin(&target, &mut names, rel("d.js"), 0o644, 1)
            .await
            .expect("begin");
        file.abandon(&mut names);
        let other = names.admit(&rel("D.js"));
        #[cfg(windows)]
        assert_eq!(other, Err(rel("d.js")));
        #[cfg(unix)]
        assert_eq!(other, Ok(true));
    }
}
