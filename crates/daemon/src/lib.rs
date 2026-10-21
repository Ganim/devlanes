//! Local API runtime. Managed resources are supplied by the composition root.
//!
//! [`Server`] owns the socket and the singleton lock; [`Engine`] is the
//! [`Handler`] that runs lanes. Concrete integrations arrive through
//! [`integrations::Integrations`].

mod attach;
pub mod bus;
mod certs;
mod control;
mod dns;
mod engine;
pub mod hosts;
mod input;
pub mod integrations;
mod lifecycle;
mod maintenance;
mod model;
mod orphans;
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
mod stream;
pub mod units;
pub mod update;

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
    /// Startup reconciliation failed; the message reads `CODE: message`, like
    /// other startup reasons, so the spawning client keeps the code.
    #[error("{0}")]
    Reconcile(String),
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
    /// A failed reconciliation still runs the handler's shutdown (records exits,
    /// closes the store) and returns [`Error::Reconcile`] for the spawning client.
    pub async fn run(
        self,
        handler: Arc<dyn Handler>,
        ready: impl FnOnce() -> std::io::Result<()>,
    ) -> Result<(), Error> {
        if let Err(error) = handler.reconcile().await {
            if let Err(stopped) = handler.shutdown().await {
                tracing::warn!(code = %stopped.code, "shutdown after a failed reconcile");
            }
            let _ = tokio::fs::remove_file(&self.options.socket).await;
            return Err(Error::Reconcile(startup_reason(&error)));
        }
        ready()?;
        let Self {
            listener,
            options,
            started,
            shutdown,
            _lock,
        } = self;
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => {
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
                    let shutdown = shutdown.clone();
                    let socket = options.socket.clone();
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
        // No new connection from here on; connected clients get `daemon_stopping`
        // for anything that would start work.
        drop(listener);
        // Stop resources before closing subscribers so they can observe final state.
        let stopped = handler.shutdown().await;
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        let remove = tokio::fs::remove_file(&options.socket).await;
        stopped.map_err(|error| Error::Shutdown(error.code))?;
        remove?;
        Ok(())
    }
}

/// The error reply that replaces response `id` when it does not fit in a frame.
fn too_large(id: u64) -> Frame {
    let error = RpcError::new(
        "response_too_large",
        json!({
            "limit_bytes": devlanes_proto::MAX_FRAME_SIZE,
            "message": devlanes_ipc::Error::TooLarge.to_string(),
        }),
        1,
    );
    Frame::Control(Control::Response(Response {
        id,
        result: Outcome::Error { error },
    }))
}

/// `CODE: message` from a handler error, the shape startup failures report in.
fn startup_reason(error: &RpcError) -> String {
    let message = match error.data.get("message").and_then(Value::as_str) {
        Some(message) => message.to_owned(),
        None => error.data.to_string(),
    };
    format!("{}: {message}", error.code.to_uppercase())
}

/// Uses no-follow opens inside owner-only directories to protect the singleton lock.
fn prepare(options: &Options) -> Result<File, Error> {
    let parent = options
        .socket
        .parent()
        .ok_or_else(|| Error::UnsafeDirectory(options.socket.clone()))?;
    let lock = lock_singleton(&options.home)?;
    secure_directory(parent)?;
    Ok(lock)
}

