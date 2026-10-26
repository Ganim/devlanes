//! Folder syncs streamed to a client (plan 14.5).
//!
//! On `follow`, the client sends its destination's manifest in batches, ended by
//! an end-of-input marker. Each batch is compared with the source as it arrives
//! ([`Syncer::offer_held`]); nothing is sent before the marker. Then a [`Syncer`]
//! writes into [`MuxTarget`], which turns each file into a manifest header plus
//! chunks, and each deletion into a delete message, so only differences travel.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_client::{Client, Paths};
use devlanes_proto::{
    Context, Method,
    remote::{MAX_SYNC_FILE, ManifestBatch, SyncEntry, SyncFile, SyncManifest, SyncRequest},
};
use devlanes_sync::{ContentHash, HeldFile, Manifest, RelPath, SyncOptions, SyncTarget, Syncer};
use devlanes_transport::{SyncError, SyncFeed, SyncInput, SyncInputEvent, SyncMessage, SyncSource};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// File bytes per chunk; well under the transport's 8 MiB frame limit.
const CHUNK: usize = 1024 * 1024;

/// Syncs one client may follow at once; each holds a watcher and a task here.
const MAX_SUBSCRIPTIONS: usize = 16;

/// [`SyncSource`] for the entries the box daemon lists in `sync.status`.
pub(crate) struct DaemonSync {
    paths: Paths,
    slots: Arc<Semaphore>,
}

impl DaemonSync {
    /// Looks entries up through the daemon behind `paths`.
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            slots: Arc::new(Semaphore::new(MAX_SUBSCRIPTIONS)),
        }
    }

    /// Sync entries of the box's running lanes.
    async fn entries(&self) -> Result<Vec<SyncEntry>, SyncError> {
        let mut client = Client::connect(&self.paths)
            .await
            .map_err(|error| SyncError::new(format!("box daemon: {error}")))?;
        let data = client
            .request(Method::SyncStatus, json!({}), Context::default())
            .await
            .map_err(|error| SyncError::new(format!("sync.status: {error}")))?;
        serde_json::from_value(data.get("items").cloned().unwrap_or(Value::Null))
            .map_err(|error| SyncError::new(format!("sync.status: {error}")))
    }

    /// Streams one entry until the client drops the feed; `read_only_modes` as
    /// the client's request says (see [`SyncOptions::read_only_modes`]).
    async fn follow(
        &self,
        lane: String,
        unit: String,
        index: usize,
        read_only_modes: bool,
        input: SyncInput,
    ) -> Result<SyncFeed, SyncError> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            SyncError::new(format!(
                "this client already follows {MAX_SUBSCRIPTIONS} syncs on the box"
            ))
        })?;
        let entry = self
            .entries()
            .await?
            .into_iter()
            .find(|entry| entry.lane == lane && entry.unit == unit && entry.index == index)
            .ok_or_else(|| {
                SyncError::new(format!(
                    "{lane}:{unit} has no sync entry {index} running on this box"
                ))
            })?;
        follow_entry(&entry, read_only_modes, input, permit)
    }
}

