//! Forwarding of opaque daemon requests, one mux stream per call.
//!
//! Each call gets its own stream so a long streaming reply (logs follow, attach)
//! cannot delay pings on the control stream, and dropping the stream cancels it.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream::{BoxStream, SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::mux::MuxStream;
use crate::wire::{self, Channel, RpcBoxFrame, RpcClientFrame, WireError};

/// A failure reported by the box's handler, forwarded verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    /// Human-readable description.
    pub message: String,
    /// Optional structured detail for the daemon's own error model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Builds an error with only a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            data: None,
        }
    }
}

/// Replies produced by a handler; the stream ending finishes the call.
pub type RpcReplyStream = BoxStream<'static, Result<Value, RpcError>>;

/// Further client input for one call, for interactive requests.
///
/// Ends when the client finishes input. Dropping it does not cancel the call.
pub struct RpcInput {
    receiver: mpsc::Receiver<Value>,
}

impl RpcInput {
    /// Waits for the next input value; `None` once the client finished input.
    pub async fn recv(&mut self) -> Option<Value> {
        self.receiver.recv().await
    }

    /// Converts into a stream of input values.
    pub fn into_stream(self) -> BoxStream<'static, Value> {
        futures_util::stream::unfold(self.receiver, |mut receiver| async move {
            receiver.recv().await.map(|value| (value, receiver))
        })
        .boxed()
    }
}

/// Box-side handler for forwarded requests, implemented by the box daemon.
#[async_trait]
pub trait RpcHandler: Send + Sync {
    /// Handles `request`; the returned stream is dropped if the client cancels.
    async fn call(&self, request: Value, input: RpcInput) -> Result<RpcReplyStream, RpcError>;
}

/// Serves one RPC stream on the box until replies end or the client goes away.
pub(crate) async fn serve_call(
    channel: Channel<MuxStream>,
    request: Value,
    handler: Arc<dyn RpcHandler>,
) {
    let (mut sink, mut source) = channel.split();
    let (input_tx, receiver) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let reader_cancel = cancel.clone();
    let reader = tokio::spawn(async move {
        let mut input_tx = Some(input_tx);
        let mut dropped = 0u64;
        loop {
            match wire::recv::<_, RpcClientFrame>(&mut source).await {
                Ok(Some((RpcClientFrame::Input { value }, _))) => {
                    if let Some(tx) = input_tx.as_ref() {
                        // Never wait on a slow handler: this loop must keep reading to
                        // notice the client leaving. Input beyond the queue is dropped.
                        match tx.try_send(value) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                dropped += 1;
                                if dropped.is_power_of_two() {
                                    tracing::warn!(dropped, "rpc input queue full; dropping input");
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => input_tx = None,
                        }
                    }
                }
                Ok(Some((RpcClientFrame::EndInput, _))) => input_tx = None,
                // EOF or a broken stream means the caller is gone.
                Ok(None) | Err(_) => {
                    reader_cancel.cancel();
                    return;
                }
            }
        }
    });
    let replies = tokio::select! {
        replies = handler.call(request, RpcInput { receiver }) => replies,
        () = cancel.cancelled() => {
            reader.abort();
            return;
        }
    };
    let outcome = match replies {
        Ok(mut replies) => loop {
            let item = tokio::select! {
                item = replies.next() => item,
                () = cancel.cancelled() => break Err(()),
            };
            let frame = match item {
                Some(Ok(value)) => RpcBoxFrame::Item { value },
                Some(Err(error)) => {
                    break send_final(&mut sink, RpcBoxFrame::Error { error }).await;
                }
                None => break send_final(&mut sink, RpcBoxFrame::End).await,
            };
            if wire::send(&mut sink, &frame).await.is_err() {
                break Err(());
            }
        },
        Err(error) => send_final(&mut sink, RpcBoxFrame::Error { error }).await,
    };
    if outcome.is_ok() {
        let _ = sink.close().await;
    }
    reader.abort();
}

/// Sends the terminal frame of a call.
async fn send_final(
    sink: &mut SplitSink<Channel<MuxStream>, wire::Frame>,
    frame: RpcBoxFrame,
) -> Result<(), ()> {
    wire::send(sink, &frame).await.map_err(|_| ())
}

/// Failure of a forwarded call as seen by the client.
#[derive(Debug, thiserror::Error)]
pub enum RpcCallError {
    /// The box's handler failed.
    #[error("remote error: {0}")]
    Remote(#[from] RpcError),
    /// The stream broke or carried invalid frames.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The stream ended without a terminal frame, usually because the session died.
    #[error("call ended without a reply")]
    Interrupted,
    /// A unary call produced no item before ending.
    #[error("call produced no value")]
    NoValue,
    /// Opening the stream failed.
    #[error(transparent)]
    Session(#[from] crate::session::SessionError),
}

/// A client call in progress. Dropping it cancels the call on the box.
pub struct RpcCall {
    sender: RpcSender,
    receiver: RpcReceiver,
}

impl RpcCall {
    /// Wraps a stream whose request frame was already sent.
    pub(crate) fn new(channel: Channel<MuxStream>) -> Self {
        let (sink, stream) = channel.split();
        Self {
            sender: RpcSender { sink, ended: false },
            receiver: RpcReceiver {
                stream,
                done: false,
            },
        }
    }

    /// Sends one more input value to the handler.
    pub async fn send(&mut self, value: Value) -> Result<(), RpcCallError> {
        self.sender.send(value).await
    }

    /// Signals that no more input follows; replies keep flowing.
    pub async fn finish_input(&mut self) -> Result<(), RpcCallError> {
        self.sender.finish().await
    }

    /// Next reply; `None` after the handler finished successfully.
    pub async fn next(&mut self) -> Option<Result<Value, RpcCallError>> {
        self.receiver.next().await
    }

    /// Splits for concurrent input and output; dropping both halves cancels.
    pub fn into_split(self) -> (RpcSender, RpcReceiver) {
        (self.sender, self.receiver)
    }
}

/// Input half of an [`RpcCall`].
pub struct RpcSender {
    sink: SplitSink<Channel<MuxStream>, wire::Frame>,
    ended: bool,
}

impl RpcSender {
    /// Sends one input value.
    pub async fn send(&mut self, value: Value) -> Result<(), RpcCallError> {
        wire::send(&mut self.sink, &RpcClientFrame::Input { value }).await?;
        Ok(())
    }

    /// Ends input. The write half stays open: EOF would read as cancellation.
    pub async fn finish(&mut self) -> Result<(), RpcCallError> {
        if !self.ended {
            wire::send(&mut self.sink, &RpcClientFrame::EndInput).await?;
            self.ended = true;
        }
        Ok(())
    }
}

/// Output half of an [`RpcCall`].
pub struct RpcReceiver {
    stream: SplitStream<Channel<MuxStream>>,
    done: bool,
}

impl RpcReceiver {
    /// Next reply; `None` after the handler finished successfully.
    pub async fn next(&mut self) -> Option<Result<Value, RpcCallError>> {
        if self.done {
            return None;
        }
        let result = match wire::recv::<_, RpcBoxFrame>(&mut self.stream).await {
            Ok(Some((RpcBoxFrame::Item { value }, _))) => return Some(Ok(value)),
            Ok(Some((RpcBoxFrame::End, _))) => None,
            Ok(Some((RpcBoxFrame::Error { error }, _))) => Some(Err(error.into())),
            Ok(None) => Some(Err(RpcCallError::Interrupted)),
            Err(error) => Some(Err(error.into())),
        };
        self.done = true;
        result
    }
}
