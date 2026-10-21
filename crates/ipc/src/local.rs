//! The daemon's local endpoint, reachable only by the user that runs it.
//!
//! On Unix it is a domain socket checked with `SO_PEERCRED`. On Windows it will be a
//! named pipe; that transport is not implemented yet, so binding and connecting
//! return [`io::ErrorKind::Unsupported`].
//!
//! Peer checks happen at two points because the platforms differ in when the
//! peer's identity is available. Unix knows it as soon as a connection exists. A
//! Windows pipe server can only impersonate a client after reading from it. The
//! daemon calls [`LocalStream::peer_is_current_user`] right after accepting and
//! [`LocalStream::confirm_peer_after_read`] after the first frame, and each platform
//! does its real check at the point where it can.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Failures creating the daemon's endpoint.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// A live daemon already answers on the endpoint.
    #[error("another daemon is listening on {}", .0.display())]
    AlreadyListening(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A connected local stream, from either side.
pub struct LocalStream(imp::Stream);

/// The daemon's listening endpoint.
pub struct LocalListener(imp::Listener);

impl LocalListener {
    /// Creates the endpoint at `path`.
    ///
    /// On Unix a socket file that refuses connections is left over from a crashed
    /// daemon and is replaced; one that accepts means another daemon owns it. Callers
    /// hold the singleton lock first, so two daemons never race here.
    pub async fn bind(path: &Path) -> Result<Self, BindError> {
        imp::bind(path).await.map(Self)
    }

    /// Waits for the next client. The caller still checks the peer with
    /// [`LocalStream::peer_is_current_user`].
    pub async fn accept(&mut self) -> io::Result<LocalStream> {
        self.0.accept().await.map(LocalStream)
    }

    /// Stops accepting: new clients fail to connect from here on, while connected
    /// clients keep their streams.
    pub fn close(self) {
        self.0.close();
    }
}

/// Connects to the daemon endpoint at `path`. A missing endpoint is
/// [`io::ErrorKind::NotFound`] and a stale one [`io::ErrorKind::ConnectionRefused`],
/// which callers read as "no daemon".
pub async fn connect(path: &Path) -> io::Result<LocalStream> {
    imp::connect(path).await.map(LocalStream)
}

/// Removes what the endpoint leaves behind once its listener is dropped: the socket
/// file on Unix, nothing on Windows. A missing file is not an error.
pub async fn remove(path: &Path) -> io::Result<()> {
    imp::remove(path).await
}

/// Why [`probe`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// Preparing the probe failed before any endpoint was created, for example a
    /// temporary file could not be made next to the socket.
    #[error(transparent)]
    Setup(io::Error),
    /// Creating the endpoint itself failed.
    #[error("creating a local endpoint failed: {0}")]
    Endpoint(io::Error),
}

/// Creates and drops a throwaway endpoint next to `path`. Sandboxes that forbid
/// creating sockets or pipes fail here, before a daemon is spawned that could
/// never listen.
pub fn probe(path: &Path) -> Result<(), ProbeError> {
    imp::probe(path)
}

impl LocalStream {
    /// Whether the peer runs as the current user, checked when the platform can tell
    /// right after connecting: always on Unix, and on Windows for a client checking
    /// the daemon. A Windows daemon defers its check to
    /// [`LocalStream::confirm_peer_after_read`] and reports `true` here.
    pub fn peer_is_current_user(&self) -> io::Result<bool> {
        self.0.peer_is_current_user()
    }

    /// The daemon's check after reading the client's first frame. On Unix the accept
    /// check already covered it and this reports `true`.
    pub fn confirm_peer_after_read(&self) -> io::Result<bool> {
        self.0.confirm_peer_after_read()
    }
}

impl AsyncRead for LocalStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::{UnixListener, UnixStream};

    use super::{BindError, ProbeError};

    pub(super) struct Stream(UnixStream);

    pub(super) struct Listener(UnixListener);

    pub(super) async fn bind(path: &Path) -> Result<Listener, BindError> {
        match UnixStream::connect(path).await {
            Ok(_) => return Err(BindError::AlreadyListening(path.to_owned())),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                tokio::fs::remove_file(path).await?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Listener(UnixListener::bind(path)?))
    }

    impl Listener {
        pub(super) async fn accept(&mut self) -> io::Result<Stream> {
            let (stream, _) = self.0.accept().await?;
            Ok(Stream(stream))
        }

        pub(super) fn close(self) {
            drop(self.0);
        }
    }

    pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path).await.map(Stream)
    }

    pub(super) async fn remove(path: &Path) -> io::Result<()> {
        match tokio::fs::remove_file(path).await {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    pub(super) fn probe(path: &Path) -> Result<(), ProbeError> {
        let parent = path.parent().ok_or_else(|| {
            ProbeError::Setup(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path has no parent",
            ))
        })?;
        let probe = tempfile::Builder::new()
            .prefix("p")
            .rand_bytes(3)
            .tempfile_in(parent)
            .map_err(ProbeError::Setup)?
            .into_temp_path();
        std::fs::remove_file(&probe).map_err(ProbeError::Setup)?;
        // Dropping `probe` after the listener deletes the socket file the bind created.
        let listener =
            std::os::unix::net::UnixListener::bind(&probe).map_err(ProbeError::Endpoint)?;
        drop(listener);
        Ok(())
    }

    impl Stream {
        pub(super) fn peer_is_current_user(&self) -> io::Result<bool> {
            Ok(self.0.peer_cred()?.uid() == rustix::process::geteuid().as_raw())
        }

        pub(super) fn confirm_peer_after_read(&self) -> io::Result<bool> {
            Ok(true)
        }
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::{BindError, ProbeError};

    /// Uninhabited until the named-pipe transport lands, so no stream ever exists.
    pub(super) enum Stream {}

    pub(super) enum Listener {}

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "the daemon's named-pipe endpoint is not implemented yet",
        )
    }

    pub(super) async fn bind(_path: &Path) -> Result<Listener, BindError> {
        Err(unsupported().into())
    }

    impl Listener {
        pub(super) async fn accept(&mut self) -> io::Result<Stream> {
            match *self {}
        }

        pub(super) fn close(self) {
            match self {}
        }
    }

    pub(super) async fn connect(_path: &Path) -> io::Result<Stream> {
        Err(unsupported())
    }

    pub(super) async fn remove(_path: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn probe(_path: &Path) -> Result<(), ProbeError> {
        Err(ProbeError::Endpoint(unsupported()))
    }

    impl Stream {
        pub(super) fn peer_is_current_user(&self) -> io::Result<bool> {
            match *self {}
        }

        pub(super) fn confirm_peer_after_read(&self) -> io::Result<bool> {
            match *self {}
        }
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match *self.get_mut() {}
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match *self.get_mut() {}
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match *self.get_mut() {}
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match *self.get_mut() {}
        }
    }
}
