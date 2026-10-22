//! The byte-pipe abstraction every remote transport implements.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Boxed error used where the concrete cause belongs to an integration crate.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A bidirectional byte stream that can move between tasks.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

/// Type-erased duplex returned by [`Transport::connect`].
pub type BoxDuplex = Box<dyn Duplex>;

/// Produces one authenticated duplex byte stream to a box per call.
///
/// Implementations do not interpret the bytes; the mux and protocol run on top.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Opens a fresh pipe to the box.
    ///
    /// Return [`TransportError::Permanent`] when retrying cannot help (missing
    /// binary, rejected credentials) so reconnect loops stop instead of spinning.
    async fn connect(&self) -> Result<BoxDuplex, TransportError>;

    /// Problems that do not stop connecting but should be shown with the
    /// connection, such as a pipe running without the usual cleanup. Empty by
    /// default.
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Failure to produce a pipe, classified for reconnect decisions.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Local or network I/O failed; retrying later may succeed.
    #[error("transport I/O: {0}")]
    Io(#[from] io::Error),
    /// A failure that may clear up on its own, such as an unreachable host.
    #[error(transparent)]
    Transient(BoxError),
    /// A failure that needs user action before another attempt makes sense.
    #[error(transparent)]
    Permanent(BoxError),
}

impl TransportError {
    /// Wraps a cause that a later retry may not hit again.
    pub fn transient(error: impl Into<BoxError>) -> Self {
        Self::Transient(error.into())
    }

    /// Wraps a cause that requires user action, stopping reconnect loops.
    pub fn permanent(error: impl Into<BoxError>) -> Self {
        Self::Permanent(error.into())
    }

    /// Reports whether reconnecting without user action is pointless.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Permanent(_))
    }

    /// Returns the integration-specific cause when it has type `E`.
    ///
    /// Callers use this to recover typed details such as an SSH bootstrap plan.
    pub fn downcast_ref<E: std::error::Error + 'static>(&self) -> Option<&E> {
        match self {
            Self::Io(_) => None,
            Self::Transient(error) | Self::Permanent(error) => error.downcast_ref(),
        }
    }
}

/// Joins a separate reader and writer, such as child stdio, into one duplex.
#[derive(Debug)]
pub struct Joined<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> Joined<R, W> {
    /// Combines `reader` and `writer`; shutting down the duplex shuts down `writer`.
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Returns the original halves.
    pub fn into_parts(self) -> (R, W) {
        (self.reader, self.writer)
    }
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for Joined<R, W> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for Joined<R, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// The process's stdin and stdout as one duplex, for `devlanes tunnel --stdio` on the box.
pub fn stdio() -> Joined<tokio::io::Stdin, tokio::io::Stdout> {
    Joined::new(tokio::io::stdin(), tokio::io::stdout())
}
