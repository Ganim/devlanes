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
///
/// Covers the framing here and the opaque payloads devlanes puts in them
/// (`devlanes_proto::remote`: sync requests and manifests). It changes only for
/// breaking changes; additive behaviour is a named feature instead (see
/// [`FEATURES`]). Revision 2 added sync manifests sent by the client and
/// `not_running` refusals; revision 3 streams the client's manifest in batches as
/// sync input frames, ended by an explicit marker; revision 4 adds the tunnel
/// preamble, features, client identity and box info to the handshake, task units
/// to the registry, and removes daemon request streams.
pub const PROTOCOL_VERSION: u32 = 4;

/// Additive protocol features this build implements, listed by both ends in the
/// handshake. A side uses a feature only when the other side lists it too.
pub const FEATURES: &[&str] = &[];

/// Names in both lists, sorted and without repeats: the features a session uses.
pub(crate) fn negotiate(ours: &[String], theirs: &[String]) -> Vec<String> {
    let mut shared: Vec<String> = ours
        .iter()
        .filter(|name| theirs.contains(name))
        .cloned()
        .collect();
    shared.sort();
    shared.dedup();
    shared
}

/// What the box reports about itself in the handshake. Every field is optional,
/// so a box that cannot tell leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxInfo {
    /// Port the box's proxy serves HTTPS on, as in the URLs it generates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https_port: Option<u16>,
    /// The box's `ports.range` user setting, such as `20000-29999`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports_range: Option<String>,
}

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
    Control {
        protocol: u32,
        /// Version label, such as `devlanes 0.5.0`.
        client: String,
        /// Every feature the client implements.
        #[serde(default)]
        features: Vec<String>,
        /// Random id the client machine keeps across restarts.
        #[serde(default)]
        client_id: String,
        /// The client machine's host name.
        #[serde(default)]
        client_name: String,
    },
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
    Welcome {
        protocol: u32,
        /// Version label, such as `devlanes 0.5.0`.
        server: String,
        /// Every feature the box implements.
        #[serde(default)]
        features: Vec<String>,
        #[serde(default, rename = "box")]
        box_info: BoxInfo,
    },
    Incompatible {
        protocol: u32,
        server: String,
    },
    Snapshot {
        snapshot: RegistrySnapshot,
    },
    Event {
        event: RegistryEvent,
    },
    Pong {
        nonce: u64,
    },
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
    /// The unit advertising the target is not running.
    NotRunning,
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

/// Client frames on a sync stream after the request, such as manifest batches.
///
/// EOF without `EndInput` means the client left, which cancels the subscription:
/// yamux reports a reset as EOF, so the explicit marker is the only way to say
/// that no more input follows while still reading.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SyncClientFrame {
    Input { value: Value },
    EndInput,
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

    #[test]
    fn negotiation_keeps_features_both_sides_list() {
        let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
        let ours: Vec<String> = names(&["b", "a", "c"]);
        assert_eq!(negotiate(&ours, &names(&["c", "a", "x"])), ["a", "c"]);
        assert!(negotiate(&ours, &[]).is_empty());
        assert!(negotiate(&[], &ours).is_empty());
    }

    /// Fields added in revision 4 have defaults, so a handshake without them
    /// still decodes far enough to compare protocol revisions.
    #[test]
    fn handshake_fields_are_optional() -> Result<(), serde_json::Error> {
        let hello: OpenRequest = serde_json::from_value(serde_json::json!({
            "open": "control", "protocol": 3, "client": "devlanes 0.4.0", "later": true
        }))?;
        assert!(matches!(
            hello,
            OpenRequest::Control { protocol: 3, ref features, ref client_id, .. }
                if features.is_empty() && client_id.is_empty()
        ));
        let welcome: BoxControl = serde_json::from_value(serde_json::json!({
            "type": "welcome", "protocol": 4, "server": "devlanes 0.5.0"
        }))?;
        assert!(matches!(
            welcome,
            BoxControl::Welcome { ref box_info, .. } if *box_info == BoxInfo::default()
        ));
        let encoded = serde_json::to_value(BoxControl::Welcome {
            protocol: 4,
            server: "box".into(),
            features: vec!["x".into()],
            box_info: BoxInfo {
                https_port: Some(8443),
                ports_range: None,
            },
        })?;
        assert_eq!(encoded["box"], serde_json::json!({"https_port": 8443}));
        Ok(())
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
