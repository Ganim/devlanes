use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Instant,
};

/// Receives activity the proxy sees for each lane, keyed by `Route::owner`
/// (the lane reference, such as `acme@0`).
pub trait LaneActivity: Send + Sync {
    /// Records a request for the lane that owns the route. Returns a future to
    /// await before forwarding when the lane is frozen and is being thawed.
    fn request(&self, owner: &str) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>>;
    /// Records client bytes on an upgraded stream (WebSocket, other Upgrade).
    fn stream(&self, owner: &str);
}

/// Idle timers count in minutes, so one report per second per stream is enough and keeps
/// chatty WebSockets from calling the receiver on every read.
const REPORT_EVERY: Duration = Duration::from_secs(1);

/// The client side of an upgraded stream, reporting bytes read from it to the owning lane.
pub(crate) struct ClientReads<I> {
    inner: I,
    lane: Option<(Arc<dyn LaneActivity>, String)>,
    reported: Option<Instant>,
}

impl<I> ClientReads<I> {
    /// Reports nothing when `lane` is `None`.
    pub(crate) fn new(inner: I, lane: Option<(Arc<dyn LaneActivity>, String)>) -> Self {
        Self {
            inner,
            lane,
            reported: None,
        }
    }

    /// Calls `LaneActivity::stream` unless this stream reported less than a second ago.
    fn report(&mut self) {
        let Some((activity, owner)) = &self.lane else {
            return;
        };
        let now = Instant::now();
        if self
            .reported
            .is_some_and(|last| now.duration_since(last) < REPORT_EVERY)
        {
            return;
        }
        self.reported = Some(now);
        activity.stream(owner);
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for ClientReads<I> {
    /// Reports only reads that returned bytes; EOF and errors are not activity.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.report();
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for ClientReads<I> {
    /// Bytes sent to the client are not activity, so writes pass straight through.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    /// Delegates to the wrapped stream.
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    /// Delegates to the wrapped stream.
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    /// Delegates to the wrapped stream.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    /// Delegates to the wrapped stream, so half-close reaches the client unchanged.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Collects every owner passed to `stream`.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl LaneActivity for Recorder {
        /// Never asks the proxy to wait.
        fn request(&self, _owner: &str) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
            None
        }

        /// Records the owner.
        fn stream(&self, owner: &str) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(owner.to_string());
        }
    }

    /// Reads with bytes report once per second, writes and EOF never do, and
    /// `copy_bidirectional` still sees both half-closes.
    #[tokio::test]
    async fn reports_client_reads_only() -> Result<(), Box<dyn std::error::Error>> {
        // Paused, so the two client writes are less than a second apart even
        // on a stalled machine.
        tokio::time::pause();
        tokio::time::timeout(Duration::from_secs(5), async {
            let recorder = Arc::new(Recorder::default());
            let (client, proxy_side) = tokio::io::duplex(64);
            let (upstream, mut upstream_side) = tokio::io::duplex(64);
            let lane: Arc<dyn LaneActivity> = recorder.clone();
            let mut reads = ClientReads::new(proxy_side, Some((lane, "acme@0".to_string())));
            let bridge = tokio::spawn(async move {
                tokio::io::copy_bidirectional(&mut reads, &mut upstream_side).await
            });
            let (mut client_read, mut client_write) = tokio::io::split(client);
            let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
            upstream_write.write_all(b"down").await?;
            let mut buffer = [0u8; 4];
            client_read.read_exact(&mut buffer).await?;
            assert!(recorder.0.lock().map_err(|_| "poisoned")?.is_empty());
            client_write.write_all(b"up").await?;
            upstream_read.read_exact(&mut buffer[..2]).await?;
            client_write.write_all(b"again").await?;
            let mut again = [0u8; 5];
            upstream_read.read_exact(&mut again).await?;
            assert_eq!(*recorder.0.lock().map_err(|_| "poisoned")?, ["acme@0"]);
            client_write.shutdown().await?;
            upstream_write.shutdown().await?;
            let mut rest = Vec::new();
            upstream_read.read_to_end(&mut rest).await?;
            client_read.read_to_end(&mut rest).await?;
            assert!(rest.is_empty());
            assert_eq!(bridge.await??, (7, 4));
            assert_eq!(recorder.0.lock().map_err(|_| "poisoned")?.len(), 1);
            Ok(())
        })
        .await?
    }
}
