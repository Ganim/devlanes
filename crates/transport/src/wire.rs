//! Per-stream framing and the messages exchanged over the mux.
//!
//! Every mux stream starts with framed messages. A frame body is a big-endian
//! `u32` header length, a JSON header, then an optional binary payload, so
//! sync chunks travel without base64 while everything else stays inspectable.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::codec::{Decoder, Encoder, Framed, LengthDelimitedCodec};

use crate::registry::{RegistryEvent, RegistrySnapshot};

/// Wire protocol revision; both ends must match exactly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Frames above this size are rejected before allocation.
pub(crate) const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// Malformed or failed framing on one mux stream.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// Reading or writing the stream failed.
    #[error("stream I/O: {0}")]
    Io(#[from] io::Error),
    /// A header was not the expected JSON message.
    #[error("invalid message: {0}")]
    Json(#[from] serde_json::Error),
    /// The frame layout was inconsistent.
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
}

/// One decoded frame: a JSON header and raw payload bytes.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    header: Bytes,
    payload: Bytes,
}

impl Frame {
    /// Serializes `message` as the header of a frame carrying `payload`.
    pub(crate) fn new<T: Serialize>(message: &T, payload: Bytes) -> Result<Self, WireError> {
        Ok(Self {
            header: Bytes::from(serde_json::to_vec(message)?),
            payload,
        })
    }

    /// Parses the header as `T` and returns it with the payload.
    pub(crate) fn decode<T: DeserializeOwned>(self) -> Result<(T, Bytes), WireError> {
        Ok((serde_json::from_slice(&self.header)?, self.payload))
    }
}

/// Length-delimited codec for [`Frame`] values.
pub(crate) struct FrameCodec {
    inner: LengthDelimitedCodec,
}

impl FrameCodec {
    /// Builds a codec bounded by [`MAX_FRAME_LEN`].
    pub(crate) fn new() -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .max_frame_length(MAX_FRAME_LEN)
                .new_codec(),
        }
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = WireError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, WireError> {
        let Some(mut body) = self.inner.decode(src)? else {
            return Ok(None);
        };
        if body.len() < 4 {
            return Err(WireError::Malformed("missing header length"));
        }
        let header_len = body.get_u32() as usize;
        if header_len > body.len() {
            return Err(WireError::Malformed("header length exceeds frame"));
        }
        let header = body.split_to(header_len).freeze();
        Ok(Some(Frame {
            header,
            payload: body.freeze(),
        }))
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = WireError;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), WireError> {
        let header_len = u32::try_from(frame.header.len())
            .map_err(|_| WireError::Malformed("header too large"))?;
        let mut body = BytesMut::with_capacity(4 + frame.header.len() + frame.payload.len());
        body.put_u32(header_len);
        body.extend_from_slice(&frame.header);
        body.extend_from_slice(&frame.payload);
        // The inner codec enforces MAX_FRAME_LEN and reports it as an I/O error.
        self.inner.encode(body.freeze(), dst)?;
        Ok(())
    }
}

/// A mux stream wrapped in [`FrameCodec`].
pub(crate) type Channel<S> = Framed<S, FrameCodec>;

/// Starts framing on `io`.
pub(crate) fn channel<S: AsyncRead + AsyncWrite>(io: S) -> Channel<S> {
    Framed::new(io, FrameCodec::new())
}

/// Sends one message without payload.
pub(crate) async fn send<K, T>(sink: &mut K, message: &T) -> Result<(), WireError>
where
    K: Sink<Frame, Error = WireError> + Unpin,
    T: Serialize,
{
    sink.send(Frame::new(message, Bytes::new())?).await
}

/// Sends one message followed by binary `payload`.
pub(crate) async fn send_with_payload<K, T>(
    sink: &mut K,
    message: &T,
    payload: Bytes,
) -> Result<(), WireError>
where
    K: Sink<Frame, Error = WireError> + Unpin,
    T: Serialize,
{
    sink.send(Frame::new(message, payload)?).await
}

/// Receives the next message; `Ok(None)` means the peer closed or reset the stream.
///
/// Cancel-safe: the only await point is the underlying `next()`.
pub(crate) async fn recv<S, T>(stream: &mut S) -> Result<Option<(T, Bytes)>, WireError>
where
    S: Stream<Item = Result<Frame, WireError>> + Unpin,
    T: DeserializeOwned,
{
    match stream.next().await {
        Some(frame) => frame?.decode().map(Some),
        None => Ok(None),
    }
}

