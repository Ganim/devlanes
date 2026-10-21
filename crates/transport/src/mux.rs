//! yamux driven from an actor task.
//!
//! yamux 0.14 has no `Control` handle: the `Connection` must be polled
//! continuously and outbound streams are opened through `poll_new_outbound` on
//! the same object. One task owns the connection; handles talk to it over
//! channels.

use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;

use crate::transport::BoxDuplex;

/// Streams per session; browsers plus database pools rarely exceed this.
pub(crate) const MAX_STREAMS: usize = 512;

/// yamux's own limit sits above [`MAX_STREAMS`]: yamux treats hitting it on an
/// open as fatal for the whole connection, and it forgets dropped streams only
/// when the driver next polls, so the client refuses opens itself with headroom.
const YAMUX_STREAMS: usize = MAX_STREAMS + 64;

/// yamux auto-tunes each stream window from 256 KiB; this budget lets every
/// stream grow to at least 4 MiB so one high-RTT SSH hop does not cap throughput.
const STREAM_WINDOW_FLOOR: usize = 4 * 1024 * 1024;

/// How long a graceful yamux close may take before the socket is dropped.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Streams the box may open before the client treats it as misbehaving. Only the
/// client opens streams, so even one is a protocol error; a few are tolerated
/// (and logged once) in case a future box probes for features.
const BOX_STREAM_LIMIT: usize = 8;

