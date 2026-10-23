//! The pipe: who may open it, and one framed request per connection.
//!
//! The access list grants the pipe's owner (SYSTEM, for the service) full
//! control and the configured user only [`CLIENT_RIGHTS`], which excludes
//! creating server instances. Windows checks it when a client opens the pipe:
//! other accounts are refused, AppContainers are not granted, the medium label
//! keeps out lower-integrity processes, and a restricted token passes only when
//! its restricting SIDs include the user. The service adds no check of its own.

use std::ffi::OsString;
use std::io;
use std::time::Duration;

use devlanes_hosts_helper::exit;
use devlanes_hosts_helper::protocol::{
    self, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, ProtocolError, Request, Response,
};
use devlanes_sys::windows::pipe::{CLIENT_RIGHTS, PipeFactory};
use devlanes_sys::windows::security::SecurityDescriptor;
use devlanes_sys::windows::token::{self, Sid};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::NamedPipeServer;

use crate::handler::{self, Target};

/// Longest a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest a client may take to read its response.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest the first instance waits for the pipe name to be released.
const FIRST_INSTANCE_WAIT: Duration = Duration::from_secs(5);

/// What the server answers and for whom.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// `\\.\pipe\devlanes-hosts` for the service; tests use their own.
    pub pipe_name: OsString,
    /// The only account that may open the pipe.
    pub user: Sid,
    /// The file requests edit.
    pub target: Target,
}

/// Failure to start serving.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The access list could not be built.
    #[error("building the pipe's access list: {0}")]
    Security(#[source] io::Error),
    /// The first instance could not be created; another process holds the name.
    #[error("creating {name}: {source}")]
    Create {
        /// The pipe name.
        name: String,
        /// The OS error; access denied when the name is taken.
        #[source]
        source: io::Error,
    },
}

/// The pipe's security descriptor: owned by this process's account, which gets
/// full control (it creates every instance), `user` gets [`CLIENT_RIGHTS`],
/// and the medium integrity label keeps lower-integrity processes out.
pub fn pipe_sddl(user: &Sid) -> io::Result<String> {
    let owner = token::current_user()?.to_sddl()?;
    let user = user.to_sddl()?;
    Ok(format!(
        "O:{owner}D:P(A;;GA;;;{owner})(A;;{CLIENT_RIGHTS:#x};;;{user})S:(ML;;NWNR;;;ME)"
    ))
}

/// Serves until `shutdown` completes, one connection at a time, since each
/// request rewrites the whole file. The first instance is created with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so this fails if anything else still holds
/// the name after about 5 s.
pub async fn serve(
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), ServeError> {
    serve_with(config, shutdown, || {}).await
}

/// [`serve`], calling `listening` once the first instance exists, so a caller
/// can report readiness only when clients can connect.
pub async fn serve_with(
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()>,
    listening: impl FnOnce(),
) -> Result<(), ServeError> {
    let descriptor = pipe_sddl(&config.user)
        .and_then(|sddl| SecurityDescriptor::from_sddl(&sddl))
        .map_err(ServeError::Security)?;
    let mut factory = PipeFactory::with_descriptor(&config.pipe_name, descriptor);
    tokio::pin!(shutdown);
    let deadline = tokio::time::Instant::now() + FIRST_INSTANCE_WAIT;
    let mut instance = loop {
        match factory.create(true) {
            Ok(instance) => break instance,
            // The name still exists: a client handle from before a restart
            // keeps it alive until that client closes it.
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
            Err(source) => {
                return Err(ServeError::Create {
                    name: config.pipe_name.to_string_lossy().into_owned(),
                    source,
                });
            }
        }
    };
    listening();
    loop {
        let connected = tokio::select! {
            () = &mut shutdown => return Ok(()),
            connected = instance.connect() => connected,
        };
        // The next instance must exist before this one is answered, or a client
        // arriving meanwhile finds no pipe and reports the service missing.
        let next = loop {
            match factory.create(false) {
                Ok(next) => break next,
                // Out of resources for the moment: wait, unless stopping.
                Err(_) => tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                },
            }
        };
        let pipe = std::mem::replace(&mut instance, next);
        // A client that left before the connect finished is simply dropped.
        if connected.is_ok() {
            tokio::select! {
                () = &mut shutdown => return Ok(()),
                () = answer(pipe, &config.target) => {}
            }
        }
    }
}

/// Reads one request, handles it and writes the response.
async fn answer(mut pipe: NamedPipeServer, target: &Target) {
    let payload =
        match tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut pipe, MAX_REQUEST_BYTES)).await
        {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(FrameError::Protocol(error))) => Err(error),
            // Too slow, or gone: there is nobody to answer.
            Ok(Err(FrameError::Io(_))) | Err(_) => return,
        };
    let response = match payload.and_then(|payload| Request::decode(&payload)) {
        Ok(request) => {
            let target = target.clone();
            tokio::task::spawn_blocking(move || handler::handle(&request, &target))
                .await
                .unwrap_or_else(|error| Response::failure(exit::IO, error.to_string()))
        }
        Err(error) => Response::failure(exit::USAGE, error.to_string()),
    };
    let framed = protocol::frame(&response.encode(), MAX_RESPONSE_BYTES).or_else(|error| {
        protocol::frame(
            &Response::failure(exit::IO, error.to_string()).encode(),
            MAX_RESPONSE_BYTES,
        )
    });
    if let Ok(framed) = framed {
        // The client may already be gone, or not read; nothing else to do then.
        let _ = tokio::time::timeout(RESPONSE_TIMEOUT, async {
            pipe.write_all(&framed).await?;
            pipe.flush().await
        })
        .await;
    }
}

/// Failure to read a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The connection failed or closed early.
    Io(io::Error),
    /// The frame announced more than the limit.
    Protocol(ProtocolError),
}

/// Reads one length-prefixed frame of at most `limit` bytes. Memory grows with
/// what actually arrives, not with what the prefix announces.
pub async fn read_frame(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    limit: usize,
) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0; 4];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(FrameError::Io)?;
    let length = protocol::frame_length(prefix, limit).map_err(FrameError::Protocol)?;
    let mut payload = Vec::new();
    reader
        .take(length as u64)
        .read_to_end(&mut payload)
        .await
        .map_err(FrameError::Io)?;
    if payload.len() < length {
        return Err(FrameError::Io(io::Error::from(
            io::ErrorKind::UnexpectedEof,
        )));
    }
    Ok(payload)
}
