//! Byte copying between a data stream and a socket.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::time::Instant;

/// How long a half-closed connection may stay silent before both sides close.
///
/// A peer that sent FIN but never closes its read side would otherwise hold a mux
/// stream (and a socket) forever.
pub const HALF_CLOSE_IDLE: Duration = Duration::from_secs(60);

/// Copies both ways until both directions finished, or until one finished and
/// nothing moved in either direction for `idle`. Returns bytes copied `a -> b`
/// and `b -> a`.
pub async fn splice<A, B>(a: A, b: B, idle: Duration) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let last = Arc::new(Mutex::new(Instant::now()));
    let (a_read, mut a_write) = tokio::io::split(a);
    let (b_read, mut b_write) = tokio::io::split(b);
    let mut a_read = Touch {
        inner: a_read,
        last: last.clone(),
    };
    let mut b_read = Touch {
        inner: b_read,
        last: last.clone(),
    };
    let forward = async {
        let copied = tokio::io::copy(&mut a_read, &mut b_write).await?;
        b_write.shutdown().await?;
        Ok::<_, io::Error>(copied)
    };
    let backward = async {
        let copied = tokio::io::copy(&mut b_read, &mut a_write).await?;
        a_write.shutdown().await?;
        Ok::<_, io::Error>(copied)
    };
    tokio::pin!(forward);
    tokio::pin!(backward);
    let (mut sent, mut received) = (None, None);
    while sent.is_none() || received.is_none() {
        let half_closed = sent.is_some() || received.is_some();
        let quiet_until = half_closed.then(|| stamp(&last) + idle);
        tokio::select! {
            copied = &mut forward, if sent.is_none() => sent = Some(copied?),
            copied = &mut backward, if received.is_none() => received = Some(copied?),
            () = sleep_until(quiet_until) => {
                if stamp(&last) + idle <= Instant::now() {
                    break;
                }
            }
        }
    }
    Ok((sent.unwrap_or(0), received.unwrap_or(0)))
}

/// The last read time.
fn stamp(last: &Mutex<Instant>) -> Instant {
    *last.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sleeps until `deadline`, or forever when there is none.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// A reader that records when bytes arrive.
struct Touch<R> {
    inner: R,
    last: Arc<Mutex<Instant>>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Touch<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            *this.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn a_silent_half_closed_peer_is_dropped() -> io::Result<()> {
        let (a, mut a_peer) = tokio::io::duplex(1024);
        let (b, mut b_peer) = tokio::io::duplex(1024);
        let spliced = tokio::spawn(splice(a, b, Duration::from_millis(100)));
        a_peer.write_all(b"request").await?;
        // FIN from one side; the other side never answers nor closes.
        a_peer.shutdown().await?;
        let mut seen = [0u8; 7];
        b_peer.read_exact(&mut seen).await?;
        assert_eq!(&seen, b"request");
        let copied = tokio::time::timeout(Duration::from_secs(2), spliced)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)??;
        assert_eq!(copied.0, 7);
        Ok(())
    }

    #[tokio::test]
    async fn traffic_after_half_close_keeps_the_splice() -> io::Result<()> {
        let (a, mut a_peer) = tokio::io::duplex(1024);
        let (b, mut b_peer) = tokio::io::duplex(1024);
        let spliced = tokio::spawn(splice(a, b, Duration::from_millis(150)));
        a_peer.shutdown().await?;
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(80)).await;
            b_peer.write_all(b"x").await?;
        }
        let mut seen = [0u8; 5];
        a_peer.read_exact(&mut seen).await?;
        assert!(!spliced.is_finished(), "a talking peer was cut off");
        drop(b_peer);
        let copied = tokio::time::timeout(Duration::from_secs(2), spliced)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)??;
        assert_eq!(copied.1, 5);
        Ok(())
    }
}
