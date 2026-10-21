//! Local API runtime. Managed resources are supplied by the composition root.
//!
//! [`Server`] owns the socket and the singleton lock; [`Engine`] is the
//! [`Handler`] that runs lanes. Concrete integrations arrive through
//! [`integrations::Integrations`].

mod attach;
pub mod bus;
mod control;
mod dns;
mod engine;
pub mod hosts;
pub mod integrations;
mod lifecycle;
mod maintenance;
mod model;
mod pacing;
mod platforms;
pub mod process;
mod query;
pub mod ready;
mod reconcile;
mod registry;
mod remote;
mod resolve;
mod routes;
mod sampler;
pub mod secrets;
pub mod units;

pub use engine::{Engine, EngineOptions};

use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_ipc::{Connection, Frame};
use devlanes_proto::{Control, Event, Hello, Method, Outcome, Request, Response, RpcError};
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

/// Startup failures leave the current daemon and its processes untouched.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("daemon I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("a daemon already owns the lock")]
    AlreadyRunning,
    #[error("unsafe daemon directory: {0}")]
    UnsafeDirectory(PathBuf),
    #[error("startup worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("shutdown failed: {0}")]
    Shutdown(String),
}

/// Paths are supplied explicitly so tests never touch the user's daemon.
#[derive(Debug, Clone)]
pub struct Options {
    pub home: PathBuf,
    pub socket: PathBuf,
}

/// A client may disconnect without cancelling the operation it requested.
#[derive(Clone)]
pub struct Session {
    sender: mpsc::Sender<Outgoing>,
    disconnected: CancellationToken,
    id: u64,
}

/// Delivery acknowledgement ensures shutdown cannot overtake its response.
struct Outgoing {
    frame: Frame,
    delivered: oneshot::Sender<()>,
}

impl Session {
    /// Attach authorization is scoped to the connection, not merely the user's UID.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Event producers can await backpressure without blocking unrelated clients.
    pub async fn event(&self, event: Event) -> Result<(), RpcError> {
        self.send(Frame::Control(Control::Event(event))).await
    }

    /// Terminal streams bypass JSON so arbitrary control sequences survive unchanged.
    pub async fn bytes(&self, stream_id: u64, data: Bytes) -> Result<(), RpcError> {
        self.send(Frame::Bytes { stream_id, data }).await
    }

    /// Lets follow/attach subscriptions release resources when their viewer leaves.
    pub fn disconnected(&self) -> CancellationToken {
        self.disconnected.clone()
    }

    /// An in-process session for embedding and tests: frames sent to it arrive on
    /// the returned receiver. Dropping the receiver makes sends fail as a disconnect.
    pub fn local(id: u64) -> (Self, mpsc::Receiver<Frame>) {
        let (sender, mut outgoing) = mpsc::channel::<Outgoing>(128);
        let (frames, receiver) = mpsc::channel(1024);
        let disconnected = CancellationToken::new();
        let token = disconnected.clone();
        tokio::spawn(async move {
            while let Some(Outgoing { frame, delivered }) = outgoing.recv().await {
                if frames.send(frame).await.is_err() {
                    token.cancel();
                    break;
                }
                let _ = delivered.send(());
            }
        });
        (
            Self {
                sender,
                disconnected,
                id,
            },
            receiver,
        )
    }

    /// Marks the session disconnected, ending its follows, watches and attaches.
    pub fn close(&self) {
        self.disconnected.cancel();
    }

    /// Failed delivery never changes the lifecycle of a managed unit.
    async fn send(&self, frame: Frame) -> Result<(), RpcError> {
        let (delivered, received) = oneshot::channel();
        self.sender
            .send(Outgoing { frame, delivered })
            .await
            .map_err(|_| RpcError::new("client_disconnected", Value::Null, 4))?;
        received
            .await
            .map_err(|_| RpcError::new("client_disconnected", Value::Null, 4))
    }
}

