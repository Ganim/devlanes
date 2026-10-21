//! Bounded framing shared by Unix sockets and tunneled daemon connections.

use bytes::{BufMut, Bytes, BytesMut};
use devlanes_proto::{Control, MAX_FRAME_SIZE};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Protocol violations close the connection rather than attempting resynchronization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IPC I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid control frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid frame kind or length")]
    InvalidFrame,
    #[error(
        "a message exceeded the {} MiB frame limit; ask for less (a smaller --tail or --budget) or page with --cursor",
        devlanes_proto::MAX_FRAME_SIZE / (1024 * 1024)
    )]
    TooLarge,
    #[error("peer disconnected")]
    Disconnected,
}

/// A stream identifier distinguishes simultaneous terminal byte streams.
// Frames are built and consumed once per message; boxing Control would add an
// allocation per request for no memory win, since frames are never stored in bulk.
#[allow(clippy::large_enum_variant)]
pub enum Frame {
    Control(Control),
    Bytes { stream_id: u64, data: Bytes },
}

/// The codec bounds frame allocation before handing bytes to serde.
pub struct Connection<T> {
    framed: Framed<T, LengthDelimitedCodec>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Connection<T> {
    /// Wraps a connected stream; callers perform hello negotiation before requests.
    pub fn new(stream: T) -> Self {
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME_SIZE)
            .new_codec();
        Self {
            framed: Framed::new(stream, codec),
        }
    }

    /// Sends a complete bounded frame with backpressure from the peer.
    pub async fn send(&mut self, frame: Frame) -> Result<(), Error> {
        self.framed.send(encode(frame)?).await?;
        Ok(())
    }

    /// Reads one frame, distinguishing orderly EOF from malformed input.
    pub async fn recv(&mut self) -> Result<Frame, Error> {
        match self.framed.next().await {
            Some(Ok(bytes)) => decode(bytes.freeze()),
            Some(Err(error)) => Err(error.into()),
            None => Err(Error::Disconnected),
        }
    }
}

/// Rejects oversized outgoing frames before enqueueing them on the transport.
fn encode(frame: Frame) -> Result<Bytes, Error> {
    let mut bytes = BytesMut::new();
    match frame {
        Frame::Control(control) => {
            bytes.put_u8(0);
            bytes.extend_from_slice(&serde_json::to_vec(&control)?);
        }
        Frame::Bytes { stream_id, data } => {
            if data.len() > MAX_FRAME_SIZE - 9 {
                return Err(Error::TooLarge);
            }
            bytes.reserve(9 + data.len());
            bytes.put_u8(1);
            bytes.put_u64(stream_id);
            bytes.extend_from_slice(&data);
        }
    }
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(Error::TooLarge);
    }
    Ok(bytes.freeze())
}

/// Binary headers are network byte order so remote peers need no platform assumptions.
fn decode(bytes: Bytes) -> Result<Frame, Error> {
    match bytes.first() {
        Some(0) => Ok(Frame::Control(serde_json::from_slice(&bytes[1..])?)),
        Some(1) if bytes.len() >= 9 => {
            let header: [u8; 8] = bytes[1..9].try_into().map_err(|_| Error::InvalidFrame)?;
            Ok(Frame::Bytes {
                stream_id: u64::from_be_bytes(header),
                data: bytes.slice(9..),
            })
        }
        _ => Err(Error::InvalidFrame),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binary_payload_roundtrips_without_utf8_conversion() -> Result<(), Error> {
        let (a, b) = tokio::io::duplex(64);
        let mut a = Connection::new(a);
        let mut b = Connection::new(b);
        a.send(Frame::Bytes {
            stream_id: 42,
            data: Bytes::from_static(&[0, 255, 27]),
        })
        .await?;
        match b.recv().await? {
            Frame::Bytes { stream_id, data } => {
                assert_eq!(stream_id, 42);
                assert_eq!(data.as_ref(), &[0, 255, 27]);
            }
            _ => panic!("binary frame changed kind"),
        }
        Ok(())
    }

    #[test]
    fn rejects_short_and_unknown_frames() {
        for bytes in [vec![], vec![2], vec![1, 0]] {
            assert!(matches!(decode(bytes.into()), Err(Error::InvalidFrame)));
        }
    }

    #[tokio::test]
    async fn length_limit_precedes_payload_allocation() -> Result<(), std::io::Error> {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_u32((MAX_FRAME_SIZE + 1) as u32).await?;
        let result = Connection::new(reader).recv().await;
        assert!(matches!(result, Err(Error::Io(_))));
        Ok(())
    }
}