/// Feeds one sync entry to a client whose manifest arrives on `input`.
///
/// A client that leaves before its end marker leaves nothing behind: the next
/// subscription starts over with a fresh manifest.
pub(crate) fn follow_entry(
    entry: &SyncEntry,
    read_only_modes: bool,
    mut input: SyncInput,
    permit: OwnedSemaphorePermit,
) -> Result<SyncFeed, SyncError> {
    let (messages, receiver) = mpsc::channel(16);
    let target = Arc::new(MuxTarget {
        messages: messages.clone(),
    });
    let options = SyncOptions {
        ignore: entry.ignore.clone(),
        max_file: Some(MAX_SYNC_FILE),
        read_only_modes,
        ..SyncOptions::default()
    };
    let mut syncer = Syncer::new(&entry.from, target, options)
        .map_err(|error| SyncError::new(error.to_string()))?;
    tokio::spawn(async move {
        let _permit = permit;
        loop {
            let event = tokio::select! {
                () = messages.closed() => return,
                event = input.recv() => event,
            };
            match event {
                Some(SyncInputEvent::Value(value)) => {
                    let batch = match serde_json::from_value::<ManifestBatch>(value) {
                        Ok(batch) => batch,
                        Err(error) => {
                            let message = format!("bad manifest batch: {error}");
                            let _ = messages.send(Err(SyncError::new(message))).await;
                            return;
                        }
                    };
                    let held = batch.files.into_iter().filter_map(held_file).collect();
                    if let Err(error) = syncer.offer_held(held).await {
                        let _ = messages.send(Err(SyncError::new(error.to_string()))).await;
                        return;
                    }
                }
                Some(SyncInputEvent::End) => break,
                // The client left mid-manifest.
                None => return,
            }
        }
        drop(input);
        // `start` returns after the initial pass, which needs the feed being read.
        match syncer.start().await {
            Ok(handle) => {
                if let Ok(ready) = serde_json::to_value(SyncManifest::Ready)
                    && messages
                        .send(Ok(SyncMessage::Manifest(ready)))
                        .await
                        .is_ok()
                {
                    messages.closed().await;
                }
                handle.stop().await;
            }
            Err(error) => {
                let _ = messages.send(Err(SyncError::new(error.to_string()))).await;
            }
        }
    });
    Ok(feed(receiver))
}

/// The receiver as a feed.
fn feed(receiver: mpsc::Receiver<Result<SyncMessage, SyncError>>) -> SyncFeed {
    futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    })
    .boxed()
}

/// A client manifest entry as a held file; an invalid path is dropped (it cannot
/// name a source file), an unreadable hash means "send or delete it".
fn held_file(file: SyncFile) -> Option<HeldFile> {
    Some(HeldFile {
        path: RelPath::new(file.path).ok()?,
        size: file.size,
        mode: file.mode,
        hash: ContentHash::from_hex(&file.hash),
        mtime: None,
    })
}

#[async_trait]
impl SyncSource for DaemonSync {
    /// Lists entries or follows one.
    async fn subscribe(&self, request: Value, input: SyncInput) -> Result<SyncFeed, SyncError> {
        match serde_json::from_value(request)
            .map_err(|error| SyncError::new(format!("bad sync request: {error}")))?
        {
            SyncRequest::List => {
                let entries = SyncManifest::Entries {
                    entries: self.entries().await?,
                };
                let entries = serde_json::to_value(entries)
                    .map_err(|error| SyncError::new(error.to_string()))?;
                Ok(
                    futures_util::stream::once(async move { Ok(SyncMessage::Manifest(entries)) })
                        .boxed(),
                )
            }
            SyncRequest::Follow {
                lane,
                unit,
                index,
                read_only_modes,
            } => self.follow(lane, unit, index, read_only_modes, input).await,
        }
    }
}

/// The client end of a sync as a [`SyncTarget`].
struct MuxTarget {
    messages: mpsc::Sender<Result<SyncMessage, SyncError>>,
}

impl MuxTarget {
    /// Queues one message; fails once the client is gone, which stops the syncer's pass.
    async fn send(&self, message: SyncMessage) -> Result<(), devlanes_sync::SyncError> {
        self.messages
            .send(Ok(message))
            .await
            .map_err(|_| devlanes_sync::SyncError::Target("client disconnected".into()))
    }
}

#[async_trait]
impl SyncTarget for MuxTarget {
    /// Never read: the client's manifest arrives through [`Syncer::offer_held`]
    /// (an empty manifest means an empty destination).
    async fn manifest(&self) -> Result<Manifest, devlanes_sync::SyncError> {
        Ok(Manifest::new())
    }

    /// A `file` header, then the contents in chunks.
    async fn write_file(
        &self,
        path: &RelPath,
        contents: Bytes,
        mode: u32,
    ) -> Result<(), devlanes_sync::SyncError> {
        let header = SyncManifest::File {
            path: path.as_str().to_owned(),
            mode,
            size: contents.len() as u64,
        };
        let header = serde_json::to_value(header)
            .map_err(|error| devlanes_sync::SyncError::Target(error.to_string()))?;
        self.send(SyncMessage::Manifest(header)).await?;
        let mut offset = 0;
        loop {
            let end = (offset + CHUNK).min(contents.len());
            self.send(SyncMessage::Chunk {
                path: path.as_str().to_owned(),
                offset: offset as u64,
                data: contents.slice(offset..end),
                last: end == contents.len(),
            })
            .await?;
            if end == contents.len() {
                break;
            }
            offset = end;
        }
        Ok(())
    }

