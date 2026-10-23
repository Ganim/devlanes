//! Local API runtime. Managed resources are supplied by the composition root.
//!
//! [`Server`] owns the socket and the singleton lock; [`Engine`] is the
//! [`Handler`] that runs lanes. Concrete integrations arrive through
//! [`integrations::Integrations`].

mod attach;
pub mod bus;
mod certs;
mod control;
pub mod diagnostics;
mod dns;
mod engine;
mod files;
pub mod hosts;
mod input;
pub mod integrations;
mod lifecycle;
mod local_sync;
mod maintenance;
mod model;
mod orphans;
mod pacing;
mod phase_progress;
mod platforms;
pub mod process;
mod query;
pub mod ready;
mod reconcile;
mod registry;
mod remote;
mod rendered;
#[cfg(windows)]
pub mod reserved_ports;
mod resolve;
mod routes;
mod sampler;
mod screen;
pub mod secrets;
mod spawn_error;
mod stream;
pub mod units;
pub mod update;

pub use engine::{Engine, EngineOptions};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_ipc::{
    Connection, Frame,
    local::{self, BindError, LocalListener, LocalStream},
};
use devlanes_proto::{Control, Event, Hello, Method, Outcome, Request, Response, RpcError};
#[cfg(unix)]
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Startup failures leave the current daemon and its processes untouched.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("daemon I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("a daemon already owns the lock")]
    AlreadyRunning,
    /// Windows: another process created the daemon's pipe name first, while
    /// this daemon held the lock, so it is no daemon of this home.
    #[cfg(windows)]
    #[error("another process holds the daemon's pipe {}", .0.display())]
    EndpointTaken(PathBuf),
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

    /// Stops owned processes but preserves adopted containers. Returns the
    /// report an explicit `daemon.shutdown` request answers with.
    async fn shutdown(&self, request: Shutdown) -> Result<Value, RpcError>;

    /// Resolves when the daemon has nothing left to do and may exit on its
    /// own ([`ShutdownReason::Idle`]); never, by default. The server exits
    /// only when no client is connected at that moment, and asks again
    /// otherwise, so an implementation must not resolve at once every time.
    async fn idle(&self) {
        std::future::pending::<()>().await;
    }
}

/// Why the daemon shuts down. Only an explicit shutdown runs the lanes'
/// `lane.stop` tasks; on a signal units stop without them, so a logout or a
/// machine shutdown does not wait for tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    /// `daemon.shutdown`: `devlanes daemon stop` or `daemon restart`.
    Explicit,
    /// SIGTERM or SIGINT (launchd, systemd, logout, system shutdown), a failed
    /// reconcile, or an embedder cancelling [`Server::shutdown_token`].
    Signal,
    /// [`Handler::idle`]: the last client left and nothing runs.
    Idle,
}

impl ShutdownReason {
    /// `explicit`, `signal` or `idle`, as reports and logs show it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Signal => "signal",
            Self::Idle => "idle",
        }
    }
}

/// What [`Handler::shutdown`] is asked to do.
#[derive(Debug, Clone)]
pub struct Shutdown {
    pub reason: ShutdownReason,
    /// Fires on a signal during an explicit shutdown: the running `lane.stop`
    /// tasks stop and the rest are skipped, so the units still stop in time.
    pub interrupt: CancellationToken,
}

impl Shutdown {
    /// A shutdown for `reason` that nothing interrupts.
    pub fn new(reason: ShutdownReason) -> Self {
        Self {
            reason,
            interrupt: CancellationToken::new(),
        }
    }
}

/// Longest the server waits for explicit `daemon.shutdown` requests to deliver
/// their answers before it closes the connections.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// The explicit `daemon.shutdown` requests and the report they answer with.
struct Stops {
    state: std::sync::Mutex<StopState>,
    /// Cancelled by [`Server::interrupt_token`] holders (the signal handler).
    interrupt: CancellationToken,
    /// The handler's shutdown result, once it is done.
    report: tokio::sync::watch::Sender<Option<Result<Value, RpcError>>>,
}

struct StopState {
    explicit: bool,
    /// The server left its loop because the handler went idle.
    idle: bool,
    /// The run loop read the reason; later requests no longer change it.
    decided: bool,
    /// Cloned by each request waiting for the report and dropped once its
    /// answer was delivered; `None` once the server stopped waiting for them.
    answering: Option<mpsc::Sender<()>>,
}

