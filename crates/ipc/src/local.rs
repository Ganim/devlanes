//! The daemon's local endpoint, reachable only by the user that runs it.
//!
//! On Unix it is a domain socket whose peers are checked by uid. On Windows it
//! is a named pipe whose security descriptor admits only the current user;
//! clients check that the pipe is owned by that user before trusting it.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Why [`LocalListener::bind`] failed.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// A live daemon answers on the endpoint.
    #[error("a daemon already listens on {}", .0.display())]
    AlreadyListening(PathBuf),
    /// Windows only: another process created the pipe name first. Callers hold
    /// the singleton lock, so it is not a daemon of this home.
    #[cfg(windows)]
    #[error("another process holds the daemon's pipe {}", .0.display())]
    Taken(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Why [`connect`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// A missing endpoint is [`io::ErrorKind::NotFound`] and a stale socket
    /// [`io::ErrorKind::ConnectionRefused`]: no daemon runs.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The endpoint belongs to another user.
    #[error("the daemon endpoint belongs to another user")]
    WrongPeer,
}

/// Why [`probe`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The socket path has no parent directory to probe in.
    #[error("socket path has no parent directory")]
    NoParent,
    /// Preparing the probe failed before any endpoint was created.
    #[error(transparent)]
    Setup(io::Error),
    /// Creating the throwaway endpoint failed, as sandboxes that forbid
    /// sockets or pipes make it.
    #[error("creating a local endpoint failed: {0}")]
    Endpoint(io::Error),
}

/// The daemon's listening endpoint.
pub struct LocalListener(imp::Listener);

impl LocalListener {
    /// Creates the endpoint at `path`; callers hold the singleton lock first,
    /// so two daemons of one home never race here.
    ///
    /// On Unix a socket that refuses connections is left over from a crashed
    /// daemon and is replaced; one that accepts is
    /// [`BindError::AlreadyListening`]. On Windows `path` is a pipe name
    /// (`\\.\pipe\...`) and an existing one is `BindError::Taken`.
    pub async fn bind(path: &Path) -> Result<Self, BindError> {
        imp::bind(path).await.map(Self)
    }

    /// Waits for the next client. Cancel safe, so it can sit in `select!`.
    pub async fn accept(&mut self) -> io::Result<LocalStream> {
        self.0.accept().await.map(LocalStream)
    }
}

/// A connected local stream, from either side.
pub struct LocalStream(imp::Stream);

impl LocalStream {
    /// For the daemon, on an accepted stream: whether the client runs as the
    /// daemon's real user. Always true on Windows, where the pipe's descriptor
    /// admits no other account.
    pub fn peer_is_current_user(&self) -> io::Result<bool> {
        imp::peer_is_current_user(&self.0)
    }
}

/// Connects to the endpoint at `path` and checks that it belongs to the
/// effective user. On Windows it waits while every pipe instance is busy, so
/// callers bound it with a timeout.
pub async fn connect(path: &Path) -> Result<LocalStream, ConnectError> {
    imp::connect(path).await.map(LocalStream)
}

/// Removes what a listener at `path` leaves behind once dropped: the socket
/// file on Unix, nothing on Windows, where the pipe goes with its last handle.
pub async fn remove(path: &Path) -> io::Result<()> {
    imp::remove(path).await
}