    /// A deletion message.
    async fn remove(&self, path: &RelPath) -> Result<(), devlanes_sync::SyncError> {
        self.send(SyncMessage::Delete {
            path: path.as_str().to_owned(),
        })
        .await
    }

    /// Remote: no local destination to compare against the source.
    fn local_root(&self) -> Option<&Path> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use devlanes_sync::{HashCache, LocalTarget};
    use devlanes_transport::{
        BoxServices, LoopbackDialer, RegistrySnapshot, ServeOptions, Session, SessionOptions,
        SharedRegistry, SyncSubscription, memory::memory_transport, serve,
    };

    use super::*;

    /// A box serving one fixed sync entry over an in-memory transport.
    struct TestSource {
        entry: SyncEntry,
        slots: Arc<Semaphore>,
    }

    #[async_trait]
    impl SyncSource for TestSource {
        async fn subscribe(&self, request: Value, input: SyncInput) -> Result<SyncFeed, SyncError> {
            let permit = self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| SyncError::new("busy"))?;
            let read_only_modes = request["read_only_modes"].as_bool().unwrap_or(false);
            follow_entry(&self.entry, read_only_modes, input, permit)
        }
    }

    /// Starts the box and connects a client session.
    async fn connect(source: &Path) -> Session {
        let (transport, mut listener) = memory_transport();
        let services = BoxServices {
            registry: Arc::new(SharedRegistry::new(RegistrySnapshot::default(), 4)),
            dialer: Arc::new(LoopbackDialer),
            sync: Arc::new(TestSource {
                entry: SyncEntry {
                    lane: "app@0".into(),
                    project: "app".into(),
                    unit: "web".into(),
                    index: 0,
                    from: source.to_path_buf(),
                    leaf: "ext".into(),
                    ignore: Vec::new(),
                },
                slots: Arc::new(Semaphore::new(4)),
            }),
        };
        tokio::spawn(async move {
            while let Some(link) = listener.accept().await {
                let services = services.clone();
                tokio::spawn(serve(link, services, ServeOptions::new("test-box")));
            }
        });
        Session::connect(&transport, SessionOptions::new("test"))
            .await
            .expect("session")
    }

    /// Opens a follow subscription.
    async fn follow(session: &Session) -> SyncSubscription {
        let request = json!({"op": "follow", "lane": "app@0", "unit": "web", "index": 0});
        session.subscribe_sync(request).await.expect("subscribe")
    }

    /// Sends a destination's manifest in batches of `batch`.
    async fn send_manifest(sync: &mut SyncSubscription, destination: &Path, batch: usize) {
        let mut walk = LocalTarget::new(destination).held(HashCache::default(), batch);
        while let Some(files) = walk.next().await {
            let files = files
                .into_iter()
                .map(|file| SyncFile {
                    path: file.path.as_str().to_owned(),
                    size: file.size,
                    mode: file.mode,
                    hash: file.hash.map(|hash| hash.to_hex()).unwrap_or_default(),
                    mtime_ms: None,
                })
                .collect();
            let batch = serde_json::to_value(ManifestBatch { files }).expect("batch");
            sync.send(batch).await.expect("send batch");
        }
    }

    /// Messages until `ready`, as `(written paths, deleted paths)`.
    async fn until_ready(sync: &mut SyncSubscription) -> (Vec<String>, Vec<String>) {
        let (mut written, mut deleted) = (Vec::new(), Vec::new());
        while let Some(message) = sync.next().await {
            match message.expect("message") {
                SyncMessage::Manifest(value) => match serde_json::from_value(value) {
                    Ok(SyncManifest::Ready) => return (written, deleted),
                    Ok(SyncManifest::File { path, .. }) => written.push(path),
                    _ => {}
                },
                SyncMessage::Delete { path } => deleted.push(path),
                SyncMessage::Chunk { .. } => {}
            }
        }
        panic!("the feed ended before ready");
    }

    /// Writes `count` files below `root` with settled mtimes.
    fn tree(root: &Path, count: usize) {
        let settled = std::time::SystemTime::now() - Duration::from_secs(60);
        for i in 0..count {
            let path = root.join(format!("d{}/f{i}.txt", i % 10));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(&path, format!("file {i}")).expect("write");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(settled))
                .expect("mtime");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deletions_come_only_after_the_end_marker() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (source, destination) = (dir.path().join("src"), dir.path().join("dst"));
            tree(&source, 20);
            tree(&destination, 20);
            std::fs::write(destination.join("stale.txt"), b"gone on the box").expect("stale");
            std::fs::write(destination.join("d3/f3.txt"), b"changed here").expect("changed");
            let session = connect(&source).await;
            let mut sync = follow(&session).await;
            send_manifest(&mut sync, &destination, 5).await;
            // Before the end marker the box sends nothing, deletions included.
            let early = tokio::time::timeout(Duration::from_millis(300), sync.next()).await;
            assert!(early.is_err(), "box sent {early:?} before the end marker");
            sync.finish_input().await.expect("end");
            let (written, deleted) = until_ready(&mut sync).await;
            assert_eq!(written, ["d3/f3.txt"]);
            assert_eq!(deleted, ["stale.txt"]);
        })
        .await
        .expect("timeout");
    }

    /// A client that keeps only a read-only attribute (Windows) reports `0o644`
    /// for an executable it holds; with `read_only_modes` the box does not resend
    /// it, without it the box sends it to restore the mode. Boxes are Unix.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_clients_compare_modes_by_write_access() {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (source, destination) = (dir.path().join("src"), dir.path().join("dst"));
            tree(&source, 3);
            tree(&destination, 3);
            let script = source.join("d1/f1.txt");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            let session = connect(&source).await;
            for (read_only, expected) in [
                (true, Vec::<String>::new()),
                (false, vec!["d1/f1.txt".to_owned()]),
            ] {
                let request = json!({
                    "op": "follow", "lane": "app@0", "unit": "web", "index": 0,
                    "read_only_modes": read_only,
                });
                let mut sync = session.subscribe_sync(request).await.expect("subscribe");
                send_manifest(&mut sync, &destination, 10).await;
                sync.finish_input().await.expect("end");
                let (written, deleted) = until_ready(&mut sync).await;
                assert_eq!(written, expected, "read_only_modes: {read_only}");
                assert!(deleted.is_empty(), "{deleted:?}");
            }
        })
        .await
        .expect("timeout");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscription_dropped_mid_manifest_starts_over_cleanly() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (source, destination) = (dir.path().join("src"), dir.path().join("dst"));
            tree(&source, 30);
            tree(&destination, 30);
            std::fs::write(destination.join("stale.txt"), b"x").expect("stale");
            let session = connect(&source).await;
            // Half a manifest, then the client goes away.
            let mut partial = follow(&session).await;
            let mut walk = LocalTarget::new(&destination).held(HashCache::default(), 10);
            let first = walk.next().await.expect("batch");
            let files = first
                .into_iter()
                .map(|file| SyncFile {
                    path: file.path.as_str().to_owned(),
                    size: file.size,
                    mode: file.mode,
                    hash: file.hash.map(|hash| hash.to_hex()).unwrap_or_default(),
                    mtime_ms: None,
                })
                .collect();
            let batch = serde_json::to_value(ManifestBatch { files }).expect("batch");
            partial.send(batch).await.expect("send");
            drop(partial);
            drop(walk.finish().await);
            // A new subscription with the whole manifest sees only the real difference,
            // and the dropped one freed its slot.
            for _ in 0..4 {
                let mut sync = follow(&session).await;
                send_manifest(&mut sync, &destination, 10).await;
                sync.finish_input().await.expect("end");
                let (written, deleted) = until_ready(&mut sync).await;
                assert!(written.is_empty(), "{written:?}");
                assert_eq!(deleted, ["stale.txt"]);
            }
        })
        .await
        .expect("timeout");
    }
}
