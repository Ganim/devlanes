//! Box-to-client sync subscriptions over dedicated mux streams.
//!
//! The transport only frames messages. Manifest contents, chunk ordering and
//! path validation belong to the sync crate on each side.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{BoxStream, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::mux::MuxStream;
use crate::wire::{self, Channel, SyncClientFrame, SyncFrame, WireError};

/// One sync message from the box.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncMessage {
    /// An opaque manifest, sent first and again whenever the sync source wants.
    Manifest(Value),
    /// File bytes at `offset`; `last` marks the end of that file.
    ///
    /// `path` is untrusted on the client: validate it before writing.
    Chunk {
        /// Relative path chosen by the box.
        path: String,
        /// Byte offset of `data` within the file.
        offset: u64,
        /// File bytes.
        data: Bytes,
        /// Whether this chunk completes the file.
        last: bool,
    },
    /// A file or directory deleted on the box.
    Delete {
        /// Relative path chosen by the box.
        path: String,
    },
}

/// A sync source failure, forwarded verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct SyncError {
    /// Human-readable description.
    pub message: String,
}

impl SyncError {
    /// Builds an error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Messages for one subscription; ending the stream ends the subscription.
pub type SyncFeed = BoxStream<'static, Result<SyncMessage, SyncError>>;

/// Box-side supplier of sync feeds, implemented by the sync crate.
#[async_trait]
pub trait SyncSource: Send + Sync {
    /// Starts a feed for an opaque subscription request. `input` carries what the
    /// client sends after the request, such as its manifest in batches.
    async fn subscribe(&self, request: Value, input: SyncInput) -> Result<SyncFeed, SyncError>;
}

/// One item of client input on a sync stream.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncInputEvent {
    /// An opaque value from the client.
    Value(Value),
    /// The client said no more input follows; the subscription goes on.
    End,
}

/// Client input of one subscription.
///
/// Values are queued with backpressure: a client sending faster than the source
/// reads waits. Dropping the input discards later values.
pub struct SyncInput {
    receiver: mpsc::Receiver<SyncInputEvent>,
}

impl SyncInput {
    /// Wraps a queue; for sources tested without a transport.
    pub fn from_receiver(receiver: mpsc::Receiver<SyncInputEvent>) -> Self {
        Self { receiver }
    }

    /// The next input. `None` after [`SyncInputEvent::End`], or when the client
    /// left without ending its input.
    pub async fn recv(&mut self) -> Option<SyncInputEvent> {
        self.receiver.recv().await
    }
}

/// Client input values queued per subscription before the client waits.
const INPUT_QUEUE: usize = 4;

/// A [`SyncSource`] that refuses every subscription, for boxes without sync.
pub struct NoSync;

#[async_trait]
impl SyncSource for NoSync {
    async fn subscribe(&self, _request: Value, _input: SyncInput) -> Result<SyncFeed, SyncError> {
        Err(SyncError::new("sync is not available on this box"))
    }
}