/// Creates and drops a throwaway endpoint (next to the socket on Unix), so a
/// sandbox that forbids them fails here instead of after a daemon was spawned
/// that could never listen. Blocking; on Windows it needs a tokio runtime
/// context, which `spawn_blocking` threads have.
pub fn probe(path: &Path) -> Result<(), ProbeError> {
    imp::probe(path)
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

    use tokio::net::{UnixListener, UnixStream};

    use super::{BindError, ConnectError, ProbeError};

    pub(super) type Stream = UnixStream;

    pub(super) struct Listener(UnixListener);

    /// Connects first: only a socket that refuses connections is stale.
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
        /// The next connection.
        pub(super) async fn accept(&mut self) -> io::Result<Stream> {
            let (stream, _) = self.0.accept().await?;
            Ok(stream)
        }
    }

    /// Compares the peer's uid with the effective uid.
    pub(super) async fn connect(path: &Path) -> Result<Stream, ConnectError> {
        let stream = UnixStream::connect(path).await?;
        if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() {
            return Err(ConnectError::WrongPeer);
        }
        Ok(stream)
    }

    /// Compares the peer's uid with the real uid.
    pub(super) fn peer_is_current_user(stream: &Stream) -> io::Result<bool> {
        Ok(stream.peer_cred()?.uid() == rustix::process::getuid().as_raw())
    }

    /// Removes the socket file.
    pub(super) async fn remove(path: &Path) -> io::Result<()> {
        tokio::fs::remove_file(path).await
    }

    /// A bind probe catches socket restrictions even when the launcher omitted
    /// sandbox markers.
    pub(super) fn probe(socket: &Path) -> Result<(), ProbeError> {
        let parent = socket.parent().ok_or(ProbeError::NoParent)?;
        let path = tempfile::Builder::new()
            .prefix("p")
            .rand_bytes(3)
            .tempfile_in(parent)
            .map_err(ProbeError::Setup)?
            .into_temp_path();
        std::fs::remove_file(&path).map_err(ProbeError::Setup)?;
        let listener =
            std::os::unix::net::UnixListener::bind(&path).map_err(ProbeError::Endpoint)?;
        drop(listener);
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use devlanes_sys_windows::pipe::{ERROR_NO_DATA, ERROR_PIPE_BUSY, PipeFactory, server_owner};
    use devlanes_sys_windows::token;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};

    use super::{BindError, ConnectError, ProbeError};

    /// `ERROR_ACCESS_DENIED`: a first instance of a name that exists already,
    /// or a client open the pipe's descriptor refuses.
    const ERROR_ACCESS_DENIED: i32 = 5;

    /// The wait after a replacement instance could not be created; each
    /// failure in a row doubles it, up to [`RETRY_MAX`].
    const RETRY_FIRST: Duration = Duration::from_millis(50);

    /// The longest wait between attempts to create a replacement instance.
    const RETRY_MAX: Duration = Duration::from_secs(1);

    /// One end of a pipe connection.
    pub(super) enum Stream {
        /// What [`connect`] opened.
        Client(NamedPipeClient),
        /// What the listener accepted.
        Server(NamedPipeServer),
    }

    /// The daemon's side of the pipe name.
    pub(super) struct Listener {
        pipes: PipeFactory,
        /// The instance waiting for the next client. One exists while the
        /// listener lives, so clients never find the name missing between
        /// accepts.
        next: NamedPipeServer,
        /// How long the next failed creation of a replacement instance waits;
        /// zero after a success.
        retry: Duration,
    }

    /// Creates the first instance of the name, so a pipe someone else created
    /// first is refused instead of shared.
    pub(super) async fn bind(path: &Path) -> Result<Listener, BindError> {
        let pipes = PipeFactory::current_user(path.as_os_str())?;
        match pipes.create(true) {
            Ok(next) => Ok(Listener {
                pipes,
                next,
                retry: Duration::ZERO,
            }),
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                Err(BindError::Taken(path.to_owned()))
            }
            Err(error) => Err(error.into()),
        }
    }

    impl Listener {
        /// Waits on the waiting instance and puts a fresh one in its place
        /// before handing the connected one over. A client that left before
        /// the accept (`ERROR_NO_DATA`) is skipped.
        ///
        /// When the fresh instance cannot be created (out of memory or
        /// handles), the error comes back after a wait that doubles with each
        /// failure in a row, up to a second, so a caller that logs and accepts
        /// again does not spin. The connected instance stays waiting and the
        /// next accept hands it over.
        pub(super) async fn accept(&mut self) -> io::Result<Stream> {
            loop {
                let connected = self.next.connect().await;
                // After a failed connect the instance is broken and would fail
                // every later accept the same way, so it is replaced either way.
                let fresh = match self.pipes.create(false) {
                    Ok(fresh) => fresh,
                    Err(error) => {
                        self.retry = (self.retry * 2).clamp(RETRY_FIRST, RETRY_MAX);
                        tokio::time::sleep(self.retry).await;
                        return Err(error);
                    }
                };
                self.retry = Duration::ZERO;
                let current = std::mem::replace(&mut self.next, fresh);
                match connected {
                    Ok(()) => return Ok(Stream::Server(current)),
                    Err(error) if error.raw_os_error() == Some(ERROR_NO_DATA) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    /// Opens the pipe, retrying while every instance is busy (the server is
    /// between handing one over and offering the next), then checks that its
    /// owner is this user. No server-side check exists: the pipe's descriptor
    /// admits only this user, and only this user (or an administrator) can
    /// own a pipe as them.
    ///
    /// The daemon always sets this user as the owner, elevated or not, so a
    /// pipe owned by anyone else, Administrators included, is not this
    /// user's daemon. A pipe that refuses to open stays a permission error,
    /// as on Unix: the daemon's medium-integrity label also refuses this
    /// user's sandboxed (low-integrity) processes, which is the common case.
    pub(super) async fn connect(path: &Path) -> Result<Stream, ConnectError> {
        let client = loop {
            match ClientOptions::new().open(path.as_os_str()) {
                Ok(client) => break client,
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        if server_owner(&client)? != token::current_user()? {
            return Err(ConnectError::WrongPeer);
        }
        Ok(Stream::Client(client))
    }

    /// The descriptor already limited the client to this user.
    pub(super) fn peer_is_current_user(_stream: &Stream) -> io::Result<bool> {
        Ok(true)
    }

    /// Nothing outlives the pipe's handles.
    pub(super) async fn remove(_path: &Path) -> io::Result<()> {
        Ok(())
    }

    /// Creates and drops a throwaway pipe with the daemon's descriptor.
    pub(super) fn probe(_socket: &Path) -> Result<(), ProbeError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or_default();
        let name = format!(r"\\.\pipe\devlanes-probe-{}-{nanos}", std::process::id());
        let pipes = PipeFactory::current_user(&name).map_err(ProbeError::Setup)?;
        drop(pipes.create(true).map_err(ProbeError::Endpoint)?);
        Ok(())
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(client) => Pin::new(client).poll_read(cx, buf),
                Self::Server(server) => Pin::new(server).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Self::Client(client) => Pin::new(client).poll_write(cx, buf),
                Self::Server(server) => Pin::new(server).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(client) => Pin::new(client).poll_flush(cx),
                Self::Server(server) => Pin::new(server).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(client) => Pin::new(client).poll_shutdown(cx),
                Self::Server(server) => Pin::new(server).poll_shutdown(cx),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A fresh endpoint: a socket in `dir`, or a pipe named after it.
    fn endpoint(dir: &Path) -> PathBuf {
        #[cfg(unix)]
        let path = dir.join("e.sock");
        #[cfg(windows)]
        let path = PathBuf::from(format!(
            r"\\.\pipe\devlanes-ipc-test-{}-{}",
            std::process::id(),
            dir.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        ));
        path
    }

    /// Fifty clients connecting at once all get through, each to its own
    /// stream, and a second listener on a live endpoint is refused.
    #[tokio::test]
    async fn many_clients_connect_and_the_endpoint_is_exclusive() -> io::Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let path = endpoint(dir.path());
            let mut listener = LocalListener::bind(&path).await.map_err(io::Error::other)?;
            let server = tokio::spawn(async move {
                for _ in 0..50 {
                    let mut stream = listener.accept().await?;
                    assert!(stream.peer_is_current_user()?);
                    tokio::spawn(async move {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).await?;
                        stream.write_all(&byte).await
                    });
                }
                Ok::<_, io::Error>(listener)
            });
            let clients: Vec<_> = (0..50u8)
                .map(|n| {
                    let path = path.clone();
                    tokio::spawn(async move {
                        let mut stream = connect(&path).await.map_err(io::Error::other)?;
                        stream.write_all(&[n]).await?;
                        let mut echo = [0];
                        stream.read_exact(&mut echo).await?;
                        assert_eq!(echo, [n]);
                        Ok::<_, io::Error>(())
                    })
                })
                .collect();
            for client in clients {
                client.await.map_err(io::Error::other)??;
            }
            let listener = server.await.map_err(io::Error::other)??;
            // Only once no accept is pending: on Unix the bind's own probe
            // connection would otherwise count as one of the fifty.
            let second = LocalListener::bind(&path).await;
            #[cfg(unix)]
            assert!(matches!(second, Err(BindError::AlreadyListening(_))));
            #[cfg(windows)]
            assert!(matches!(second, Err(BindError::Taken(_))));
            drop(listener);
            remove(&path).await
        })
        .await
        .map_err(io::Error::other)?
    }

    /// A missing endpoint reads as "no daemon".
    #[tokio::test]
    async fn a_missing_endpoint_is_not_found() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let Err(ConnectError::Io(error)) = connect(&endpoint(dir.path())).await else {
            panic!("connected to nothing");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    /// The probe leaves nothing behind.
    #[tokio::test]
    async fn probe_creates_and_drops_an_endpoint() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("d.sock");
        tokio::task::spawn_blocking(move || probe(&path))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::time::Duration;

    use devlanes_sys_windows::pipe::PipeFactory;
    use devlanes_sys_windows::security::SecurityDescriptor;

    use super::*;

    /// A pipe someone else created first is refused at bind, and the owner
    /// check passes for a pipe of this user.
    #[tokio::test]
    async fn a_squatted_name_is_taken() -> io::Result<()> {
        let name = PathBuf::from(format!(
            r"\\.\pipe\devlanes-ipc-test-{}-squat",
            std::process::id()
        ));
        let squatter = PipeFactory::current_user(&name)?.create(true)?;
        assert!(matches!(
            LocalListener::bind(&name).await,
            Err(BindError::Taken(_))
        ));
        let client = tokio::time::timeout(Duration::from_secs(10), connect(&name))
            .await
            .map_err(io::Error::other)?;
        assert!(client.is_ok(), "an own pipe passes the owner check");
        drop(squatter);
        Ok(())
    }

    /// A pipe whose descriptor refuses this user is a permission error, the
    /// same answer a sandboxed process of this user gets, never a missing daemon.
    #[tokio::test]
    async fn a_refused_pipe_is_a_permission_error() -> io::Result<()> {
        let name = PathBuf::from(format!(
            r"\\.\pipe\devlanes-ipc-test-{}-refused",
            std::process::id()
        ));
        let system_only = SecurityDescriptor::from_sddl("D:P(A;;GA;;;SY)")?;
        let server = PipeFactory::new(&name, system_only).create(true)?;
        let refused = tokio::time::timeout(Duration::from_secs(10), connect(&name))
            .await
            .map_err(io::Error::other)?;
        let kind = match &refused {
            Err(ConnectError::Io(error)) => Some(error.kind()),
            _ => None,
        };
        assert_eq!(kind, Some(io::ErrorKind::PermissionDenied));
        drop(server);
        Ok(())
    }
}
