//! Box side: accepts client streams and answers them from injected services.

use std::io;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout};

use crate::mux::{self, MuxStream};
use crate::registry::{AuthorizedTarget, Registry, RegistrySource, RegistryUpdate};
use crate::rpc::{self, RpcHandler};
use crate::session::Timing;
use crate::sync::{self, SyncSource};
use crate::transport::{BoxDuplex, BoxError, Duplex};
use crate::wire::{
    self, BoxControl, Channel, ClientControl, DialTarget, OpenReply, OpenRequest, PROTOCOL_VERSION,
    RejectReason, WireError,
};

/// Connects to an authorized endpoint on the box.
#[async_trait]
pub trait Dialer: Send + Sync {
    /// Opens a byte stream to `target`, which already passed registry authorization.
    async fn dial(&self, target: &AuthorizedTarget) -> io::Result<BoxDuplex>;
}

/// Dials `127.0.0.1:<port>`, then `[::1]:<port>`, matching how services bind.
pub struct LoopbackDialer;

#[async_trait]
impl Dialer for LoopbackDialer {
    async fn dial(&self, target: &AuthorizedTarget) -> io::Result<BoxDuplex> {
        let port = target.port();
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => Ok(Box::new(stream)),
            Err(v4) => match TcpStream::connect(("::1", port)).await {
                Ok(stream) => Ok(Box::new(stream)),
                Err(_) => Err(v4),
            },
        }
    }
}

/// The box daemon's pieces that `serve` delegates to.
#[derive(Clone)]
pub struct BoxServices {
    /// Registry snapshot and changes; also the authorization source for dials.
    pub registry: Arc<dyn RegistrySource>,
    /// Handler for forwarded daemon requests.
    pub rpc: Arc<dyn RpcHandler>,
    /// Connects authorized data streams.
    pub dialer: Arc<dyn Dialer>,
    /// Sync feeds; use [`crate::NoSync`] when unsupported.
    pub sync: Arc<dyn SyncSource>,
}

/// Box-side session settings.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Version label sent to clients for diagnostics, such as `devlanes 0.1.0`.
    pub server: String,
    /// Ping cadence; the box closes after `ping_interval + ping_timeout` of silence.
    pub timing: Timing,
    /// Limit for connecting a data stream's target.
    pub dial_timeout: Duration,
}

impl ServeOptions {
    /// Default timing with the given version label.
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            timing: Timing::default(),
            dial_timeout: Duration::from_secs(10),
        }
    }
}

/// Why a box-side session ended abnormally.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Stream framing or I/O failed on the control stream.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The client did not complete the handshake in time.
    #[error("client did not complete the handshake in time")]
    HandshakeTimeout,
    /// The client broke the protocol during the handshake.
    #[error("handshake failed: {0}")]
    Handshake(&'static str),
    /// The client speaks another protocol revision; it was told so.
    #[error("client {client} speaks protocol {protocol}, this box speaks {PROTOCOL_VERSION}")]
    IncompatibleClient {
        /// The client's protocol revision.
        protocol: u32,
        /// The client's version label.
        client: String,
    },
    /// The registry source failed to subscribe.
    #[error("registry source: {0}")]
    Registry(BoxError),
    /// The registry source ended its update stream.
    #[error("registry source closed")]
    RegistryClosed,
    /// No ping arrived within the idle limit, or a control write stalled.
    #[error("client stopped responding")]
    ClientTimeout,
}

/// State shared by per-stream tasks of one session.
struct StreamContext {
    registry: Arc<RwLock<Registry>>,
    services: BoxServices,
    handshake_timeout: Duration,
    dial_timeout: Duration,
}