/// Serves one sync stream until the feed ends or the client goes away.
pub(crate) async fn serve_subscription(
    channel: Channel<MuxStream>,
    request: Value,
    source: Arc<dyn SyncSource>,
) {
    let (mut sink, mut client) = channel.split();
    let (input_tx, receiver) = mpsc::channel(INPUT_QUEUE);
    let gone = CancellationToken::new();
    let reader_gone = gone.clone();
    let reader = tokio::spawn(async move {
        let mut input_tx = Some(input_tx);
        loop {
            match wire::recv::<_, SyncClientFrame>(&mut client).await {
                Ok(Some((SyncClientFrame::Input { value }, _))) => {
                    // Waiting is the backpressure for manifest batches. A source that
                    // dropped its input makes the send fail and reading resumes, so
                    // the client leaving is still noticed.
                    if let Some(tx) = &input_tx
                        && tx.send(SyncInputEvent::Value(value)).await.is_err()
                    {
                        input_tx = None;
                    }
                }
                Ok(Some((SyncClientFrame::EndInput, _))) => {
                    if let Some(tx) = input_tx.take() {
                        let _ = tx.send(SyncInputEvent::End).await;
                    }
                }
                // EOF, a reset or a malformed frame: the client is gone.
                Ok(None) | Err(_) => {
                    reader_gone.cancel();
                    return;
                }
            }
        }
    });
    let _reader = AbortOnDrop(reader);
    let input = SyncInput { receiver };
    let feed = tokio::select! {
        feed = source.subscribe(request, input) => feed,
        () = gone.cancelled() => return,
    };
    let mut feed = match feed {
        Ok(feed) => feed,
        Err(error) => {
            let _ = wire::send(&mut sink, &SyncFrame::Error { error }).await;
            let _ = sink.close().await;
            return;
        }
    };
    loop {
        let item = tokio::select! {
            item = feed.next() => item,
            () = gone.cancelled() => return,
        };
        let sent = match item {
            Some(Ok(SyncMessage::Manifest(manifest))) => {
                wire::send(&mut sink, &SyncFrame::Manifest { manifest }).await
            }
            Some(Ok(SyncMessage::Chunk {
                path,
                offset,
                data,
                last,
            })) => {
                let header = SyncFrame::Chunk { path, offset, last };
                wire::send_with_payload(&mut sink, &header, data).await
            }
            Some(Ok(SyncMessage::Delete { path })) => {
                wire::send(&mut sink, &SyncFrame::Delete { path }).await
            }
            Some(Err(error)) => {
                let _ = wire::send(&mut sink, &SyncFrame::Error { error }).await;
                break;
            }
            None => {
                let _ = wire::send(&mut sink, &SyncFrame::End).await;
                break;
            }
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = sink.close().await;
}

/// Aborts the client reader when the subscription ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    /// Stops reading the client.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Failure of a subscription as seen by the client.
#[derive(Debug, thiserror::Error)]
pub enum SyncStreamError {
    /// The box's sync source failed.
    #[error("remote sync error: {0}")]
    Remote(#[from] SyncError),
    /// The stream broke or carried invalid frames.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The stream ended without an end marker, usually because the session died.
    #[error("sync stream interrupted")]
    Interrupted,
}

/// Client side of a subscription. Dropping it cancels the feed on the box.
pub struct SyncSubscription {
    // Closing the write side would read as cancellation, so it stays open.
    sink: futures_util::stream::SplitSink<Channel<MuxStream>, wire::Frame>,
    stream: SplitStream<Channel<MuxStream>>,
    done: bool,
    input_ended: bool,
}

impl SyncSubscription {
    /// Wraps a stream whose subscription frame was already sent.
    pub(crate) fn new(channel: Channel<MuxStream>) -> Self {
        let (sink, stream) = channel.split();
        Self {
            sink,
            stream,
            done: false,
            input_ended: false,
        }
    }

    /// Sends one input value to the box's sync source; waits while the box is
    /// behind.
    pub async fn send(&mut self, value: Value) -> Result<(), SyncStreamError> {
        wire::send(&mut self.sink, &SyncClientFrame::Input { value }).await?;
        Ok(())
    }

    /// Tells the box no more input follows; messages keep flowing.
    pub async fn finish_input(&mut self) -> Result<(), SyncStreamError> {
        if !self.input_ended {
            wire::send(&mut self.sink, &SyncClientFrame::EndInput).await?;
            self.input_ended = true;
        }
        Ok(())
    }

    /// Next message; `None` once the box ended the feed normally.
    pub async fn next(&mut self) -> Option<Result<SyncMessage, SyncStreamError>> {
        if self.done {
            return None;
        }
        let result = match wire::recv::<_, SyncFrame>(&mut self.stream).await {
            Ok(Some((frame, payload))) => match frame {
                SyncFrame::Manifest { manifest } => {
                    return Some(Ok(SyncMessage::Manifest(manifest)));
                }
                SyncFrame::Chunk { path, offset, last } => {
                    return Some(Ok(SyncMessage::Chunk {
                        path,
                        offset,
                        data: payload,
                        last,
                    }));
                }
                SyncFrame::Delete { path } => return Some(Ok(SyncMessage::Delete { path })),
                SyncFrame::Error { error } => Some(Err(error.into())),
                SyncFrame::End => None,
            },
            Ok(None) => Some(Err(SyncStreamError::Interrupted)),
            Err(error) => Some(Err(error.into())),
        };
        self.done = true;
        result
    }
}