/// Implemented by the resource supervisor, with adapters injected by the binary.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// The count supports version and shutdown decisions without enumerating secrets.
    fn managed_processes(&self) -> usize;

    /// Completes reconciliation before the daemon announces readiness.
    async fn reconcile(&self) -> Result<(), RpcError>;

    /// Returns structured data; neither mode selection nor wording belongs here.
    async fn handle(&self, request: Request, session: Session) -> Result<Value, RpcError>;

    /// Routes bytes only to an attach stream previously authorized for this client.
    async fn input(&self, stream_id: u64, data: Bytes, session: Session) -> Result<(), RpcError>;

    /// Explicit daemon shutdown stops owned processes but preserves adopted containers.
    async fn shutdown(&self) -> Result<(), RpcError>;
}

/// Owns the singleton lock until all shutdown work and socket cleanup complete.
pub struct Server {
    listener: UnixListener,
    _lock: File,
    options: Options,
    started: Instant,
    shutdown: CancellationToken,
}

impl Server {
    /// Acquires the lock before inspecting a stale socket, preventing spawn races.
    pub async fn bind(options: Options) -> Result<Self, Error> {
        let prepared = options.clone();
        let lock = tokio::task::spawn_blocking(move || prepare(&prepared)).await??;
        match UnixStream::connect(&options.socket).await {
            Ok(_) => return Err(Error::AlreadyRunning),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                tokio::fs::remove_file(&options.socket).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&options.socket)?;
        Ok(Self {
            listener,
            _lock: lock,
            options,
            started: Instant::now(),
            shutdown: CancellationToken::new(),
        })
    }

    /// A service manager can request the same explicit shutdown path as the CLI.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Reconciliation precedes the callback so a connecting client sees coherent state.
    pub async fn run(
        self,
        handler: Arc<dyn Handler>,
        ready: impl FnOnce() -> std::io::Result<()>,
    ) -> Result<(), Error> {
        handler
            .reconcile()
            .await
            .map_err(|error| Error::Shutdown(error.code))?;
        ready()?;
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => break,
                accepted = self.listener.accept() => {
                    // One client's failure must never end the daemon: a peer that closes
                    // before its credentials are read (macOS: ENOTCONN) used to exit here.
                    let stream = match accepted {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            tracing::warn!(%error, "accept failed");
                            // Out of descriptors: give closing units a moment instead of spinning.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    match stream.peer_cred() {
                        Ok(cred) if cred.uid() == rustix::process::getuid().as_raw() => {}
                        Ok(_) => continue,
                        Err(error) => {
                            tracing::debug!(%error, "client left before its credentials were read");
                            continue;
                        }
                    }
                    let handler = handler.clone();
                    let shutdown = self.shutdown.clone();
                    let started = self.started;
                    let socket = self.options.socket.clone();
                    clients.spawn(async move {
                        if let Err(error) = serve_client(stream, handler, shutdown, started, socket).await {
                            tracing::debug!(%error, "client connection ended");
                        }
                    });
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = result { tracing::error!(%error, "client task failed"); }
                }
            }
        }
        // Stop resources before closing subscribers so they can observe final state.
        let stopped = handler.shutdown().await;
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        let remove = tokio::fs::remove_file(&self.options.socket).await;
        stopped.map_err(|error| Error::Shutdown(error.code))?;
        remove?;
        Ok(())
    }
}

/// Uses no-follow opens inside owner-only directories to protect the singleton lock.
fn prepare(options: &Options) -> Result<File, Error> {
    secure_directory(&options.home)?;
    let run = options.home.join("run");
    secure_directory(&run)?;
    let parent = options
        .socket
        .parent()
        .ok_or_else(|| Error::UnsafeDirectory(options.socket.clone()))?;
    secure_directory(parent)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(run.join("daemon.lock"))?;
    match FileExt::try_lock_exclusive(&lock) {
        Ok(()) => Ok(lock),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(Error::AlreadyRunning),
        Err(error) => Err(error.into()),
    }
}