/// Runs one box-side session over `io` until the client leaves.
///
/// The first stream must be the control stream. Returns `Ok(())` when the
/// client closes the session; per-stream tasks are aborted on return.
pub async fn serve<S>(io: S, services: BoxServices, options: ServeOptions) -> Result<(), ServeError>
where
    S: Duplex + 'static,
{
    let timing = options.timing;
    let mut mux = mux::server(Box::new(io));
    let first = timeout(timing.handshake_timeout, mux.accept())
        .await
        .map_err(|_| ServeError::HandshakeTimeout)?
        .ok_or(ServeError::Handshake(
            "connection closed before the control stream",
        ))?;
    let mut control = wire::channel(first);
    let open = timeout(
        timing.handshake_timeout,
        wire::recv::<_, OpenRequest>(&mut control),
    )
    .await
    .map_err(|_| ServeError::HandshakeTimeout)??;
    let Some((OpenRequest::Control { protocol, client }, _)) = open else {
        return Err(ServeError::Handshake(
            "first stream must be the control stream",
        ));
    };
    if protocol != PROTOCOL_VERSION {
        let reply = BoxControl::Incompatible {
            protocol: PROTOCOL_VERSION,
            server: options.server.clone(),
        };
        let _ = timeout(timing.handshake_timeout, async {
            wire::send(&mut control, &reply).await?;
            control.close().await
        })
        .await;
        return Err(ServeError::IncompatibleClient { protocol, client });
    }
    let feed = services
        .registry
        .subscribe()
        .await
        .map_err(ServeError::Registry)?;
    let registry = Arc::new(RwLock::new(Registry::from_snapshot(feed.snapshot.clone())));
    let welcome = BoxControl::Welcome {
        protocol: PROTOCOL_VERSION,
        server: options.server.clone(),
    };
    send_control(&mut control, &welcome, timing).await?;
    let snapshot = BoxControl::Snapshot {
        snapshot: feed.snapshot,
    };
    send_control(&mut control, &snapshot, timing).await?;
    tracing::debug!("client {client} connected");

    let context = Arc::new(StreamContext {
        registry: registry.clone(),
        services,
        handshake_timeout: timing.handshake_timeout,
        dial_timeout: options.dial_timeout,
    });
    let idle_limit = timing.ping_interval + timing.ping_timeout;
    let mut deadline = Instant::now() + idle_limit;
    let mut updates = feed.updates;
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            stream = mux.accept() => match stream {
                Some(stream) => {
                    tasks.spawn(handle_stream(stream, context.clone()));
                }
                None => return Ok(()),
            },
            message = wire::recv::<_, ClientControl>(&mut control) => match message? {
                Some((ClientControl::Ping { nonce }, _)) => {
                    deadline = Instant::now() + idle_limit;
                    send_control(&mut control, &BoxControl::Pong { nonce }, timing).await?;
                }
                None => return Ok(()),
            },
            update = updates.next() => {
                // Authorization switches to the new state before the client hears about it.
                let message = match update {
                    Some(RegistryUpdate::Event(event)) => {
                        write(&registry).apply(event.clone());
                        BoxControl::Event { event }
                    }
                    Some(RegistryUpdate::Resync(snapshot)) => {
                        write(&registry).replace(snapshot.clone());
                        BoxControl::Snapshot { snapshot }
                    }
                    None => return Err(ServeError::RegistryClosed),
                };
                send_control(&mut control, &message, timing).await?;
            }
            () = tokio::time::sleep_until(deadline) => return Err(ServeError::ClientTimeout),
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

/// Writes a control message; a write stalled past the ping timeout ends the session.
async fn send_control(
    control: &mut Channel<MuxStream>,
    message: &BoxControl,
    timing: Timing,
) -> Result<(), ServeError> {
    timeout(timing.ping_timeout, wire::send(control, message))
        .await
        .map_err(|_| ServeError::ClientTimeout)??;
    Ok(())
}

/// Write-locks the registry; poisoning cannot leave it half-updated, so it is ignored.
fn write(registry: &RwLock<Registry>) -> std::sync::RwLockWriteGuard<'_, Registry> {
    registry.write().unwrap_or_else(PoisonError::into_inner)
}

/// Reads the open frame of a non-control stream and dispatches it.
async fn handle_stream(stream: MuxStream, context: Arc<StreamContext>) {
    let mut channel = wire::channel(stream);
    let open = match timeout(
        context.handshake_timeout,
        wire::recv::<_, OpenRequest>(&mut channel),
    )
    .await
    {
        Ok(Ok(Some((open, _)))) => open,
        _ => return,
    };
    match open {
        OpenRequest::Control { .. } => tracing::warn!("ignoring a second control stream"),
        OpenRequest::Rpc { request } => {
            rpc::serve_call(channel, request, context.services.rpc.clone()).await;
        }
        OpenRequest::Sync { request } => {
            sync::serve_subscription(channel, request, context.services.sync.clone()).await;
        }
        OpenRequest::Data { target } => serve_data(channel, target, &context).await,
    }
}

/// Authorizes, dials and splices one data stream.
async fn serve_data(mut channel: Channel<MuxStream>, target: DialTarget, context: &StreamContext) {
    let authorized = context
        .registry
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .authorize(&target);
    let target = match authorized {
        Ok(target) => target,
        Err(reason) => {
            let message = match reason {
                RejectReason::Hidden => "target belongs to a hidden unit".to_owned(),
                _ => "target is not registered on this box".to_owned(),
            };
            reject(&mut channel, reason, message).await;
            return;
        }
    };
    let dialed = timeout(context.dial_timeout, context.services.dialer.dial(&target)).await;
    let mut upstream = match dialed {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(error)) => {
            let message = format!("dial port {}: {error}", target.port());
            reject(&mut channel, RejectReason::DialFailed, message).await;
            return;
        }
        Err(_) => {
            let message = format!("dial port {} timed out", target.port());
            reject(&mut channel, RejectReason::DialFailed, message).await;
            return;
        }
    };
    if wire::send(&mut channel, &OpenReply::Accepted)
        .await
        .is_err()
    {
        return;
    }
    let mut downstream = wire::into_raw(channel);
    if let Err(error) = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await {
        tracing::debug!("data stream to port {} ended: {error}", target.port());
    }
}

/// Tells the client why its open failed, then closes the stream.
async fn reject(channel: &mut Channel<MuxStream>, reason: RejectReason, message: String) {
    let _ = wire::send(channel, &OpenReply::Rejected { reason, message }).await;
    let _ = channel.close().await;
}