/// The mux failed or is gone.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// The connection is closed; no further streams can be opened.
    #[error("mux connection closed")]
    Closed,
    /// Every stream slot is in use; this open failed but the connection lives on.
    #[error("too many open streams ({MAX_STREAMS}); close some connections and retry")]
    TooManyStreams,
    /// yamux reported a connection-level failure.
    #[error("mux: {0}")]
    Connection(#[from] yamux::ConnectionError),
    /// The peer broke the protocol, such as a box opening streams.
    #[error("mux: {0}")]
    Misbehaving(&'static str),
}

/// When the pipe last delivered bytes, shared with the session's liveness checks.
///
/// Any inbound frame proves the peer is alive, so a large transfer that delays a
/// pong behind data frames does not count as a dead link.
#[derive(Debug)]
pub(crate) struct Activity {
    last: Mutex<Instant>,
}

impl Activity {
    /// Starts at "now".
    fn new() -> Arc<Self> {
        Arc::new(Self {
            last: Mutex::new(Instant::now()),
        })
    }

    /// Records inbound bytes.
    fn touch(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// When bytes last arrived.
    pub(crate) fn last(&self) -> Instant {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The pipe with its reads recorded in an [`Activity`].
struct Watched {
    inner: BoxDuplex,
    activity: Arc<Activity>,
}

impl AsyncRead for Watched {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.activity.touch();
        }
        poll
    }
}

impl AsyncWrite for Watched {
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

/// One multiplexed stream.
pub struct MuxStream {
    inner: Compat<yamux::Stream>,
    _slot: Option<Slot>,
}

/// A client stream slot, released when the stream drops.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    /// Frees the slot.
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for MuxStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MuxStream")
            .field("id", &self.inner.get_ref().id())
            .finish()
    }
}

impl MuxStream {
    fn new(stream: yamux::Stream) -> Self {
        Self {
            inner: stream.compat(),
            _slot: None,
        }
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MuxStream {
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

    /// Half-closes the stream (yamux FIN); reading continues until the peer closes.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

type OpenReply = oneshot::Sender<Result<yamux::Stream, MuxError>>;

/// Client-side handle; the only side allowed to open streams.
#[derive(Clone)]
pub(crate) struct MuxClient {
    open: mpsc::Sender<OpenReply>,
    shutdown: CancellationToken,
    done: CancellationToken,
    failure: watch::Receiver<Option<String>>,
    activity: Arc<Activity>,
    open_streams: Arc<AtomicUsize>,
}

impl MuxClient {
    /// Why the connection failed, once the driver saw an I/O or protocol error.
    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    /// Waits up to `limit` for the driver to report why the connection ended.
    ///
    /// Streams can see EOF before the driver records the pipe's error (such as
    /// the remote tunnel's stderr), so callers that just lost a stream wait here.
    pub(crate) async fn failure_within(&self, limit: Duration) -> Option<String> {
        let mut failure = self.failure.clone();
        let wait = async {
            tokio::select! {
                reason = failure.wait_for(Option::is_some) => reason.ok().and_then(|r| r.clone()),
                () = self.done.cancelled() => None,
            }
        };
        match tokio::time::timeout(limit, wait).await {
            Ok(Some(reason)) => Some(reason),
            _ => self.failure(),
        }
    }

    /// Opens a new outbound stream.
    ///
    /// A full stream table fails this open with [`MuxError::TooManyStreams`]; the
    /// connection and its other streams are unaffected.
    pub(crate) async fn open(&self) -> Result<MuxStream, MuxError> {
        let taken = self.open_streams.fetch_add(1, Ordering::AcqRel);
        let slot = Slot(self.open_streams.clone());
        if taken >= MAX_STREAMS {
            return Err(MuxError::TooManyStreams);
        }
        let (reply, response) = oneshot::channel();
        self.open.send(reply).await.map_err(|_| MuxError::Closed)?;
        let stream = response.await.map_err(|_| MuxError::Closed)??;
        Ok(MuxStream {
            inner: stream.compat(),
            _slot: Some(slot),
        })
    }

    /// When the box last sent anything on any stream.
    pub(crate) fn last_inbound(&self) -> Instant {
        self.activity.last()
    }

    /// Asks the driver to close the connection.
    pub(crate) fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Resolves once the driver task has exited.
    pub(crate) async fn closed(&self) {
        self.done.cancelled().await;
    }
}

/// Box-side handle yielding client-opened streams.
pub(crate) struct MuxServer {
    inbound: mpsc::UnboundedReceiver<yamux::Stream>,
    shutdown: CancellationToken,
    activity: Arc<Activity>,
}

impl MuxServer {
    /// Waits for the next client stream; `None` once the connection is gone.
    pub(crate) async fn accept(&mut self) -> Option<MuxStream> {
        self.inbound.recv().await.map(MuxStream::new)
    }

    /// When the client last sent anything on any stream.
    pub(crate) fn last_inbound(&self) -> Instant {
        self.activity.last()
    }
}

impl Drop for MuxServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Builds the shared yamux configuration.
fn config() -> yamux::Config {
    let mut config = yamux::Config::default();
    config.set_max_num_streams(YAMUX_STREAMS);
    config.set_max_connection_receive_window(Some(YAMUX_STREAMS * STREAM_WINDOW_FLOOR));
    config
}

/// Wraps `io` so reads are recorded.
fn watched(io: BoxDuplex) -> (Compat<Watched>, Arc<Activity>) {
    let activity = Activity::new();
    let io = Watched {
        inner: io,
        activity: activity.clone(),
    };
    (io.compat(), activity)
}

/// Starts a client-mode driver over `io`.
pub(crate) fn client(io: BoxDuplex) -> MuxClient {
    let (open, open_rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let done = CancellationToken::new();
    let (failure_tx, failure) = watch::channel(None);
    let (io, activity) = watched(io);
    let connection = yamux::Connection::new(io, config(), yamux::Mode::Client);
    tokio::spawn(drive(
        connection,
        Some(open_rx),
        None,
        shutdown.clone(),
        done.clone(),
        failure_tx,
    ));
    MuxClient {
        open,
        shutdown,
        done,
        failure,
        activity,
        open_streams: Arc::new(AtomicUsize::new(0)),
    }
}

/// Starts a server-mode driver over `io`.
pub(crate) fn server(io: BoxDuplex) -> MuxServer {
    let (inbound_tx, inbound) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let (io, activity) = watched(io);
    let connection = yamux::Connection::new(io, config(), yamux::Mode::Server);
    tokio::spawn(drive(
        connection,
        None,
        Some(inbound_tx),
        shutdown.clone(),
        CancellationToken::new(),
        watch::channel(None).0,
    ));
    MuxServer {
        inbound,
        shutdown,
        activity,
    }
}

/// Owns and polls the connection until it ends, the handles drop, or shutdown.
///
/// Inbound streams use an unbounded channel because the poll loop cannot wait;
/// yamux's `MAX_STREAMS` limit bounds it instead.
async fn drive<T>(
    mut connection: yamux::Connection<T>,
    mut open_rx: Option<mpsc::Receiver<OpenReply>>,
    inbound_tx: Option<mpsc::UnboundedSender<yamux::Stream>>,
    shutdown: CancellationToken,
    done: CancellationToken,
    failure: watch::Sender<Option<String>>,
) where
    T: futures_util::AsyncRead + futures_util::AsyncWrite + Unpin,
{
    let mut pending: Option<OpenReply> = None;
    let mut box_streams = 0usize;
    let polled = poll_fn(|cx| {
        loop {
            let mut progressed = false;
            if let Some(open_rx) = open_rx.as_mut() {
                if pending.is_none() {
                    match open_rx.poll_recv(cx) {
                        Poll::Ready(Some(reply)) => pending = Some(reply),
                        Poll::Ready(None) => return Poll::Ready(Ok(())),
                        Poll::Pending => {}
                    }
                }
                if let Some(reply) = pending.take() {
                    match connection.poll_new_outbound(cx) {
                        Poll::Ready(Ok(stream)) => {
                            // A dropped requester just discards the stream, which resets it.
                            let _ = reply.send(Ok(stream));
                            progressed = true;
                        }
                        Poll::Ready(Err(error)) => {
                            let _ = reply.send(Err(MuxError::Closed));
                            return Poll::Ready(Err(MuxError::from(error)));
                        }
                        Poll::Pending => pending = Some(reply),
                    }
                }
            }
            match connection.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => {
                    match inbound_tx.as_ref() {
                        Some(tx) => {
                            if tx.send(stream).is_err() {
                                return Poll::Ready(Ok(()));
                            }
                        }
                        // Only the client opens streams; the box opening one is dropped,
                        // and a box that keeps doing it is disconnected.
                        None => {
                            box_streams += 1;
                            if box_streams == 1 {
                                tracing::warn!("dropping a stream opened by the box");
                            }
                            if box_streams > BOX_STREAM_LIMIT {
                                return Poll::Ready(Err(MuxError::Misbehaving(
                                    "the box keeps opening streams",
                                )));
                            }
                        }
                    }
                    progressed = true;
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(MuxError::from(error))),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => {}
            }
            if !progressed {
                return Poll::Pending;
            }
        }
    });
    let result = tokio::select! {
        result = polled => result,
        () = shutdown.cancelled() => Ok(()),
    };
    if let Err(error) = result {
        tracing::debug!("mux ended: {error}");
        // The pipe's own error carries the box's report (an SSH tunnel's stderr), so
        // it is kept verbatim instead of yamux's "i/o error" wrapper.
        let reason = match error {
            MuxError::Connection(yamux::ConnectionError::Io(io)) => io.to_string(),
            other => other.to_string(),
        };
        failure.send_replace(Some(reason));
    }
    // Fail queued opens promptly instead of leaving callers waiting.
    if let Some(reply) = pending.take() {
        let _ = reply.send(Err(MuxError::Closed));
    }
    if let Some(open_rx) = open_rx.as_mut() {
        open_rx.close();
    }
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, poll_fn(|cx| connection.poll_close(cx))).await;
    done.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn client_streams_reach_server() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (a, b) = tokio::io::duplex(64 * 1024);
            let client = client(Box::new(a));
            let mut server = server(Box::new(b));
            let mut outbound = client.open().await?;
            outbound.write_all(b"ping").await?;
            let mut inbound = server.accept().await.ok_or("no inbound stream")?;
            let mut buf = [0u8; 4];
            inbound.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"ping");
            drop(server);
            client.closed().await;
            assert!(client.open().await.is_err());
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn a_full_stream_table_fails_only_that_open() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (a, b) = tokio::io::duplex(1024 * 1024);
            let client = client(Box::new(a));
            let mut server = server(Box::new(b));
            // Like the real box, answer every stream: yamux stops opening streams
            // while too many wait for their first reply (ACK).
            let (held_tx, mut held) = mpsc::unbounded_channel();
            let answering = tokio::spawn(async move {
                while let Some(mut stream) = server.accept().await {
                    if stream.write_all(b"k").await.is_err() {
                        break;
                    }
                    if held_tx.send(stream).is_err() {
                        break;
                    }
                }
            });
            let mut open = Vec::new();
            let refused = loop {
                match client.open().await {
                    Ok(mut stream) => {
                        // yamux announces a stream with its first frame, as the open
                        // request does in the real protocol.
                        stream.write_all(b"o").await?;
                        let mut ack = [0u8; 1];
                        stream.read_exact(&mut ack).await?;
                        open.push(stream);
                    }
                    Err(error) => break error,
                }
                assert!(
                    open.len() <= MAX_STREAMS,
                    "the stream limit was not enforced"
                );
            };
            assert!(matches!(refused, MuxError::TooManyStreams), "{refused:?}");
            // The first stream still carries data, and freed slots open again.
            open[0].write_all(b"alive").await?;
            let mut inbound = held.recv().await.ok_or("no inbound stream")?;
            let mut opened = [0u8; 1];
            inbound.read_exact(&mut opened).await?;
            let mut buf = [0u8; 5];
            inbound.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"alive");
            // Both ends must close a stream before yamux frees its slot.
            drop(inbound);
            open.clear();
            while let Ok(Some(stream)) =
                tokio::time::timeout(Duration::from_millis(50), held.recv()).await
            {
                drop(stream);
            }
            let reopened = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(mut stream) = client.open().await
                        && stream.write_all(b"o").await.is_ok()
                    {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or(false);
            assert!(reopened, "a closed stream frees its slot");
            answering.abort();
            Ok(())
        })
        .await?
    }
}