/// A stream whose first reads return bytes already buffered by the framer.
///
/// Needed when a stream switches from framed messages to raw bytes: the peer
/// may have sent data right after its last frame, such as a server greeting.
#[derive(Debug)]
pub struct Prefixed<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> Prefixed<S> {
    /// Serves `prefix` before reading from `inner`.
    pub(crate) fn new(prefix: Bytes, inner: S) -> Self {
        Self { prefix, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.prefix.is_empty() {
            let n = this.prefix.len().min(buf.remaining());
            buf.put_slice(&this.prefix.split_to(n));
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Unwraps a framed channel into raw bytes, keeping anything already read.
pub(crate) fn into_raw<S>(channel: Channel<S>) -> Prefixed<S> {
    let parts = channel.into_parts();
    Prefixed::new(parts.read_buf.freeze(), parts.io)
}

/// The first frame on every client-opened stream; selects the stream's role.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "open", rename_all = "snake_case")]
pub(crate) enum OpenRequest {
    /// The single control stream; must be the first stream of a session.
    Control { protocol: u32, client: String },
    /// One forwarded daemon request.
    Rpc { request: Value },
    /// A raw byte stream to a registered endpoint.
    Data { target: DialTarget },
    /// A sync subscription; the request is interpreted by the box's sync source.
    Sync { request: Value },
}

/// Messages the client sends on the control stream after opening it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClientControl {
    Ping { nonce: u64 },
}

/// Messages the box sends on the control stream.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BoxControl {
    Welcome { protocol: u32, server: String },
    Incompatible { protocol: u32, server: String },
    Snapshot { snapshot: RegistrySnapshot },
    Event { event: RegistryEvent },
    Pong { nonce: u64 },
}

/// What a data stream connects to on the box.
///
/// Tagged so later targets (Unix sockets, reverse forwards) stay compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DialTarget {
    /// A loopback TCP port on the box that some non-hidden unit advertises.
    Port { port: u16 },
}

/// Why the box refused a data stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// No unit in the box's registry advertises the target.
    NotRegistered,
    /// Only hidden units advertise the target.
    Hidden,
    /// The target is registered, but dialing it failed.
    DialFailed,
}

/// The box's answer to a data-stream open.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OpenReply {
    Accepted,
    Rejected {
        reason: RejectReason,
        message: String,
    },
}

/// Client frames on an RPC stream after the request.
///
/// EOF without `EndInput` means the client dropped the call, which cancels it:
/// yamux reports a reset as EOF, so the explicit marker is the only way to
/// tell "no more input" from "caller is gone".
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum RpcClientFrame {
    Input { value: Value },
    EndInput,
}

/// Box frames on an RPC stream.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum RpcBoxFrame {
    Item { value: Value },
    Error { error: crate::rpc::RpcError },
    End,
}

/// Box frames on a sync stream. `Chunk` carries file bytes as the payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SyncFrame {
    Manifest {
        manifest: Value,
    },
    Chunk {
        path: String,
        offset: u64,
        last: bool,
    },
    Delete {
        path: String,
    },
    Error {
        error: crate::sync::SyncError,
    },
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn payload_survives_framing() -> Result<(), WireError> {
        let (a, b) = tokio::io::duplex(1024);
        let mut a = channel(a);
        let mut b = channel(b);
        let message = SyncFrame::Chunk {
            path: "a/b".into(),
            offset: 7,
            last: true,
        };
        send_with_payload(&mut a, &message, Bytes::from_static(&[0, 255, 1])).await?;
        let Some((SyncFrame::Chunk { path, offset, last }, payload)) = recv(&mut b).await? else {
            panic!("chunk frame changed kind");
        };
        assert_eq!((path.as_str(), offset, last), ("a/b", 7, true));
        assert_eq!(payload.as_ref(), &[0, 255, 1]);
        Ok(())
    }

    #[test]
    fn rejects_header_longer_than_frame() {
        let mut codec = FrameCodec::new();
        let mut src = BytesMut::new();
        src.put_u32(4);
        src.put_u32(10);
        assert!(matches!(
            codec.decode(&mut src),
            Err(WireError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn raw_switch_keeps_buffered_bytes() -> Result<(), WireError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a, b) = tokio::io::duplex(1024);
        let mut a = channel(a);
        send(&mut a, &OpenReply::Accepted).await?;
        let mut a = a.into_inner();
        a.write_all(b"greeting").await?;
        let mut b = channel(b);
        // Wait for the greeting so the framer has buffered it with the reply.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let reply: Option<(OpenReply, Bytes)> = recv(&mut b).await?;
        assert!(matches!(reply, Some((OpenReply::Accepted, _))));
        let mut raw = into_raw(b);
        let mut out = [0u8; 8];
        raw.read_exact(&mut out).await?;
        assert_eq!(&out, b"greeting");
        Ok(())
    }
}