/// Takes the daemon's singleton lock under `home` (blocking I/O). Offline
/// maintenance such as the CLI's local `daemon reset-state` holds it, so no daemon
/// starts meanwhile; [`Error::AlreadyRunning`] means one owns it. Dropping the
/// file releases the lock.
pub fn lock_singleton(home: &Path) -> Result<File, Error> {
    secure_directory(home)?;
    let run = home.join("run");
    secure_directory(&run)?;
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
    let mut hello = Hello::current(handler.managed_processes());
    hello
        .features
        .push(devlanes_proto::features::START_EVENTS.into());
    connection
        .send(Frame::Control(Control::Hello(hello.clone())))
        .await?;
    // A client of another protocol may still ask the frozen `daemon.status` and
    // `daemon.shutdown`, so an upgrade can always find and stop the old daemon.
    let compatible = peer.protocol == hello.protocol;
    let mismatch = RpcError::new(
        "protocol_mismatch",
        json!({
            "protocol": hello.protocol,
            "client_protocol": peer.protocol,
            "version": hello.version,
            "build_id": hello.build_id,
            "managed_processes": hello.managed_processes,
        }),
        // Plan 3.5: incompatible daemons exit 4, like unreachable ones.
        4,
    );

    let (sender, mut outgoing) = mpsc::channel(128);
    let disconnected = CancellationToken::new();
    let _disconnect_guard = disconnected.clone().drop_guard();
    static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let session = Session {
        sender,
        disconnected: disconnected.clone(),
        id: NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    };
    let mut inputs = input::Inputs::new(handler.clone(), session.clone());
    let result = async {
        loop {
            tokio::select! {
                frame = outgoing.recv() => {
                    if let Some(Outgoing { frame, delivered }) = frame {
                        // An empty frame ends an attach stream; its input queue goes with it.
                        let ended = match &frame {
                            Frame::Bytes { stream_id, data } if data.is_empty() => Some(*stream_id),
                            _ => None,
                        };
                        let response = match &frame {
                            Frame::Control(Control::Response(response)) => Some(response.id),
                            _ => None,
                        };
                        match (connection.send(frame).await, response) {
                            (Ok(()), _) => {}
                            // Nothing was written yet: the client still gets an answer.
                            (Err(devlanes_ipc::Error::TooLarge), Some(id)) => {
                                connection.send(too_large(id)).await?;
                            }
                            (Err(error), _) => return Err(error),
                        }
                        let _ = delivered.send(());
                        if let Some(stream_id) = ended {
                            inputs.end(stream_id);
                        }
                    }
                }
                received = connection.recv() => {
                    match received? {
                        Frame::Control(Control::Request(request)) => {
                            let handler = handler.clone();
                            let session = session.clone();
                            let shutdown = shutdown.clone();
                            let socket = socket.clone();
                            let mismatch = mismatch.clone();
                            // Requests outlive connections: detaching must not cancel a lane start.
                            tokio::spawn(async move {
                                let id = request.id;
                                let stop = request.method == Method::DaemonShutdown;
                                let result = match request.method {
                                    Method::DaemonStatus => {
                                        let mut status = json!({
                                            "version": env!("CARGO_PKG_VERSION"),
                                            "pid": std::process::id(),
                                            "uptime_ms": started.elapsed().as_millis(),
                                            "managed_processes": handler.managed_processes(),
                                            "socket": socket,
                                        });
                                        // Release checks and update notices come from the
                                        // engine; another protocol gets the frozen fields only.
                                        if compatible {
                                            match handler.handle(request, session.clone()).await {
                                                Ok(Value::Object(extra)) => {
                                                    if let Some(fields) = status.as_object_mut() {
                                                        fields.extend(extra);
                                                    }
                                                }
                                                Ok(_) => {}
                                                Err(error) => tracing::debug!(code = %error.code, "daemon.status without engine fields"),
                                            }
                                        }
                                        Ok(status)
                                    }
                                    Method::DaemonShutdown if handler.managed_processes() > 0 && !request.context.yes => {
                                        Err(RpcError::new("confirmation_required", json!({"processes": handler.managed_processes()}), 2))
                                    }
                                    Method::DaemonShutdown => Ok(json!({"stopping": true, "processes": handler.managed_processes()})),
                                    _ if !compatible => Err(mismatch),
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
                        // Queued per stream: a unit that never reads must not stall this loop.
                        // A mismatched client cannot have opened a stream.
                        Frame::Bytes { stream_id, data } if compatible => inputs.push(stream_id, data),
                        Frame::Bytes { .. } => {}
                        // A newer client's frame type: skipped, the connection stays.
                        Frame::Control(Control::Unknown) => {}
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

    /// Answers every request with `reply`; reconciliation fails when `broken`.
    struct Fixed {
        reply: Value,
        broken: bool,
        shut_down: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Handler for Fixed {
        fn managed_processes(&self) -> usize {
            0
        }
        async fn reconcile(&self) -> Result<(), RpcError> {
            if self.broken {
                return Err(RpcError::new(
                    "state_error",
                    json!({"message": "state.db is unreadable"}),
                    1,
                ));
            }
            Ok(())
        }
        async fn handle(&self, _: Request, _: Session) -> Result<Value, RpcError> {
            Ok(self.reply.clone())
        }
        async fn input(&self, _: u64, _: Bytes, _: Session) -> Result<(), RpcError> {
            Ok(())
        }
        async fn shutdown(&self) -> Result<(), RpcError> {
            self.shut_down
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Options under a short temporary directory (socket paths are bounded).
    fn short_options() -> (tempfile::TempDir, Options) {
        let dir = tempfile::Builder::new()
            .prefix("dls-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let home = dir.path().join("h");
        let options = Options {
            socket: home.join("run/d.sock"),
            home,
        };
        (dir, options)
    }

    /// Sends request `id` and waits for its outcome.
    async fn ask(connection: &mut Connection<UnixStream>, id: u64, method: Method) -> Outcome {
        connection
            .send(Frame::Control(Control::Request(Request {
                id,
                method,
                params: Value::Null,
                context: devlanes_proto::Context::default(),
            })))
            .await
            .expect("send");
        loop {
            if let Frame::Control(Control::Response(response)) =
                connection.recv().await.expect("recv")
                && response.id == id
            {
                return response.result;
            }
        }
    }

    #[tokio::test]
    /// A response larger than a frame becomes an error reply on a connection
    /// that keeps working.
    async fn an_oversized_response_becomes_an_error() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (_dir, options) = short_options();
            let socket = options.socket.clone();
            let server = Server::bind(options).await.expect("bind");
            let token = server.shutdown_token();
            let handler = Arc::new(Fixed {
                reply: json!("x".repeat(devlanes_proto::MAX_FRAME_SIZE + 1)),
                broken: false,
                shut_down: Default::default(),
            });
            let running = tokio::spawn(server.run(handler, || Ok(())));
            let mut connection =
                Connection::new(UnixStream::connect(&socket).await.expect("connect"));
            connection
                .send(Frame::Control(Control::Hello(Hello::current(0))))
                .await
                .expect("hello");
            connection.recv().await.expect("hello reply");
            for id in [1, 2] {
                let Outcome::Error { error } = ask(&mut connection, id, Method::Status).await
                else {
                    panic!("an error reply");
                };
                assert_eq!(error.code, "response_too_large");
                assert_eq!(error.data["limit_bytes"], devlanes_proto::MAX_FRAME_SIZE);
            }
            token.cancel();
            running.await.expect("join").expect("clean shutdown");
        })
        .await
        .expect("timeout");
    }

    #[tokio::test]
    /// A failed reconcile still shuts the handler down (records exits, closes
    /// the store), removes the socket and reports `CODE: message`.
    async fn a_failed_reconcile_shuts_down_and_reports() {
        let (_dir, options) = short_options();
        let socket = options.socket.clone();
        let server = Server::bind(options).await.expect("bind");
        let handler = Arc::new(Fixed {
            reply: Value::Null,
            broken: true,
            shut_down: Default::default(),
        });
        let mut announced = false;
        let result = server
            .run(handler.clone(), || {
                announced = true;
                Ok(())
            })
            .await;
        let Err(Error::Reconcile(message)) = result else {
            panic!("a reconcile error: {result:?}");
        };
        assert_eq!(message, "STATE_ERROR: state.db is unreadable");
        assert!(!announced, "never announced ready");
        assert!(handler.shut_down.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!socket.exists());
    }
}