/// A `daemon.shutdown` request's view of the shutdown: the report to wait for
/// and the guard that keeps the connections open until its answer is out.
type StopWaiter = (
    tokio::sync::watch::Receiver<Option<Result<Value, RpcError>>>,
    Option<mpsc::Sender<()>>,
);

impl Stops {
    /// The shared state and the receiver that ends once every answer is out.
    fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (answering, answered) = mpsc::channel(1);
        let stops = Self {
            state: std::sync::Mutex::new(StopState {
                explicit: false,
                idle: false,
                decided: false,
                answering: Some(answering),
            }),
            interrupt: CancellationToken::new(),
            report: tokio::sync::watch::channel(None).0,
        };
        (Arc::new(stops), answered)
    }

    /// Records an explicit request, unless a signal's shutdown already began.
    fn join(&self) -> StopWaiter {
        let mut state = model::lock(&self.state);
        if !state.decided {
            state.explicit = true;
        }
        (self.report.subscribe(), state.answering.clone())
    }

    /// Records that the daemon stops because it is idle; an explicit request
    /// that arrived first still wins.
    fn idle(&self) {
        let mut state = model::lock(&self.state);
        if !state.decided {
            state.idle = true;
        }
    }

    /// The reason, fixed from here on.
    fn decide(&self) -> ShutdownReason {
        let mut state = model::lock(&self.state);
        state.decided = true;
        if state.explicit {
            ShutdownReason::Explicit
        } else if state.idle {
            ShutdownReason::Idle
        } else {
            ShutdownReason::Signal
        }
    }

    /// Publishes the report; requests arriving later answer with it at once.
    fn finish(&self, report: Result<Value, RpcError>) {
        self.report.send_replace(Some(report));
        model::lock(&self.state).answering = None;
    }
}

/// The `daemon.shutdown` answer: the handler's report plus `stopping` and the
/// live process count at the request, the fields older clients read.
fn shutdown_answer(
    report: Option<Result<Value, RpcError>>,
    processes: usize,
) -> Result<Value, RpcError> {
    match report {
        Some(Ok(Value::Object(mut fields))) => {
            fields.insert("stopping".into(), json!(true));
            fields.insert("processes".into(), json!(processes));
            Ok(Value::Object(fields))
        }
        Some(Err(error)) => Err(error),
        _ => Ok(json!({"stopping": true, "processes": processes})),
    }
}

/// Owns the singleton lock until all shutdown work and socket cleanup complete.
pub struct Server {
    listener: LocalListener,
    _lock: File,
    options: Options,
    started: Instant,
    shutdown: CancellationToken,
    stops: Arc<Stops>,
    answered: mpsc::Receiver<()>,
}

impl Server {
    /// Acquires the lock before inspecting a stale socket, preventing spawn races.
    pub async fn bind(options: Options) -> Result<Self, Error> {
        let prepared = options.clone();
        let lock = tokio::task::spawn_blocking(move || prepare(&prepared)).await??;
        let listener = match LocalListener::bind(&options.socket).await {
            Ok(listener) => listener,
            Err(BindError::AlreadyListening(_)) => return Err(Error::AlreadyRunning),
            #[cfg(windows)]
            Err(BindError::Taken(path)) => return Err(Error::EndpointTaken(path)),
            Err(BindError::Io(error)) => return Err(error.into()),
        };
        let (stops, answered) = Stops::new();
        Ok(Self {
            listener,
            _lock: lock,
            options,
            started: Instant::now(),
            shutdown: CancellationToken::new(),
            stops,
            answered,
        })
    }

