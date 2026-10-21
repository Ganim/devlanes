//! Folder syncs streamed to a client (plan 14.5): a [`Syncer`] per subscription
//! writes into [`MuxTarget`], which turns each file into a manifest header plus
//! chunks on the sync stream.

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
    remote::{SyncEntry, SyncManifest, SyncRequest},
};
use devlanes_sync::{Manifest, RelPath, SyncOptions, SyncTarget, Syncer};
use devlanes_transport::{SyncError, SyncFeed, SyncMessage, SyncSource};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// File bytes per chunk; well under the transport's 8 MiB frame limit.
const CHUNK: usize = 1024 * 1024;

/// [`SyncSource`] for the entries the box daemon lists in `sync.status`.
pub(crate) struct DaemonSync {
    paths: Paths,
}

impl DaemonSync {
    /// Looks entries up through the daemon behind `paths`.
    pub(crate) fn new(paths: Paths) -> Self {
        Self { paths }
    }

    /// The entry a request names, from the daemon's current list.
    async fn entry(&self, request: &SyncRequest) -> Result<SyncEntry, SyncError> {
        let mut client = Client::connect(&self.paths)
            .await
            .map_err(|error| SyncError::new(format!("box daemon: {error}")))?;
        let data = client
            .request(Method::SyncStatus, json!({}), Context::default())
            .await
            .map_err(|error| SyncError::new(format!("sync.status: {error}")))?;
        let entries: Vec<SyncEntry> =
            serde_json::from_value(data.get("items").cloned().unwrap_or(Value::Null))
                .map_err(|error| SyncError::new(format!("sync.status: {error}")))?;
        entries
            .into_iter()
            .find(|entry| {
                entry.lane == request.lane
                    && entry.unit == request.unit
                    && entry.index == request.index
            })
            .ok_or_else(|| {
                SyncError::new(format!(
                    "{}:{} has no sync entry {} running on this box",
                    request.lane, request.unit, request.index
                ))
            })
    }
}

#[async_trait]
impl SyncSource for DaemonSync {
    /// Starts a syncer for the named entry; it stops when the client drops the feed.
    async fn subscribe(&self, request: Value) -> Result<SyncFeed, SyncError> {
        let request: SyncRequest = serde_json::from_value(request)
            .map_err(|error| SyncError::new(format!("bad sync request: {error}")))?;
        let entry = self.entry(&request).await?;
        let (messages, receiver) = mpsc::channel(16);
        let target = Arc::new(MuxTarget {
            messages: messages.clone(),
            files: Mutex::new(BTreeSet::new()),
        });
        let options = SyncOptions {
            ignore: entry.ignore.clone(),
            ..SyncOptions::default()
        };
        let syncer = Syncer::new(&entry.from, target.clone(), options)
            .map_err(|error| SyncError::new(error.to_string()))?;
        // `start` returns after the initial copy, which needs the feed being read.
        tokio::spawn(async move {
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
        Ok(
            futures_util::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|item| (item, receiver))
            })
            .boxed(),
        )
    }
}

/// The client end of a sync as a [`SyncTarget`].
struct MuxTarget {
    messages: mpsc::Sender<Result<SyncMessage, SyncError>>,
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
    /// Unknown: the client's copy is not visible from here, so the first pass sends
    /// every file and the `ready` manifest lets the client drop stale ones.
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