/// Existing directories must already be private rather than being silently repurposed.
fn secure_directory(path: &Path) -> Result<(), Error> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(Error::UnsafeDirectory(path.into()));
    }
    Ok(())
}

/// One bounded outgoing queue prevents slow terminals from consuming unbounded memory.
async fn serve_client(
    stream: UnixStream,
    handler: Arc<dyn Handler>,
    shutdown: CancellationToken,
    started: Instant,
    socket: PathBuf,
) -> Result<(), devlanes_ipc::Error> {
    let mut connection = Connection::new(stream);
    let first = tokio::time::timeout(Duration::from_secs(5), connection.recv())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "hello timeout"))??;
    let Frame::Control(Control::Hello(peer)) = first else {
        return Err(devlanes_ipc::Error::InvalidFrame);
    };
    let hello = Hello::current(handler.managed_processes());
    connection
        .send(Frame::Control(Control::Hello(hello.clone())))
        .await?;
    if peer.protocol != hello.protocol {
        return Ok(());
    }

    let (sender, mut outgoing) = mpsc::channel(128);
    let disconnected = CancellationToken::new();
    let _disconnect_guard = disconnected.clone().drop_guard();
    static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let session = Session {
        sender,
        disconnected: disconnected.clone(),
        id: NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    };
    let result = async {
        loop {
            tokio::select! {
                frame = outgoing.recv() => {
                    if let Some(Outgoing { frame, delivered }) = frame {
                        connection.send(frame).await?;
                        let _ = delivered.send(());
                    }
                }
                received = connection.recv() => {
                    match received? {
                        Frame::Control(Control::Request(request)) => {
                            let handler = handler.clone();
                            let session = session.clone();
                            let shutdown = shutdown.clone();
                            let socket = socket.clone();
                            // Requests outlive connections: detaching must not cancel a lane start.
                            tokio::spawn(async move {
                                let id = request.id;
                                let stop = request.method == Method::DaemonShutdown;
                                let result = match request.method {
                                    Method::DaemonStatus => Ok(json!({
                                        "version": env!("CARGO_PKG_VERSION"),
                                        "pid": std::process::id(),
                                        "uptime_ms": started.elapsed().as_millis(),
                                        "managed_processes": handler.managed_processes(),
                                        "socket": socket,
                                    })),
                                    Method::DaemonShutdown if handler.managed_processes() > 0 && !request.context.yes => {
                                        Err(RpcError::new("confirmation_required", json!({"processes": handler.managed_processes()}), 2))
                                    }
                                    Method::DaemonShutdown => Ok(json!({"stopping": true, "processes": handler.managed_processes()})),
                                    _ => handler.handle(request, session.clone()).await,
                                };
                                let accepted = result.is_ok();
                                let outcome = match result {
                                    Ok(data) => Outcome::Ok { data },
                                    Err(error) => Outcome::Error { error },
                                };
                                let _ = session.send(Frame::Control(Control::Response(Response { id, result: outcome }))).await;
                                if stop && accepted { shutdown.cancel(); }
                            });
                        }
                        Frame::Bytes { stream_id, data } => {
                            handler.input(stream_id, data, session.clone()).await
                                .map_err(|_| devlanes_ipc::Error::InvalidFrame)?;
                        }
                        _ => return Err(devlanes_ipc::Error::InvalidFrame),
                    }
                }
            }
        }
    }.await;
    disconnected.cancel();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lock_prevents_a_second_bind() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let options = Options {
            socket: home.join("run/daemon.sock"),
            home,
        };
        let _first = Server::bind(options.clone()).await?;
        assert!(matches!(
            Server::bind(options).await,
            Err(Error::AlreadyRunning)
        ));
        Ok(())
    }

    #[test]
    fn refuses_a_symlinked_home() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let link = dir.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &link)?;
        assert!(matches!(
            secure_directory(&link),
            Err(Error::UnsafeDirectory(_))
        ));
        Ok(())
    }
}
