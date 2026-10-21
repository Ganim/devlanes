//! Client side: one session over one transport connection.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

use crate::mux::{self, MuxClient, MuxError, MuxStream};
use crate::registry::ClientRegistry;
use crate::rpc::{RpcCall, RpcCallError};
use crate::sync::SyncSubscription;
use crate::transport::{BoxDuplex, Transport, TransportError};
use crate::wire::{
    self, BoxControl, Channel, ClientControl, DialTarget, OpenReply, OpenRequest, PROTOCOL_VERSION,
    Prefixed, RejectReason, WireError,
};

/// Liveness and handshake limits shared by both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How often the client pings.
    pub ping_interval: Duration,
    /// How long a ping may stay unanswered before the session is dead.
    pub ping_timeout: Duration,
    /// Limit for handshakes and for the first frame of each stream.
    pub handshake_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(10),
            ping_timeout: Duration::from_secs(20),
            handshake_timeout: Duration::from_secs(15),
        }
    }
}

/// Client-side session settings.
#[derive(Clone)]
pub struct SessionOptions {
    /// Version label sent to the box for diagnostics.
    pub client: String,
    /// Ping and handshake limits.
    pub timing: Timing,
    /// State replaced by each session's snapshot; share it across reconnects.
    pub registry: ClientRegistry,
}

impl SessionOptions {
    /// Default timing and a fresh registry.
    pub fn new(client: impl Into<String>) -> Self {
        Self {
            client: client.into(),
            timing: Timing::default(),
            registry: ClientRegistry::new(),
        }
    }
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// [`Session::close`] was called or every handle was dropped.
    Closed,
    /// The box closed the control stream.
    Disconnected,
    /// A ping went unanswered for [`Timing::ping_timeout`].
    PingTimeout,
    /// The control stream failed.
    Failed(String),
}

impl fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("session closed"),
            Self::Disconnected => f.write_str("box closed the session"),
            Self::PingTimeout => f.write_str("box stopped answering pings"),
            Self::Failed(reason) => write!(f, "session failed: {reason}"),
        }
    }
}

/// Failure to establish or use a session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transport could not produce a pipe.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The mux failed.
    #[error(transparent)]
    Mux(#[from] MuxError),
    /// A stream failed or carried invalid frames.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The box did not complete the handshake in time.
    #[error("box did not complete the handshake in time")]
    HandshakeTimeout,
    /// The box sent something unexpected during the handshake.
    #[error("handshake failed: {0}")]
    Handshake(&'static str),
    /// Client and box speak different protocol revisions.
    #[error(
        "the box runs {server} (protocol {remote}) but this client speaks protocol {local}. \
         Install the matching devlanes release on the box, then run `devlanes daemon restart` \
         there when its managed processes may stop. Box daemons are never upgraded automatically"
    )]
    IncompatibleProtocol {
        /// This client's revision.
        local: u32,
        /// The box's revision.
        remote: u32,
        /// The box's version label.
        server: String,
    },
    /// The box side broke the connection; the text is the pipe's own report, such
    /// as the stderr of a remote `devlanes tunnel --stdio` that exited.
    #[error("box closed the connection: {0}")]
    Closed(String),
    /// The session already ended.
    #[error("{0}")]
    Ended(SessionEnd),
}

impl SessionError {
    /// Reports whether reconnecting without user action is pointless.
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Transport(error) => error.is_permanent(),
            Self::IncompatibleProtocol { .. } => true,
            _ => false,
        }
    }
}

/// Failure to open a data stream.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The box refused the target.
    #[error("box refused the stream ({reason:?}): {message}")]
    Rejected {
        /// Refusal category.
        reason: RejectReason,
        /// Box-provided detail.
        message: String,
    },
    /// The session could not open a stream.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The open exchange failed.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The box did not answer in time.
    #[error("box did not answer the open in time")]
    Timeout,
    /// The stream closed before the box answered.
    #[error("stream closed before the box answered")]
    Interrupted,
}

/// A raw byte stream to a box endpoint.
pub type DataStream = Prefixed<MuxStream>;

/// A live client session. Clones share it; dropping the last clone closes it.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    mux: MuxClient,
    server: String,
    registry: ClientRegistry,
    timing: Timing,
    end: watch::Receiver<Option<SessionEnd>>,
    cancel: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.mux.shutdown();
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("server", &self.inner.server)
            .field("end", &*self.inner.end.borrow())
            .finish()
    }
}

