//! The pipe: who may open it, who is answered, and one framed request per
//! connection.
//!
//! The access list grants the pipe's owner (SYSTEM, for the service) full
//! control and the configured user only [`CLIENT_RIGHTS`], which excludes
//! creating server instances. Each caller is identified after its request is
//! read, by impersonating it just long enough to read its token: the same user,
//! not sandboxed (AppContainer, restricted), at medium integrity or above.

use std::ffi::OsString;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use devlanes_hosts_helper::exit;
use devlanes_hosts_helper::protocol::{
    self, DENIED, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, ProtocolError, Request, Response,
};
use devlanes_sys::windows::pipe::{CLIENT_RIGHTS, PipeFactory, client_token_facts};
use devlanes_sys::windows::security::SecurityDescriptor;
use devlanes_sys::windows::token::{self, MEDIUM_INTEGRITY, Sid};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::NamedPipeServer;

use crate::handler::{self, Target};

/// Longest a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest a client may take to read its response.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections handled at once. Requests are written one at a time anyway; the
/// cap bounds the memory and tasks a flood of connections can hold.
const MAX_CONNECTIONS: usize = 4;

/// How long the first instance is retried when the name is taken. A process that
/// held the name while the service was stopped may be on its way out (the
/// service manager restarts the service after a failure too).
const FIRST_INSTANCE_WAIT: Duration = Duration::from_secs(5);

/// What the server answers and for whom.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// `\\.\pipe\devlanes-hosts` for the service; tests use their own.
    pub pipe_name: OsString,
    /// The only account answered.
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

/// Serves until `shutdown` completes. Requests are handled one at a time, since
/// each rewrites the whole file, and at most [`MAX_CONNECTIONS`] connections are
/// open at once. The first instance is created with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so this fails if anything else holds the
/// name for longer than [`FIRST_INSTANCE_WAIT`].
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
    let name = config.pipe_name.to_string_lossy().into_owned();
    let mut factory = PipeFactory::with_descriptor(&config.pipe_name, descriptor);
    tokio::pin!(shutdown);
    let deadline = tokio::time::Instant::now() + FIRST_INSTANCE_WAIT;
    let mut instance = loop {
        match factory.create(true) {
            Ok(instance) => break instance,
            Err(source) if tokio::time::Instant::now() >= deadline => {
                return Err(ServeError::Create { name, source });
            }
            Err(_) => tokio::select! {
                () = &mut shutdown => return Ok(()),
                () = tokio::time::sleep(Duration::from_millis(250)) => {}
            },
        }
    };
    listening();
    let config = Arc::new(config);
    let one_at_a_time = Arc::new(Mutex::new(()));
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        // Waiting for a free slot before accepting: while all are busy, at most
        // one more client gets the listening instance and waits.
        let permit = tokio::select! {
            () = &mut shutdown => return Ok(()),
            permit = connections.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                // Never closed; nothing to serve with if it were.
                Err(_) => return Ok(()),
            },
        };
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            connected = instance.connect() => {
                // The next instance must exist before this one is handed off, or a
                // client arriving in between finds no pipe.
                let next = loop {
                    match factory.create(false) {
                        Ok(next) => break Some(next),
                        // Out of resources for the moment: wait, unless stopping.
                        Err(_) => tokio::select! {
                            () = &mut shutdown => break None,
                            () = tokio::time::sleep(Duration::from_millis(100)) => {}
                        },
                    }
                };
                let Some(next) = next else {
                    return Ok(());
                };
                let pipe = std::mem::replace(&mut instance, next);
                // A client that left before the connect finished is simply dropped.
                if connected.is_ok() {
                    let (config, lock) = (config.clone(), one_at_a_time.clone());
                    tokio::spawn(async move {
                        answer(pipe, config, lock).await;
                        drop(permit);
                    });
                }
            }
        }
    }
}

/// Whether the client on `pipe` is `user`, not sandboxed, at medium integrity
/// or above. Call after reading from the pipe.
fn caller_allowed(pipe: &NamedPipeServer, user: &Sid) -> io::Result<bool> {
    let facts = client_token_facts(pipe)?;
    Ok(facts.user == *user
        && !facts.app_container
        && !facts.restricted
        && facts.integrity >= MEDIUM_INTEGRITY)
}

/// Reads one request, checks the caller, handles it and writes the response.
async fn answer(mut pipe: NamedPipeServer, config: Arc<ServerConfig>, lock: Arc<Mutex<()>>) {
    let payload =
        match tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut pipe, MAX_REQUEST_BYTES)).await
        {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(FrameError::Protocol(error))) => Err(error),
            // Too slow, or gone: there is nobody to answer.
            Ok(Err(FrameError::Io(_))) | Err(_) => return,
        };
    // Windows lets a server impersonate its client only after reading from the
    // pipe, so the check comes after the read even for a bad request.
    let response = match caller_allowed(&pipe, &config.user) {
        Ok(true) => match payload.and_then(|payload| Request::decode(&payload)) {
            Ok(request) => {
                let config = config.clone();
                tokio::task::spawn_blocking(move || {
                    // A panic while holding the lock left no partial state behind:
                    // every write goes through a temp file.
                    let _guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
                    handler::handle(&request, &config.target)
                })
                .await
                .unwrap_or_else(|error| Response::failure(exit::IO, error.to_string()))
            }
            Err(error) => Response::failure(exit::USAGE, error.to_string()),
        },
        Ok(false) => Response::failure(
            DENIED,
            "this account may not use the devlanes hosts service; it serves the user who installed it",
        ),
        Err(error) => Response::failure(DENIED, format!("cannot identify the caller: {error}")),
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