    /// Cancelling it shuts the daemon down: units stop as on `daemon stop`, but
    /// without `lane.stop` tasks ([`ShutdownReason::Signal`]) unless a
    /// `daemon.shutdown` request asked first. It also tells the handler that new
    /// work is refused.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Cancelled on a signal: interrupts the `lane.stop` tasks of an explicit
    /// shutdown that is running, so a service manager's stop is not held up.
    pub fn interrupt_token(&self) -> CancellationToken {
        self.stops.interrupt.clone()
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
            if let Err(stopped) = handler
                .shutdown(Shutdown::new(ShutdownReason::Signal))
                .await
            {
                tracing::warn!(code = %stopped.code, "shutdown after a failed reconcile");
            }
            let _ = local::remove(&self.options.socket).await;
            return Err(Error::Reconcile(startup_reason(&error)));
        }
        ready()?;
        let Self {
            mut listener,
            options,
            started,
            shutdown,
            stops,
            mut answered,
            _lock,
        } = self;
        let mut clients = tokio::task::JoinSet::new();
        let idler = handler.clone();
        let mut idle = idler.idle();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = &mut idle => {
                    // A client that connected meanwhile may be about to start
                    // something: ask again later rather than cut it off.
                    if clients.is_empty() {
                        stops.idle();
                        break;
                    }
                    idle = idler.idle();
                }
                accepted = listener.accept() => {
                    // One client's failure must never end the daemon: a peer that closes
                    // before its credentials are read (macOS: ENOTCONN) used to exit here.
                    let stream = match accepted {
                        Ok(stream) => stream,
                        Err(error) => {
                            tracing::warn!(%error, "accept failed");
                            // Out of descriptors: give closing units a moment instead of spinning.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    match stream.peer_is_current_user() {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            tracing::debug!(%error, "client left before its credentials were read");
                            continue;
                        }
                    }
                    let handler = handler.clone();
                    let shutdown = shutdown.clone();
                    let stops = stops.clone();
                    let socket = options.socket.clone();
                    clients.spawn(async move {
                        if let Err(error) = serve_client(stream, handler, shutdown, stops, started, socket).await {
                            tracing::debug!(%error, "client connection ended");
                        }
                    });
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = result { tracing::error!(%error, "client task failed"); }
                }
            }
        }
        drop(idle);
        // No new connection from here on; connected clients get `daemon_stopping`
        // for anything that would start work.
        drop(listener);
        shutdown.cancel();
        // Stop resources before closing subscribers so they can observe final state.
        let request = Shutdown {
            reason: stops.decide(),
            interrupt: stops.interrupt.clone(),
        };
        tracing::info!(reason = request.reason.as_str(), "daemon shutting down");
        let stopped = handler.shutdown(request).await;
        stops.finish(stopped.clone());
        // `recv` ends once every waiting request delivered its answer.
        let _ = tokio::time::timeout(ANSWER_WAIT, answered.recv()).await;
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        let remove = local::remove(&options.socket).await;
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
#[cfg(unix)]
fn prepare(options: &Options) -> Result<File, Error> {
    let parent = options
        .socket
        .parent()
        .ok_or_else(|| Error::UnsafeDirectory(options.socket.clone()))?;
    let lock = lock_singleton(&options.home)?;
    secure_directory(parent)?;
    Ok(lock)
}

/// Takes the singleton lock; the pipe lives outside the filesystem, protected
/// by its own descriptor.
#[cfg(windows)]
fn prepare(options: &Options) -> Result<File, Error> {
    lock_singleton(&options.home)
}

/// Takes the daemon's singleton lock under `home` (blocking I/O). Offline
/// maintenance such as the CLI's local `daemon reset-state` holds it, so no daemon
/// starts meanwhile; [`Error::AlreadyRunning`] means one owns it. Dropping the
/// file releases the lock.
#[cfg(unix)]
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

/// Takes the daemon's singleton lock under `home` (blocking I/O) with
/// `LockFileEx`; [`Error::AlreadyRunning`] means another process holds it.
/// Dropping the file releases the lock. A symlink or junction at the lock path
/// is opened itself, never followed, and refused, as `O_NOFOLLOW` does on Unix.
#[cfg(windows)]
pub fn lock_singleton(home: &Path) -> Result<File, Error> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    // Win32 values: open a link itself; the attribute every link carries.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    secure_directory(home)?;
    let run = home.join("run");
    secure_directory(&run)?;
    let path = run.join("daemon.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
    if lock.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is a symlink or junction", path.display()),
        )));
    }
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::AlreadyRunning),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Creates `path` private, or tightens an existing directory of this user to
/// 0700. A symlink, a non-directory or another user's directory is refused: it
/// may not be ours to change.
#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
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
    if !metadata.is_dir() || metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(Error::UnsafeDirectory(path.into()));
    }
    if metadata.mode() & 0o077 != 0 {
        let mode = metadata.mode() & 0o7777;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "tightened a devlanes directory to 0700");
    }
    Ok(())
}

/// Creates `path` private (owned by this user, access for this user, SYSTEM
/// and Administrators), or tightens an existing, empty directory of this user
/// that others can reach. A symlink or junction, a non-directory, another
/// account's directory or a non-empty one others can reach is refused: its
/// entries would keep their grants, so fixing it is the user's call.
#[cfg(windows)]
fn secure_directory(path: &Path) -> Result<(), Error> {
    use devlanes_sys_windows::security::{Outcome, ensure_private_dir};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match ensure_private_dir(path)? {
        Ok(Outcome::Tightened) => {
            tracing::warn!(path = %path.display(), "tightened a devlanes directory to owner-only access");
            Ok(())
        }
        Ok(Outcome::Created | Outcome::Present) => Ok(()),
        Err(_) => Err(Error::UnsafeDirectory(path.into())),
    }
}