impl Session {
    /// Connects through `transport` and performs the handshake.
    pub async fn connect(
        transport: &dyn Transport,
        options: SessionOptions,
    ) -> Result<Self, SessionError> {
        let io = transport.connect().await?;
        Self::establish(io, options).await
    }

    /// Runs the handshake over an existing pipe.
    ///
    /// Returns after the box's snapshot replaced `options.registry`, so callers
    /// see current state immediately.
    pub async fn establish(io: BoxDuplex, options: SessionOptions) -> Result<Self, SessionError> {
        let mux = mux::client(io);
        match handshake(&mux, &options).await {
            Ok((control, server)) => {
                let (end_tx, end) = watch::channel(None);
                let cancel = CancellationToken::new();
                tokio::spawn(run_control(
                    control,
                    options.registry.clone(),
                    options.timing,
                    cancel.clone(),
                    end_tx,
                    mux.clone(),
                ));
                Ok(Self {
                    inner: Arc::new(Inner {
                        mux,
                        server,
                        registry: options.registry,
                        timing: options.timing,
                        end,
                        cancel,
                    }),
                })
            }
            Err(error) => {
                mux.shutdown();
                Err(error)
            }
        }
    }

    /// The box's version label from the handshake.
    pub fn server(&self) -> &str {
        &self.inner.server
    }

    /// The registry this session keeps up to date.
    pub fn registry(&self) -> &ClientRegistry {
        &self.inner.registry
    }

    /// Opens a raw byte stream to `target`; the box rejects unregistered targets.
    pub async fn open(&self, target: DialTarget) -> Result<DataStream, OpenError> {
        let mut channel = self.open_stream(&OpenRequest::Data { target }).await?;
        let reply = timeout(
            self.inner.timing.handshake_timeout,
            wire::recv::<_, OpenReply>(&mut channel),
        )
        .await
        .map_err(|_| OpenError::Timeout)??;
        match reply {
            Some((OpenReply::Accepted, _)) => Ok(wire::into_raw(channel)),
            Some((OpenReply::Rejected { reason, message }, _)) => {
                Err(OpenError::Rejected { reason, message })
            }
            None => Err(OpenError::Interrupted),
        }
    }

    /// Starts a forwarded daemon request that may stream replies and take input.
    pub async fn call(&self, request: Value) -> Result<RpcCall, SessionError> {
        let channel = self.open_stream(&OpenRequest::Rpc { request }).await?;
        Ok(RpcCall::new(channel))
    }

    /// Sends a request without input and returns its first reply.
    ///
    /// Use [`Session::call`] for requests that stream several replies.
    pub async fn request(&self, request: Value) -> Result<Value, RpcCallError> {
        let mut call = self.call(request).await?;
        call.finish_input().await?;
        let value = call.next().await.ok_or(RpcCallError::NoValue)??;
        Ok(value)
    }

    /// Starts a sync subscription; the request is opaque to the transport.
    pub async fn subscribe_sync(&self, request: Value) -> Result<SyncSubscription, SessionError> {
        let channel = self.open_stream(&OpenRequest::Sync { request }).await?;
        Ok(SyncSubscription::new(channel))
    }

    /// Returns the end reason once the session ended.
    pub fn end(&self) -> Option<SessionEnd> {
        self.inner.end.borrow().clone()
    }

    /// Waits until the session ends.
    pub async fn closed(&self) -> SessionEnd {
        let mut end = self.inner.end.clone();
        match end.wait_for(Option::is_some).await {
            Ok(end) => end.clone().unwrap_or(SessionEnd::Closed),
            // The control task only drops the sender after publishing an end.
            Err(_) => SessionEnd::Closed,
        }
    }

    /// Ends the session and every stream on it.
    pub fn close(&self) {
        self.inner.cancel.cancel();
    }

    /// Opens a mux stream and sends its open frame.
    async fn open_stream(&self, open: &OpenRequest) -> Result<Channel<MuxStream>, SessionError> {
        if let Some(end) = self.end() {
            return Err(SessionError::Ended(end));
        }
        let stream = match self.inner.mux.open().await {
            Ok(stream) => stream,
            Err(error) => return Err(self.end().map_or(error.into(), SessionError::Ended)),
        };
        let mut channel = wire::channel(stream);
        wire::send(&mut channel, open).await?;
        Ok(channel)
    }
}

