//! Box-to-client folder sync (plan 14.5): for every `sync` entry of a running box
//! lane, a subscription streams a full copy and then changes, which land in the
//! entry's destination on this machine. Paths from the box are untrusted and pass
//! [`RelPath`] validation before anything touches the disk.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use devlanes_proto::{
    Context, Method, Outcome,
    remote::{Forward, Reply, SyncEntry, SyncManifest, SyncRequest},
};
use devlanes_sync::{IgnoreRules, LocalTarget, RelPath, SyncTarget};
use devlanes_transport::{Session as BoxSession, SyncMessage};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::{HostConn, local_destination, now_ms};
use crate::{engine::Inner, model::lock};

/// Quiet period after a registry change before the entries are listed again.
const RELIST_DELAY: Duration = Duration::from_millis(500);

/// Largest file the client buffers; bigger ones are refused, not truncated.
const MAX_FILE: u64 = 512 * 1024 * 1024;

/// Follows the box's sync entries for one session; ends with the session.
pub(super) async fn run(engine: Weak<Inner>, conn: Arc<HostConn>, session: BoxSession) {
    let Some(home) = engine.upgrade().and_then(|inner| inner.integ.remote.home()) else {
        return;
    };
    let mut registry = conn.registry.subscribe();
    let mut running: BTreeMap<String, JoinHandle<()>> = BTreeMap::new();
    loop {
        match list(&session).await {
            Ok(entries) => {
                let keys: BTreeSet<String> = entries.iter().map(key).collect();
                running.retain(|key, task| {
                    let keep = keys.contains(key) && !task.is_finished();
                    if !keep {
                        task.abort();
                    }
                    keep
                });
                lock(&conn.syncs).retain(|key, _| keys.contains(key));
                for entry in entries {
                    let key = key(&entry);
                    if running.contains_key(&key) {
                        continue;
                    }
                    let destination = local_destination(&home, &entry);
                    let task =
                        tokio::spawn(follow(conn.clone(), session.clone(), entry, destination));
                    running.insert(key, task);
                }
            }
            Err(error) => tracing::debug!(host = %conn.name, %error, "cannot list box syncs"),
        }
        tokio::select! {
            _ = session.closed() => break,
            () = conn.cancel.cancelled() => break,
            changed = registry.changed() => {
                if changed.is_err() {
                    break;
                }
                tokio::time::sleep(RELIST_DELAY).await;
            }
        }
    }
    for task in running.into_values() {
        task.abort();
    }
}

/// Status key of an entry.
fn key(entry: &SyncEntry) -> String {
    format!("{}:{}#{}", entry.lane, entry.unit, entry.index)
}