/// A request's tap and the task forwarding its records to the client.
type RequestTap = (diagnostics::TapGuard, tokio::task::JoinHandle<()>);

/// The span every request runs in, and its diagnostics tap when the client
/// asked for one. Another protocol's client gets no tap: it may not know the
/// event.
fn request_span(
    request: &Request,
    compatible: bool,
    session: &Session,
) -> (tracing::Span, Option<RequestTap>) {
    let verbose = request.context.verbose;
    // Registered before the span exists: the tap filter only passes spans while
    // a tap is active.
    let tap = (compatible && verbose > 0).then(|| diagnostics::register(verbose));
    let method = serde_json::to_value(request.method)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let span = tracing::info_span!(
        target: "devlanes_daemon::request",
        "request",
        id = request.id,
        method = %method,
        session = session.id,
        tap = tracing::field::Empty,
    );
    let tap = tap.map(|(guard, mut receiver)| {
        span.record("tap", guard.id());
        let session = session.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                let event = Event {
                    seq: 0,
                    event: devlanes_proto::events::DIAGNOSTIC.into(),
                    data,
                };
                if session.event(event).await.is_err() {
                    break;
                }
            }
        });
        (guard, forwarder)
    });
    tracing::debug!(parent: &span, verbose, "request received");
    (span, tap)
}

/// Closes a request's tap and lets its records reach the client before the
/// response, reporting records a slow client lost.
async fn finish_tap(
    guard: diagnostics::TapGuard,
    forwarder: tokio::task::JoinHandle<()>,
    session: &Session,
) {
    // Unregistering drops the sender; the forwarder ends once it drained.
    let dropped = guard.finish();
    let _ = tokio::time::timeout(Duration::from_secs(5), forwarder).await;
    if dropped > 0 {
        let _ = session
            .event(Event {
                seq: 0,
                event: devlanes_proto::events::DIAGNOSTIC.into(),
                data: json!({"kind": "dropped", "count": dropped}),
            })
            .await;
    }
}