/// Opens the control stream, checks the protocol and applies the first snapshot.
async fn handshake(
    mux: &MuxClient,
    options: &SessionOptions,
) -> Result<(Channel<MuxStream>, String), SessionError> {
    let limit = options.timing.handshake_timeout;
    let exchange = async {
        let mut control = wire::channel(mux.open().await?);
        let hello = OpenRequest::Control {
            protocol: PROTOCOL_VERSION,
            client: options.client.clone(),
        };
        wire::send(&mut control, &hello).await?;
        let server = match wire::recv::<_, BoxControl>(&mut control).await? {
            Some((BoxControl::Welcome { protocol, server }, _)) if protocol == PROTOCOL_VERSION => {
                server
            }
            Some((
                BoxControl::Welcome { protocol, server }
                | BoxControl::Incompatible { protocol, server },
                _,
            )) => {
                return Err(SessionError::IncompatibleProtocol {
                    local: PROTOCOL_VERSION,
                    remote: protocol,
                    server,
                });
            }
            Some(_) => return Err(SessionError::Handshake("expected welcome")),
            None => return Err(SessionError::Handshake("box closed the control stream")),
        };
        match wire::recv::<_, BoxControl>(&mut control).await? {
            Some((BoxControl::Snapshot { snapshot }, _)) => options.registry.replace(snapshot),
            Some(_) => return Err(SessionError::Handshake("expected registry snapshot")),
            None => return Err(SessionError::Handshake("box closed the control stream")),
        }
        Ok((control, server))
    };
    match timeout(limit, exchange).await {
        Err(_) => Err(SessionError::HandshakeTimeout),
        Ok(Err(error @ SessionError::IncompatibleProtocol { .. })) => Err(error),
        // A closed control stream usually means the pipe died; its error says why.
        Ok(Err(error)) => match mux.failure_within(FAILURE_GRACE).await {
            Some(reason) => Err(SessionError::Closed(reason)),
            None => Err(error),
        },
        Ok(Ok(established)) => Ok(established),
    }
}

/// How long a lost stream waits for the mux driver to report the pipe's error.
const FAILURE_GRACE: Duration = Duration::from_secs(1);

/// Ends a session that lost its control stream, preferring the pipe's own error.
async fn lost(mux: &MuxClient) -> SessionEnd {
    match mux.failure_within(FAILURE_GRACE).await {
        Some(reason) => SessionEnd::Failed(reason),
        None => SessionEnd::Disconnected,
    }
}