/// Lists the box's entries through its `sync.status`.
async fn list(session: &BoxSession) -> Result<Vec<SyncEntry>, String> {
    let forward = Forward {
        method: Method::SyncStatus,
        params: json!({}),
        context: Context::default(),
    };
    let request = serde_json::to_value(&forward).map_err(|e| e.to_string())?;
    let mut call = session.call(request).await.map_err(|e| e.to_string())?;
    call.finish_input().await.map_err(|e| e.to_string())?;
    while let Some(item) = call.next().await {
        let reply: Reply =
            serde_json::from_value(item.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        match reply {
            Reply::Response {
                outcome: Outcome::Ok { data },
            } => {
                let items = data.get("items").cloned().unwrap_or(Value::Null);
                return serde_json::from_value(items).map_err(|e| e.to_string());
            }
            Reply::Response {
                outcome: Outcome::Error { error },
            } => return Err(error.code),
            _ => {}
        }
    }
    Err("sync.status ended without an answer".into())
}

/// One entry's subscription: applies files until the stream ends.
async fn follow(conn: Arc<HostConn>, session: BoxSession, entry: SyncEntry, to: PathBuf) {
    let key = key(&entry);
    let mut status = Status::new(&entry, &to);
    status.publish(&conn, &key);
    let request = SyncRequest {
        lane: entry.lane.clone(),
        unit: entry.unit.clone(),
        index: entry.index,
    };
    let subscription = match serde_json::to_value(&request) {
        Ok(request) => session.subscribe_sync(request).await,
        Err(error) => {
            status.fail(error.to_string());
            status.publish(&conn, &key);
            return;
        }
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            status.fail(error.to_string());
            status.publish(&conn, &key);
            return;
        }
    };
    let target = LocalTarget::new(&to);
    let rules = IgnoreRules::new(&entry.ignore).unwrap_or_else(|_| IgnoreRules::none());
    let mut file: Option<Incoming> = None;
    while let Some(message) = subscription.next().await {
        match message {
            Ok(SyncMessage::Manifest(value)) => match serde_json::from_value(value) {
                Ok(SyncManifest::File { path, mode, size }) if size <= MAX_FILE => {
                    file = Some(Incoming {
                        path,
                        mode,
                        data: BytesMut::with_capacity(
                            usize::try_from(size.min(8 * 1024 * 1024)).unwrap_or(0),
                        ),
                    });
                }
                Ok(SyncManifest::File { path, size, .. }) => {
                    file = None;
                    status.issue(format!(
                        "{path}: {size} bytes is larger than the sync limit"
                    ));
                }
                Ok(SyncManifest::Ready { files }) => {
                    status.pruned += prune(&target, &rules, &files).await;
                    status.state = "watching";
                }
                Err(error) => status.issue(format!("unreadable manifest: {error}")),
            },
            Ok(SyncMessage::Chunk {
                path,
                offset,
                data,
                last,
            }) => {
                let Some(incoming) = file.as_mut().filter(|f| f.path == path) else {
                    status.issue(format!("{path}: chunk without a file header"));
                    continue;
                };
                if offset != incoming.data.len() as u64
                    || incoming.data.len() as u64 + data.len() as u64 > MAX_FILE
                {
                    status.issue(format!("{path}: chunk out of order"));
                    file = None;
                    continue;
                }
                incoming.data.extend_from_slice(&data);
                if last && let Some(done) = file.take() {
                    write(&target, &mut status, done).await;
                }
            }
            Ok(SyncMessage::Delete { path }) => match RelPath::new(path.clone()) {
                Ok(rel) => match target.remove(&rel).await {
                    Ok(()) => status.removed += 1,
                    Err(error) => status.issue(error.to_string()),
                },
                Err(error) => status.issue(error.to_string()),
            },
            Err(error) => {
                status.fail(error.to_string());
                break;
            }
        }
        status.publish(&conn, &key);
    }
    if status.state != "failed" {
        status.state = "stopped";
    }
    status.publish(&conn, &key);
}

/// A file being received.
struct Incoming {
    path: String,
    mode: u32,
    data: BytesMut,
}

/// Validates the box's path, then writes atomically below the destination.
async fn write(target: &LocalTarget, status: &mut Status, file: Incoming) {
    let rel = match RelPath::new(file.path) {
        Ok(rel) => rel,
        Err(error) => {
            status.issue(error.to_string());
            return;
        }
    };
    let contents: Bytes = file.data.freeze();
    match target.write_file(&rel, contents, file.mode).await {
        Ok(()) => status.written += 1,
        Err(error) => status.issue(error.to_string()),
    }
}

/// Removes destination files the box's full copy did not contain, keeping ignored
/// ones, which belong to this machine.
async fn prune(target: &LocalTarget, rules: &IgnoreRules, files: &[String]) -> u64 {
    let keep: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let Ok(manifest) = target.manifest().await else {
        return 0;
    };
    let mut removed = 0;
    for rel in manifest.keys() {
        if keep.contains(rel.as_str()) || rules.is_ignored(rel) {
            continue;
        }
        if target.remove(rel).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Per-entry status shown by `host ls` and `sync status`.
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