/// One bounded outgoing queue prevents slow terminals from consuming unbounded memory.
async fn serve_client(
    stream: LocalStream,
    handler: Arc<dyn Handler>,
    shutdown: CancellationToken,
    stops: Arc<Stops>,
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
                            let stops = stops.clone();
                            let socket = socket.clone();
                            let mismatch = mismatch.clone();
                            // Requests outlive connections: detaching must not cancel a lane start.
                            tokio::spawn(async move {
                                let id = request.id;
                                // Held by a `daemon.shutdown` request until its answer is out.
                                let mut answering = None;
                                let (span, tap) = request_span(&request, compatible, &session);
                                let begun = Instant::now();
                                let result = async { match request.method {
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
                                    // Answered once the shutdown is done (`Server::run` runs it
                                    // after the accept loop), with what it stopped and ran.
                                    Method::DaemonShutdown => {
                                        let processes = handler.managed_processes();
                                        let (mut report, guard) = stops.join();
                                        answering = guard;
                                        shutdown.cancel();
                                        let report = report.wait_for(Option::is_some).await.ok().and_then(|report| report.clone());
                                        shutdown_answer(report, processes)
                                    }
                                    _ if !compatible => Err(mismatch),
                                    _ => handler.handle(request, session.clone()).await,
                                } }.instrument(span.clone()).await;
                                tracing::debug!(
                                    parent: &span,
                                    elapsed_ms = begun.elapsed().as_millis() as u64,
                                    error = result.as_ref().err().map(|error| error.code.as_str()),
                                    "answered"
                                );
                                drop(span);
                                if let Some((guard, forwarder)) = tap {
                                    finish_tap(guard, forwarder, &session).await;
                                }
                                let outcome = match result {
                                    Ok(data) => Outcome::Ok { data },
                                    Err(error) => Outcome::Error { error },
                                };
                                let _ = session.send(Frame::Control(Control::Response(Response { id, result: outcome }))).await;
                                drop(answering);
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

    /// The endpoint for `home`: a socket inside it, or on Windows a pipe named
    /// after its test directory, unique to the test.
    fn endpoint(home: &Path) -> PathBuf {
        #[cfg(unix)]
        let endpoint = home.join("run/d.sock");
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\devlanes-daemon-test-{}-{}",
            std::process::id(),
            home.parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        ));
        endpoint
    }

    /// A second daemon on one home answers `already-running`.
    #[tokio::test]
    async fn lock_prevents_a_second_bind() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let options = Options {
            socket: endpoint(&home),
            home,
        };
        let _first = Server::bind(options.clone()).await?;
        assert!(matches!(
            Server::bind(options).await,
            Err(Error::AlreadyRunning)
        ));
        Ok(())
    }

    /// A pipe someone else created first is refused, not shared.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_squatted_pipe_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let options = Options {
            socket: endpoint(&home),
            home,
        };
        let pipes = devlanes_sys_windows::pipe::PipeFactory::current_user(&options.socket)?;
        let _squatter = pipes.create(true)?;
        assert!(matches!(
            Server::bind(options).await,
            Err(Error::EndpointTaken(_))
        ));
        Ok(())
    }

    /// Grants Everyone read access to `path` and what it will hold.
    #[cfg(windows)]
    fn grant_everyone(path: &Path) -> std::io::Result<()> {
        let icacls = devlanes_sys_windows::file::system_directory()?.join("icacls.exe");
        let status = std::process::Command::new(icacls)
            .arg(path)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .stdout(std::process::Stdio::null())
            .status()?;
        assert!(status.success(), "icacls");
        Ok(())
    }

    /// An empty directory of this user that others can read is made private;
    /// one with entries is refused, since they would keep their grants.
    #[cfg(windows)]
    #[test]
    fn tightens_an_empty_open_home() -> Result<(), Box<dyn std::error::Error>> {
        use devlanes_sys_windows::security::check_private_dir;
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        std::fs::create_dir(&home)?;
        grant_everyone(&home)?;
        assert!(check_private_dir(&home)?.is_err());
        secure_directory(&home)?;
        assert_eq!(check_private_dir(&home)?, Ok(()));
        let full = dir.path().join("full");
        std::fs::create_dir(&full)?;
        std::fs::write(full.join("state.db"), "x")?;
        grant_everyone(&full)?;
        assert!(matches!(
            secure_directory(&full),
            Err(Error::UnsafeDirectory(_))
        ));
        Ok(())
    }

    /// A junction is refused as a home and as the lock file.
    #[cfg(windows)]
    #[test]
    fn refuses_junctions() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target");
        std::fs::create_dir(&target)?;
        let junction = |link: &Path| -> std::io::Result<()> {
            let cmd = devlanes_sys_windows::file::system_directory()?.join("cmd.exe");
            let status = std::process::Command::new(cmd)
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link)
                .arg(&target)
                .stdout(std::process::Stdio::null())
                .status()?;
            assert!(status.success(), "mklink");
            Ok(())
        };
        let link = dir.path().join("alias");
        junction(&link)?;
        assert!(matches!(
            secure_directory(&link),
            Err(Error::UnsafeDirectory(_))
        ));
        let home = dir.path().join("home");
        secure_directory(&home)?;
        secure_directory(&home.join("run"))?;
        junction(&home.join("run").join("daemon.lock"))?;
        assert!(lock_singleton(&home).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    /// An existing directory of this user that others can read is made private.
    fn tightens_an_open_home() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        std::fs::create_dir(&home)?;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755))?;
        secure_directory(&home)?;
        assert_eq!(std::fs::metadata(&home)?.mode() & 0o7777, 0o700);
        Ok(())
    }

    #[cfg(unix)]
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
    /// `shut_down` records the reason of its shutdown.
    struct Fixed {
        reply: Value,
        broken: bool,
        shut_down: std::sync::Mutex<Option<ShutdownReason>>,
        /// Goes idle this long after each ask; never without.
        idle_after: Option<Duration>,
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
        async fn shutdown(&self, request: Shutdown) -> Result<Value, RpcError> {
            *model::lock(&self.shut_down) = Some(request.reason);
            Ok(json!({"reason": request.reason.as_str(), "stopped": []}))
        }
        async fn idle(&self) {
            match self.idle_after {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending().await,
            }
        }
    }

    /// Options under a short temporary directory (socket paths are bounded).
    fn short_options() -> (tempfile::TempDir, Options) {
        let mut builder = tempfile::Builder::new();
        builder.prefix("dls-");
        #[cfg(unix)]
        let dir = builder.tempdir_in("/tmp").expect("tempdir");
        #[cfg(windows)]
        let dir = builder.tempdir().expect("tempdir");
        let home = dir.path().join("h");
        let options = Options {
            socket: endpoint(&home),
            home,
        };
        (dir, options)
    }

    /// A connection to the endpoint at `socket`.
    async fn connect(socket: &Path) -> LocalStream {
        local::connect(socket).await.expect("connect")
    }

    /// Sends request `id` and waits for its outcome.
    async fn ask(connection: &mut Connection<LocalStream>, id: u64, method: Method) -> Outcome {
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
                idle_after: None,
            });
            let running = tokio::spawn(server.run(handler, || Ok(())));
            let mut connection = Connection::new(connect(&socket).await);
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
            idle_after: None,
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
        assert_eq!(
            *model::lock(&handler.shut_down),
            Some(ShutdownReason::Signal)
        );
        // A pipe is gone with its listener; looking at its path would connect.
        #[cfg(unix)]
        assert!(!socket.exists());
        #[cfg(windows)]
        assert!(matches!(
            local::connect(&socket).await,
            Err(local::ConnectError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    /// A served handler answering `null`, with a handshaken connection to it.
    async fn serve() -> (
        tempfile::TempDir,
        Arc<Fixed>,
        CancellationToken,
        tokio::task::JoinHandle<Result<(), Error>>,
        Connection<LocalStream>,
    ) {
        let (dir, options) = short_options();
        let socket = options.socket.clone();
        let server = Server::bind(options).await.expect("bind");
        let token = server.shutdown_token();
        let handler = Arc::new(Fixed {
            reply: Value::Null,
            broken: false,
            shut_down: Default::default(),
            idle_after: None,
        });
        let running = tokio::spawn(server.run(handler.clone(), || Ok(())));
        let mut connection = Connection::new(connect(&socket).await);
        connection
            .send(Frame::Control(Control::Hello(Hello::current(0))))
            .await
            .expect("hello");
        connection.recv().await.expect("hello reply");
        (dir, handler, token, running, connection)
    }

    #[tokio::test]
    /// `daemon.shutdown` is an explicit shutdown, answered after the handler's
    /// shutdown with its report plus the fields older clients read.
    async fn daemon_shutdown_answers_with_the_report() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (_dir, handler, _token, running, mut connection) = serve().await;
            let Outcome::Ok { data } = ask(&mut connection, 1, Method::DaemonShutdown).await else {
                panic!("an answer");
            };
            assert_eq!(data["reason"], "explicit", "{data}");
            assert_eq!(data["stopped"], json!([]), "{data}");
            assert_eq!(data["stopping"], true, "{data}");
            assert_eq!(data["processes"], 0, "{data}");
            assert_eq!(
                *model::lock(&handler.shut_down),
                Some(ShutdownReason::Explicit)
            );
            running.await.expect("join").expect("clean shutdown");
        })
        .await
        .expect("timeout");
    }

    #[tokio::test]
    /// An idle handler ends the daemon only once no client is connected; the
    /// shutdown says it was idle and the socket goes.
    async fn an_idle_handler_waits_for_the_last_client() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (_dir, options) = short_options();
            let socket = options.socket.clone();
            let server = Server::bind(options).await.expect("bind");
            let handler = Arc::new(Fixed {
                reply: Value::Null,
                broken: false,
                shut_down: Default::default(),
                idle_after: Some(Duration::from_millis(20)),
            });
            let running = tokio::spawn(server.run(handler.clone(), || Ok(())));
            let mut connection = Connection::new(connect(&socket).await);
            connection
                .send(Frame::Control(Control::Hello(Hello::current(0))))
                .await
                .expect("hello");
            connection.recv().await.expect("hello reply");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!running.is_finished(), "a client is connected");
            assert!(matches!(
                ask(&mut connection, 1, Method::Status).await,
                Outcome::Ok { .. }
            ));
            drop(connection);
            running.await.expect("join").expect("clean shutdown");
            assert_eq!(*model::lock(&handler.shut_down), Some(ShutdownReason::Idle));
            assert!(!socket.exists());
        })
        .await
        .expect("timeout");
    }

    #[tokio::test]
    /// Cancelling the token (the signal handler's path) is a signal shutdown.
    async fn a_cancelled_token_is_a_signal_shutdown() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (_dir, handler, token, running, _connection) = serve().await;
            token.cancel();
            running.await.expect("join").expect("clean shutdown");
            assert_eq!(
                *model::lock(&handler.shut_down),
                Some(ShutdownReason::Signal)
            );
        })
        .await
        .expect("timeout");
    }
}