/// Applies registry changes and pings until the session ends.
async fn run_control(
    mut control: Channel<MuxStream>,
    registry: ClientRegistry,
    timing: Timing,
    cancel: CancellationToken,
    end_tx: watch::Sender<Option<SessionEnd>>,
    mux: MuxClient,
) {
    let mut ticker =
        tokio::time::interval_at(Instant::now() + timing.ping_interval, timing.ping_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut nonce = 0u64;
    let mut outstanding: Option<(u64, Instant)> = None;
    let end = loop {
        let deadline = outstanding.map(|(_, sent)| sent + timing.ping_timeout);
        tokio::select! {
            () = cancel.cancelled() => break SessionEnd::Closed,
            () = mux.closed() => break lost(&mux).await,
            message = wire::recv::<_, BoxControl>(&mut control) => match message {
                Ok(Some((BoxControl::Snapshot { snapshot }, _))) => registry.replace(snapshot),
                Ok(Some((BoxControl::Event { event }, _))) => registry.apply(event),
                Ok(Some((BoxControl::Pong { nonce: answered }, _))) => {
                    if outstanding.is_some_and(|(sent, _)| sent == answered) {
                        outstanding = None;
                    }
                }
                Ok(Some(_)) => break SessionEnd::Failed("unexpected control message".into()),
                Ok(None) => break lost(&mux).await,
                Err(error) => break match mux.failure_within(FAILURE_GRACE).await {
                    Some(reason) => SessionEnd::Failed(reason),
                    None => SessionEnd::Failed(error.to_string()),
                },
            },
            _ = ticker.tick(), if outstanding.is_none() => {
                nonce += 1;
                let sent = Instant::now();
                let ping = ClientControl::Ping { nonce };
                // A stalled write counts against the same budget as a missing pong.
                match timeout(timing.ping_timeout, wire::send(&mut control, &ping)).await {
                    Ok(Ok(())) => outstanding = Some((nonce, sent)),
                    Ok(Err(error)) => break SessionEnd::Failed(error.to_string()),
                    Err(_) => break SessionEnd::PingTimeout,
                }
            }
            () = sleep_until_opt(deadline) => break SessionEnd::PingTimeout,
        }
    };
    tracing::debug!("session ended: {end}");
    end_tx.send_replace(Some(end));
    let _ = timeout(Duration::from_millis(200), control.close()).await;
    mux.shutdown();
}

/// Sleeps until `deadline`, or forever when there is none.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::registry::{RegistrySnapshot, SharedRegistry};
    use crate::server::{BoxServices, LoopbackDialer, ServeError, ServeOptions, serve};
    use crate::sync::NoSync;

    struct NoRpc;

    #[async_trait::async_trait]
    impl crate::rpc::RpcHandler for NoRpc {
        async fn call(
            &self,
            _request: Value,
            _input: crate::rpc::RpcInput,
        ) -> Result<crate::rpc::RpcReplyStream, crate::rpc::RpcError> {
            Err(crate::rpc::RpcError::new("unused"))
        }
    }

    fn services() -> BoxServices {
        BoxServices {
            registry: Arc::new(SharedRegistry::new(RegistrySnapshot::default(), 4)),
            rpc: Arc::new(NoRpc),
            dialer: Arc::new(LoopbackDialer),
            sync: Arc::new(NoSync),
        }
    }

    #[tokio::test]
    async fn box_refuses_other_protocol_revisions() -> Result<(), Box<dyn std::error::Error>> {
        timeout(Duration::from_secs(5), async {
            let (client_io, box_io) = tokio::io::duplex(64 * 1024);
            let server = tokio::spawn(serve(box_io, services(), ServeOptions::new("box 9")));
            let mux = mux::client(Box::new(client_io));
            let mut control = wire::channel(mux.open().await?);
            let hello = OpenRequest::Control {
                protocol: PROTOCOL_VERSION + 1,
                client: "future".into(),
            };
            wire::send(&mut control, &hello).await?;
            let reply = wire::recv::<_, BoxControl>(&mut control).await?;
            assert!(matches!(
                reply,
                Some((
                    BoxControl::Incompatible {
                        protocol: PROTOCOL_VERSION,
                        ..
                    },
                    _
                ))
            ));
            let result = server.await?;
            assert!(matches!(result, Err(ServeError::IncompatibleClient { .. })));
            Ok(())
        })
        .await?
    }

    /// A pipe whose reads fail like an SSH tunnel that exited with an error.
    struct DeadTunnel;

    impl tokio::io::AsyncRead for DeadTunnel {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "remote `devlanes tunnel --stdio` exited with 2: error: not_supported_yet",
            )))
        }
    }

    impl tokio::io::AsyncWrite for DeadTunnel {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn handshake_surfaces_the_pipe_error() -> Result<(), Box<dyn std::error::Error>> {
        timeout(Duration::from_secs(5), async {
            let result = Session::establish(Box::new(DeadTunnel), SessionOptions::new("c")).await;
            let Err(error) = result else {
                panic!("handshake over a dead pipe succeeded");
            };
            assert!(matches!(error, SessionError::Closed(_)), "{error:?}");
            assert!(error.to_string().contains("not_supported_yet"), "{error}");
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn client_reports_incompatible_box_as_permanent() -> Result<(), Box<dyn std::error::Error>>
    {
        timeout(Duration::from_secs(5), async {
            let (client_io, box_io) = tokio::io::duplex(64 * 1024);
            let fake_box = tokio::spawn(async move {
                let mut mux = mux::server(Box::new(box_io));
                let stream = mux.accept().await.ok_or("no control stream")?;
                let mut control = wire::channel(stream);
                let _hello = wire::recv::<_, OpenRequest>(&mut control).await?;
                let reply = BoxControl::Welcome {
                    protocol: PROTOCOL_VERSION + 1,
                    server: "devlanes 9.0.0".into(),
                };
                wire::send(&mut control, &reply).await?;
                // Hold the stream until the client gives up.
                let _ = wire::recv::<_, Value>(&mut control).await;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
            let result =
                Session::establish(Box::new(client_io), SessionOptions::new("client")).await;
            let Err(error) = result else {
                panic!("handshake with an incompatible box succeeded");
            };
            assert!(error.is_permanent());
            assert!(error.to_string().contains("never upgraded automatically"));
            fake_box.abort();
            Ok(())
        })
        .await?
    }
}
