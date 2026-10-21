//! Named-pipe endpoint. Every pipe instance carries a DACL for the current user
//! only; the daemon side also confirms its client by impersonation, and the client
//! confirms the serving process's user, so a pipe name squatted by another account
//! is refused.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use devlanes_sys::windows::pipe;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};

use super::{BindError, ProbeError};

/// `ERROR_ACCESS_DENIED`: the name exists and this caller may not create it.
const ACCESS_DENIED: i32 = 5;
/// `ERROR_PIPE_BUSY`: every instance is connected; another is about to be created.
const PIPE_BUSY: i32 = 231;

pub(super) enum Stream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

pub(super) struct Listener {
    name: OsString,
    /// The instance waiting for the next client. One always exists while the
    /// listener lives, so clients never find the name missing between accepts.
    next: NamedPipeServer,
}

pub(super) async fn bind(path: &Path) -> Result<Listener, BindError> {
    let name = path.as_os_str().to_owned();
    match pipe::create_server(&name, true) {
        Ok(next) => Ok(Listener { name, next }),
        // The caller holds the singleton lock, so whoever owns this name is either
        // a daemon that predates the lock file or another account squatting on it.
        Err(error) if matches!(error.raw_os_error(), Some(ACCESS_DENIED | PIPE_BUSY)) => {
            Err(BindError::AlreadyListening(path.to_owned()))
        }
        Err(error) => Err(error.into()),
    }
}

impl Listener {
    pub(super) async fn accept(&mut self) -> io::Result<Stream> {
        self.next.connect().await?;
        let fresh = pipe::create_server(&self.name, false)?;
        Ok(Stream::Server(std::mem::replace(&mut self.next, fresh)))
    }

    pub(super) fn close(self) {
        drop(self.next);
    }
}

pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
    loop {
        match ClientOptions::new().open(path.as_os_str()) {
            Ok(client) => return Ok(Stream::Client(client)),
            // The server is between accepting one client and offering the next
            // instance; the caller's connect timeout bounds this loop.
            Err(error) if error.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// A pipe has no directory to protect.
pub(super) fn endpoint_dir(_path: &Path) -> Option<&Path> {
    None
}

/// A pipe disappears with its last handle; nothing is left to remove.
pub(super) async fn remove(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(super) fn probe(_path: &Path) -> Result<(), ProbeError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    let name = format!(r"\\.\pipe\devlanes-probe-{}-{nanos}", std::process::id());
    pipe::create_server(OsStr::new(&name), true)
        .map(drop)
        .map_err(ProbeError::Endpoint)
}

impl Stream {
    pub(super) fn peer_is_current_user(&self) -> io::Result<bool> {
        match self {
            Self::Client(client) => pipe::server_is_current_user(client),
            Self::Server(_) => Ok(true),
        }
    }

    pub(super) fn confirm_peer_after_read(&self) -> io::Result<bool> {
        match self {
            Self::Client(_) => Ok(true),
            Self::Server(server) => pipe::client_is_current_user(server),
        }
    }
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
