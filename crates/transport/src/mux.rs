//! yamux driven from an actor task.
//!
//! yamux 0.14 has no `Control` handle: the `Connection` must be polled
//! continuously and outbound streams are opened through `poll_new_outbound` on
//! the same object. One task owns the connection; handles talk to it over
//! channels.

use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;

use crate::transport::BoxDuplex;

/// Streams per session; browsers plus database pools rarely exceed this.
const MAX_STREAMS: usize = 512;

/// yamux auto-tunes each stream window from 256 KiB; this budget lets every
/// stream grow to at least 4 MiB so one high-RTT SSH hop does not cap throughput.
const STREAM_WINDOW_FLOOR: usize = 4 * 1024 * 1024;

/// How long a graceful yamux close may take before the socket is dropped.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// The mux failed or is gone.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// The connection is closed; no further streams can be opened.
    #[error("mux connection closed")]
    Closed,
    /// yamux reported a connection-level failure.
    #[error("mux: {0}")]
    Connection(#[from] yamux::ConnectionError),
}

/// One multiplexed stream.
pub struct MuxStream {
    inner: Compat<yamux::Stream>,
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
}

impl MuxClient {
    /// Opens a new outbound stream.
    pub(crate) async fn open(&self) -> Result<MuxStream, MuxError> {
        let (reply, response) = oneshot::channel();
        self.open.send(reply).await.map_err(|_| MuxError::Closed)?;
        let stream = response.await.map_err(|_| MuxError::Closed)??;
        Ok(MuxStream::new(stream))
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
}

impl MuxServer {
    /// Waits for the next client stream; `None` once the connection is gone.
    pub(crate) async fn accept(&mut self) -> Option<MuxStream> {
        self.inbound.recv().await.map(MuxStream::new)
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
    config.set_max_num_streams(MAX_STREAMS);
    config.set_max_connection_receive_window(Some(MAX_STREAMS * STREAM_WINDOW_FLOOR));
    config
}

/// Starts a client-mode driver over `io`.
pub(crate) fn client(io: BoxDuplex) -> MuxClient {
    let (open, open_rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let done = CancellationToken::new();
    let connection = yamux::Connection::new(io.compat(), config(), yamux::Mode::Client);
    tokio::spawn(drive(
        connection,
        Some(open_rx),
        None,
        shutdown.clone(),
        done.clone(),
    ));
    MuxClient {
        open,
        shutdown,
        done,
    }
}

/// Starts a server-mode driver over `io`.
pub(crate) fn server(io: BoxDuplex) -> MuxServer {
    let (inbound_tx, inbound) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let connection = yamux::Connection::new(io.compat(), config(), yamux::Mode::Server);
    tokio::spawn(drive(
        connection,
        None,
        Some(inbound_tx),
        shutdown.clone(),
        CancellationToken::new(),
    ));
    MuxServer { inbound, shutdown }
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
) where
    T: futures_util::AsyncRead + futures_util::AsyncWrite + Unpin,
{
    let mut pending: Option<OpenReply> = None;
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
                        Poll::Ready(result) => {
                            let failed = result.is_err();
                            // A dropped requester just discards the stream, which resets it.
                            let _ = reply.send(result.map_err(MuxError::from));
                            if failed {
                                return Poll::Ready(Err(MuxError::Closed));
                            }
                            progressed = true;
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
                        // Only the client opens streams; the box opening one is ignored.
                        None => tracing::warn!("dropping stream opened by the box"),
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
}
