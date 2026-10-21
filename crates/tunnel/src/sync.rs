//! Folder syncs streamed to a client (plan 14.5): a [`Syncer`] per subscription
//! writes into [`MuxTarget`], which turns each file into a manifest header plus
//! chunks on the sync stream. The client sends what it already holds, so after a
//! reconnect only differences travel.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_client::{Client, Paths};
use devlanes_proto::{
    Context, Method,
    remote::{MAX_SYNC_FILE, SyncEntry, SyncFile, SyncManifest, SyncRequest},
};
use devlanes_sync::{
    ContentHash, Manifest, ManifestEntry, RelPath, SyncOptions, SyncTarget, Syncer,
};
use devlanes_transport::{SyncError, SyncFeed, SyncMessage, SyncSource};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

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

    /// Streams one entry until the client drops the feed.
    async fn follow(
        &self,
        lane: String,
        unit: String,
        index: usize,
        have: Vec<SyncFile>,
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
        let have = client_manifest(have);
        let (messages, receiver) = mpsc::channel(16);
        let target = Arc::new(MuxTarget {
            messages: messages.clone(),
            files: Mutex::new(have.keys().map(|rel| rel.as_str().to_owned()).collect()),
            have,
        });
        let options = SyncOptions {
            ignore: entry.ignore.clone(),
            max_file: Some(MAX_SYNC_FILE),
            ..SyncOptions::default()
        };
        let syncer = Syncer::new(&entry.from, target.clone(), options)
            .map_err(|error| SyncError::new(error.to_string()))?;
        // `start` returns after the initial pass, which needs the feed being read.
        tokio::spawn(async move {
            let _permit = permit;
            match syncer.start().await {
                Ok(handle) => {
                    let ready = SyncManifest::Ready {
                        files: target.files(),
                    };
                    if let Ok(ready) = serde_json::to_value(ready)
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
}

/// The receiver as a feed.
fn feed(receiver: mpsc::Receiver<Result<SyncMessage, SyncError>>) -> SyncFeed {
    futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    })
    .boxed()
}

/// The client's files as a manifest; entries with invalid paths or hashes are
/// dropped, which only means they get sent again.
fn client_manifest(have: Vec<SyncFile>) -> Manifest {
    have.into_iter()
        .filter_map(|file| {
            let rel = RelPath::new(file.path).ok()?;
            let hash = ContentHash::from_hex(&file.hash)?;
            Some((
                rel,
                ManifestEntry {
                    size: file.size,
                    mode: file.mode,
                    hash,
                },
            ))
        })
        .collect()
}

#[async_trait]
impl SyncSource for DaemonSync {
    /// Lists entries or follows one.
    async fn subscribe(&self, request: Value) -> Result<SyncFeed, SyncError> {
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
                have,
            } => self.follow(lane, unit, index, have).await,
        }
    }
}

/// The client end of a sync as a [`SyncTarget`].
struct MuxTarget {
    messages: mpsc::Sender<Result<SyncMessage, SyncError>>,
    /// What the client said it holds when it subscribed.
    have: Manifest,
    /// Paths the client holds after this subscription's writes and removals.
    files: Mutex<BTreeSet<String>>,
}

impl MuxTarget {
    /// Paths written and not removed, for the `ready` manifest.
    fn files(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    /// Locks the path set; it stays consistent even if a holder panicked.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

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
    /// The client's own manifest, so unchanged files are skipped and files the
    /// source no longer has are deleted.
    async fn manifest(&self) -> Result<Manifest, devlanes_sync::SyncError> {
        Ok(self.have.clone())
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
        self.lock().insert(path.as_str().to_owned());
        Ok(())
    }

    /// A deletion message.
    async fn remove(&self, path: &RelPath) -> Result<(), devlanes_sync::SyncError> {
        self.send(SyncMessage::Delete {
            path: path.as_str().to_owned(),
        })
        .await?;
        self.lock().remove(path.as_str());
        Ok(())
    }

    /// Remote: no local destination to compare against the source.
    fn local_root(&self) -> Option<&Path> {
        None
    }
}
