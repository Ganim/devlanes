use std::{
    convert::Infallible,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, PoisonError},
    time::Duration,
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use devlanes_pages::{ErrorCode, ErrorPage, Format, KnownHost};
use devlanes_pki::{
    CertificateAuthority, CertificateIdentity, CertificatePolicy, DynamicCertResolver,
};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode, Uri,
    body::Incoming,
    header::{self, HeaderMap, HeaderValue},
    http::uri::PathAndQuery,
    service::service_fn,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{
    HostHeader, Route, RouteState, RouteTable, Upstream,
    activity::{ActiveBody, Activity},
    health::{Health, ListenerGuard, ListenerHealth},
    pool::{Connection, Pool},
};

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type Body = UnsyncBoxBody<Bytes, BoxError>;

/// Each HTTP/2 stream needs its own HTTP/1.1 upstream connection, so this bounds those per client.
pub(crate) const MAX_H2_STREAMS: u32 = 32;

/// Resource bounds shared by all listeners of one proxy instance.
#[derive(Clone, Debug)]
pub struct ProxyLimits {
    /// Client connections served at once across all listeners, WebSockets included.
    /// Further clients wait in the kernel backlog.
    pub max_connections: usize,
    /// Time allowed for a complete HTTP/1 request head, including between keep-alive requests.
    pub header_timeout: Duration,
    /// A connection with no request or response body in flight is closed after this long.
    pub idle_timeout: Duration,
    /// A request to a starting service waits this long for it to become ready before
    /// getting an error page.
    pub start_wait: Duration,
    /// A request to a box route waits this long for the box to reconnect.
    pub reconnect_wait: Duration,
    /// Refused connections to a ready service are retried for this long, which covers
    /// dev servers that a file watcher restarts.
    pub refused_retry: Duration,
}

impl Default for ProxyLimits {
    /// Allows browser and dev-server bursts while bounding idle and slow clients.
    fn default() -> Self {
        Self {
            max_connections: 512,
            header_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            start_wait: Duration::from_secs(30),
            reconnect_wait: Duration::from_secs(15),
            refused_retry: Duration::from_secs(5),
        }
    }
}

/// Peer policy and timeouts apply independently to every provided listener.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub lan: bool,
    /// Applies only to dialing; established SSE and WebSocket streams have no idle timeout.
    pub connect_timeout: Duration,
    /// Set only when a TLS listener is running. Plain HTTP then redirects known routes.
    pub https_redirect_port: Option<u16>,
    /// Adds `Via: 1.1 devlanes.run` to forwarded responses.
    pub via: bool,
}

impl Default for ProxyConfig {
    /// Starts in loopback-only plain HTTP mode.
    fn default() -> Self {
        Self {
            lan: false,
            connect_timeout: Duration::from_secs(5),
            https_redirect_port: None,
            via: true,
        }
    }
}

#[derive(Debug)]
struct Snapshot {
    /// Changes only when the set of certificate names changes, so state updates keep cached leaves.
    generation: u64,
    table: RouteTable,
}

#[derive(Debug)]
struct Routes {
    snapshot: ArcSwap<Snapshot>,
    /// Bumped on every published snapshot, so requests waiting on a route's state wake up.
    changes: watch::Sender<u64>,
}

impl CertificatePolicy for Routes {
    /// Certificate SANs and authorization are taken from the same route generation.
    /// The requested name comes first so it becomes the leaf's common name.
    fn lookup(&self, server_name: &str) -> Option<CertificateIdentity> {
        let snapshot = self.snapshot.load();
        let route = snapshot.table.lookup(server_name)?;
        let mut names = route.certificate_names.clone();
        if let Some(parent) = route.host.strip_prefix("*.") {
            if parent.contains('.') {
                names.push(route.host.clone());
            }
            names.push(parent.to_string());
        }
        names.retain(|name| name != server_name);
        names.sort();
        names.dedup();
        names.insert(0, server_name.to_string());
        Some(CertificateIdentity {
            generation: snapshot.generation,
            names,
            owner: route.owner.clone(),
        })
    }
}

/// Cloneable runtime handle; all listeners share one atomic route snapshot.
#[derive(Clone, Debug)]
pub struct Proxy {
    config: ProxyConfig,
    routes: Arc<Routes>,
    limits: ProxyLimits,
    connections: Arc<Semaphore>,
    health: Health,
    pool: Arc<Pool>,
}

impl Proxy {
    /// Does not bind sockets or start background tasks.
    pub fn new(config: ProxyConfig, routes: RouteTable) -> Self {
        Self {
            config,
            routes: Arc::new(Routes {
                snapshot: ArcSwap::from_pointee(Snapshot {
                    generation: 0,
                    table: routes,
                }),
                changes: watch::channel(0).0,
            }),
            limits: ProxyLimits::default(),
            connections: Arc::new(Semaphore::new(ProxyLimits::default().max_connections)),
            health: Health::default(),
            pool: Arc::new(Pool::default()),
        }
    }

    /// Publishes a complete snapshot without interrupting in-flight streams.
    pub fn replace_routes(&self, routes: RouteTable) {
        let names = certificate_names(&routes);
        self.routes.snapshot.rcu(|old| Snapshot {
            generation: old.generation + u64::from(certificate_names(&old.table) != names),
            table: routes.clone(),
        });
        self.routes
            .changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// The route for a normalized hostname in the current snapshot.
    fn lookup(&self, host: &str) -> Option<Route> {
        self.routes.snapshot.load().table.lookup(host).cloned()
    }

    /// Replaces the default limits; call before cloning the proxy or serving any listener.
    pub fn with_limits(mut self, limits: ProxyLimits) -> Self {
        self.connections = Arc::new(Semaphore::new(limits.max_connections.max(1)));
        self.limits = limits;
        self
    }

    /// Reports every listener served so far, sorted by address, including stopped ones.
    pub fn listener_health(&self) -> Vec<ListenerHealth> {
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Returns a detached snapshot for host listings and diagnostics.
    pub fn routes(&self) -> RouteTable {
        self.routes.snapshot.load().table.clone()
    }

    /// Shares live route authorization with the TLS certificate resolver.
    pub fn tls_config(
        &self,
        ca: Arc<CertificateAuthority>,
    ) -> Result<Arc<rustls::ServerConfig>, devlanes_pki::Error> {
        Arc::new(DynamicCertResolver::new(ca, self.routes.clone())).server_config()
    }

    /// Serves HTTP/1 and HTTP/2 until cancellation. The caller owns binding/fallback policy.
    pub async fn serve(
        &self,
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        self.serve_listener(listener, None, shutdown).await
    }

    /// Serves TLS with h1/h2 ALPN; extended CONNECT is intentionally not enabled.
    pub async fn serve_tls(
        &self,
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        self.serve_listener(
            listener,
            Some(tokio_rustls::TlsAcceptor::from(tls)),
            shutdown,
        )
        .await
    }

    /// All accepted peers, including IPv4-mapped IPv6 peers, pass the same LAN policy.
    ///
    /// Accept errors never end the loop: only cancellation does.
    async fn serve_listener(
        &self,
        listener: TcpListener,
        tls: Option<tokio_rustls::TlsAcceptor>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let shutdown = shutdown.child_token();
        let _shutdown_guard = shutdown.clone().drop_guard();
        let local = listener.local_addr()?;
        let mut health = ListenerGuard::new(self.health.clone(), local, tls.is_some());
        let mut connections = JoinSet::new();
        loop {
            // Taking the permit first leaves excess clients in the kernel backlog.
            let permit = tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => continue,
                permit = self.connections.clone().acquire_owned() => match permit {
                    Ok(permit) => Arc::new(permit),
                    Err(_) => break,
                },
            };
            let accepted = tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, peer) = match accepted {
                Ok(accepted) => {
                    health.recovered();
                    accepted
                }
                Err(error) => {
                    health.failed(&error);
                    if peer_error(&error) {
                        // A client reset before accept (ECONNABORTED) concerns that client only.
                        tracing::debug!(%local, %error, "proxy accept failed");
                        continue;
                    }
                    // EMFILE/ENFILE and other resource errors persist; pause instead of spinning.
                    tracing::warn!(%local, %error, "proxy accept failed, retrying");
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(100)) => continue,
                    }
                }
            };
            if !self.config.lan && !is_loopback(peer.ip()) {
                continue;
            }
            let proxy = self.clone();
            let tls = tls.clone();
            let shutdown = shutdown.clone();
            connections.spawn(async move {
                let Some(tls) = tls else {
                    proxy
                        .serve_connection(stream, peer, local, false, shutdown, permit)
                        .await;
                    return;
                };
                match tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await {
                    Ok(Ok(stream)) => {
                        proxy
                            .serve_connection(stream, peer, local, true, shutdown, permit)
                            .await;
                    }
                    Ok(Err(error)) => tracing::debug!(%error, "TLS handshake refused"),
                    Err(_) => tracing::debug!("TLS handshake timed out"),
                }
            });
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }

    /// Keeps streaming bodies and upgrades attached to the listener's shutdown lifetime.
    ///
    /// `permit` is held until the connection and any WebSocket bridged from it end.
    async fn serve_connection<I>(
        &self,
        stream: I,
        peer: SocketAddr,
        local: SocketAddr,
        secure: bool,
        shutdown: CancellationToken,
        permit: Arc<OwnedSemaphorePermit>,
    ) where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let proxy = self.clone();
        let (activity, mut active) = watch::channel(0usize);
        let service = service_fn(move |request| {
            let proxy = proxy.clone();
            let shutdown = shutdown.clone();
            let permit = permit.clone();
            let activity = Activity::new(activity.clone());
            async move {
                let response = proxy
                    .handle(request, peer, local, secure, shutdown, permit)
                    .await?;
                Ok::<_, Infallible>(
                    response.map(|body| ActiveBody::new(body, activity).boxed_unsync()),
                )
            }
        });
        let mut builder = Builder::new(TokioExecutor::new());
        // Hyper restarts this timer while waiting for each keep-alive request, so it also
        // bounds idle HTTP/1 connections.
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(self.limits.header_timeout);
        builder
            .http2()
            .timer(TokioTimer::new())
            .max_concurrent_streams(MAX_H2_STREAMS)
            .keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10));
        let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
        tokio::pin!(connection);
        // HTTP/2 has no server idle timeout in Hyper, and pings keep idle browsers alive,
        // so idleness is measured from requests and response bodies instead.
        let idle = tokio::time::sleep(self.limits.idle_timeout);
        tokio::pin!(idle);
        let mut closing = false;
        loop {
            tokio::select! {
                result = &mut connection => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "proxy connection ended");
                    }
                    break;
                }
                Ok(()) = active.changed() => {
                    if *active.borrow_and_update() == 0 && !closing {
                        idle.as_mut().reset(tokio::time::Instant::now() + self.limits.idle_timeout);
                    }
                }
                _ = &mut idle, if closing || *active.borrow() == 0 => {
                    if closing {
                        break;
                    }
                    // GOAWAY lets HTTP/2 clients retry elsewhere; give it a short grace period.
                    connection.as_mut().graceful_shutdown();
                    closing = true;
                    idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(5));
                }
            }
        }
    }

    /// Validates authority before routing and forwards bodies without collecting them.
    async fn handle(
        &self,
        mut request: Request<Incoming>,
        peer: SocketAddr,
        local: SocketAddr,
        secure: bool,
        shutdown: CancellationToken,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> Result<Response<Body>, Infallible> {
        let format = Format::negotiate(accept(request.headers()).as_deref());
        let Some(authority) = request_authority(&request) else {
            return Ok(error_response(
                &ErrorPage::new(ErrorCode::InvalidHost),
                format,
            ));
        };
        let host = authority.host().trim_end_matches('.').to_ascii_lowercase();
        let Some(mut route) = self.lookup(&host) else {
            return Ok(self.unknown_host(&host, peer.ip(), format));
        };
        if request.method() == hyper::Method::CONNECT {
            return Ok(error_response(
                &route_page(&route, ErrorCode::ConnectUnsupported),
                format,
            ));
        }
        if !secure && let Some(port) = self.config.https_redirect_port {
            let suffix = if port == 443 {
                String::new()
            } else {
                format!(":{port}")
            };
            let path = request
                .uri()
                .path_and_query()
                .map_or("/", |path| path.as_str());
            return Ok(redirect(&format!("https://{host}{suffix}{path}")));
        }
        if request
            .headers()
            .get_all("x-devlanes-via")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|value| value.trim() == route.host)
        {
            return Ok(error_response(
                &route_page(&route, ErrorCode::ProxyLoop),
                format,
            ));
        }
        if matches!(route.state, RouteState::Starting | RouteState::Reconnecting) {
            match self.settle(&host, &shutdown).await {
                Some(settled) => route = settled,
                None => return Ok(self.unknown_host(&host, peer.ip(), format)),
            }
        }
        let code = match &route.state {
            RouteState::Ready => None,
            RouteState::Idle => Some(ErrorCode::ServiceIdle),
            RouteState::Starting => Some(ErrorCode::ServiceStarting),
            RouteState::Failed { .. } => Some(ErrorCode::ServiceFailed),
            RouteState::Reconnecting => Some(ErrorCode::BoxReconnecting),
        };
        if let Some(code) = code {
            let mut page = route_page(&route, code);
            if code != ErrorCode::BoxReconnecting {
                page = page.hint(&route.start_hint);
            }
            if let RouteState::Failed { log_tail } = &route.state {
                page = page.logs(logs_for_peer(log_tail, peer.ip()));
            }
            return Ok(error_response(&page, format));
        }
        let upgrading = header_has_token(request.headers(), header::CONNECTION, "upgrade")
            && request.headers().contains_key(header::UPGRADE);
        let downstream_upgrade = upgrading.then(|| hyper::upgrade::on(&mut request));
        strip_hop_headers(request.headers_mut(), upgrading);
        strip_client_metadata(request.headers_mut());
        if request.version() == hyper::Version::HTTP_2 {
            join_cookies(request.headers_mut());
        }
        // Forwarded metadata is replaced at this trust boundary, never trusted from clients.
        let proto = if secure { "https" } else { "http" };
        let public_port = authority.port_u16().unwrap_or(local.port());
        let forwarded_for = match peer.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        let forwarded = format!("for=\"{forwarded_for}\";host=\"{authority}\";proto={proto}");
        for (name, value) in [
            ("x-forwarded-proto", proto.to_string()),
            ("x-forwarded-host", authority.to_string()),
            ("x-forwarded-for", peer.ip().to_string()),
            ("x-forwarded-port", public_port.to_string()),
            ("forwarded", forwarded),
        ] {
            if let Ok(value) = HeaderValue::from_str(&value) {
                request.headers_mut().insert(name, value);
            }
        }
        if let Ok(value) = HeaderValue::from_str(&route.host) {
            request.headers_mut().append("x-devlanes-via", value);
        }
        if route.host_header == HostHeader::Preserve
            && let Ok(value) = HeaderValue::from_str(authority.as_str())
        {
            request.headers_mut().insert(header::HOST, value);
        }
        // HTTP/2 pseudo-headers and absolute-form requests become origin-form upstream.
        // The path was validated when the request was parsed, so it is reused as is.
        let mut parts = hyper::http::uri::Parts::default();
        parts.path_and_query = Some(
            request
                .uri()
                .path_and_query()
                .filter(|path| !path.as_str().is_empty())
                .cloned()
                .unwrap_or_else(|| PathAndQuery::from_static("/")),
        );
        // A URI with only a path cannot fail to build.
        if let Ok(uri) = Uri::from_parts(parts) {
            *request.uri_mut() = uri;
        }
        *request.version_mut() = hyper::Version::HTTP_11;
        let target = route.target();
        let unavailable =
            || error_response(&route_page(&route, ErrorCode::UpstreamUnreachable), format);
        let failed = || error_response(&route_page(&route, ErrorCode::UpstreamFailed), format);
        // Upgrades dial their own connection, which is never pooled after a 101.
        let pooled = if upgrading {
            None
        } else {
            self.pool.take(target)
        };
        let (connection, sent) = match pooled {
            Some(mut connection) => {
                set_upstream_host(&mut request, route.host_header, &connection);
                match connection.sender.try_send_request(request).await {
                    Ok(response) => (connection, Ok(response)),
                    // The upstream closed the idle socket before the request was written,
                    // so it is safe to send once more on a fresh connection.
                    Err(mut error) => match error.take_message() {
                        Some(unsent) => {
                            request = unsent;
                            match self.connect_retrying(&host, target, &shutdown).await {
                                Ok(mut fresh) => {
                                    set_upstream_host(&mut request, route.host_header, &fresh);
                                    let sent = fresh.sender.send_request(request).await;
                                    (fresh, sent)
                                }
                                Err(error) => {
                                    tracing::debug!(%error, host = %route.host, "upstream dial failed");
                                    return Ok(unavailable());
                                }
                            }
                        }
                        None => {
                            tracing::debug!(error = %error.error(), host = %route.host, "upstream request failed");
                            return Ok(failed());
                        }
                    },
                }
            }
            None => match self.connect_retrying(&host, target, &shutdown).await {
                Ok(mut connection) => {
                    set_upstream_host(&mut request, route.host_header, &connection);
                    let sent = connection.sender.send_request(request).await;
                    (connection, sent)
                }
                Err(error) => {
                    tracing::debug!(%error, host = %route.host, "upstream dial failed");
                    return Ok(unavailable());
                }
            },
        };
        let mut response = match sent {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, host = %route.host, "upstream request failed");
                return Ok(failed());
            }
        };
        let switched = response.status() == StatusCode::SWITCHING_PROTOCOLS;
        if !switched {
            self.pool
                .recycle(target.clone(), connection, shutdown.clone());
        }
        if switched {
            let Some(downstream) = downstream_upgrade else {
                return Ok(error_response(
                    &route_page(&route, ErrorCode::UnexpectedUpgrade),
                    format,
                ));
            };
            let upstream = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                let _permit = permit;
                tokio::select! {
                    _ = shutdown.cancelled() => {},
                    _ = async {
                        if let (Ok(downstream), Ok(upstream)) = tokio::join!(downstream, upstream) {
                            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(downstream), &mut TokioIo::new(upstream)).await;
                        }
                    } => {},
                }
            });
        }
        strip_hop_headers(response.headers_mut(), switched);
        if self.config.via {
            // Appended after the upstream's own entries, as RFC 9110 section 7.6.3 orders them.
            let via = if route.remote {
                "1.1 devlanes.run (remote)"
            } else {
                "1.1 devlanes.run"
            };
            response
                .headers_mut()
                .append(header::VIA, HeaderValue::from_static(via));
        }
        Ok(response.map(|body| {
            body.map_err(|error| -> BoxError { Box::new(error) })
                .boxed_unsync()
        }))
    }

    /// Dials and handshakes a new HTTP/1.1 connection whose driver stops with the listener.
    async fn connect(
        &self,
        target: &Upstream,
        shutdown: CancellationToken,
    ) -> Result<Connection, BoxError> {
        let stream = dial(target, self.config.connect_timeout).await?;
        // Local targets may answer on IPv6 only, so the Host rewrite names the address that answered.
        let authority = if matches!(target, Upstream::Local(_)) {
            stream.peer_addr()?.to_string()
        } else {
            target.authority()
        };
        let (sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {},
                result = connection.with_upgrades() => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "upstream connection ended");
                    }
                }
            }
        });
        Ok(Connection { sender, authority })
    }

    /// Dials like [`Self::connect`] and retries refused connections while the route stays
    /// ready, for up to `refused_retry`. A refused connection carried no request, so a
    /// retry cannot send anything twice.
    async fn connect_retrying(
        &self,
        host: &str,
        target: &Upstream,
        shutdown: &CancellationToken,
    ) -> Result<Connection, BoxError> {
        let deadline = tokio::time::Instant::now() + self.limits.refused_retry;
        let mut delay = Duration::from_millis(50);
        loop {
            let error = match self.connect(target, shutdown.clone()).await {
                Ok(connection) => return Ok(connection),
                Err(error) => error,
            };
            let refused = error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::ConnectionRefused);
            let ready = self
                .lookup(host)
                .is_some_and(|route| route.state == RouteState::Ready && route.target() == target);
            if !refused || !ready || tokio::time::Instant::now() + delay > deadline {
                return Err(error);
            }
            tokio::select! {
                _ = shutdown.cancelled() => return Err(error),
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(1));
        }
    }

    /// Waits while the route for `host` is starting or its box is reconnecting, up to
    /// `start_wait` or `reconnect_wait` from now, and returns its latest state. `None`
    /// means the route disappeared meanwhile.
    async fn settle(&self, host: &str, shutdown: &CancellationToken) -> Option<Route> {
        // Subscribing before the lookup means no snapshot published in between is missed.
        let mut changes = self.routes.changes.subscribe();
        let started = tokio::time::Instant::now();
        loop {
            let route = self.lookup(host)?;
            let limit = match route.state {
                RouteState::Starting => self.limits.start_wait,
                RouteState::Reconnecting => self.limits.reconnect_wait,
                _ => return Some(route),
            };
            let Some(remaining) = limit.checked_sub(started.elapsed()) else {
                return Some(route);
            };
            tokio::select! {
                _ = shutdown.cancelled() => return Some(route),
                changed = tokio::time::timeout(remaining, changes.changed()) => {
                    if !matches!(changed, Ok(Ok(()))) {
                        return Some(route);
                    }
                }
            }
        }
    }

    /// The unknown-host page. Clients on this machine also get the routed hostnames to
    /// pick from; LAN peers do not, since hostnames reveal projects and branches.
    ///
    /// The list also needs a host no website can be the origin of: `localhost`, an IP or a
    /// `.localhost` name. A DNS-rebinding page arrives from loopback too, with its own
    /// domain as Host, and could otherwise read the list same-origin.
    fn unknown_host(&self, host: &str, peer: IpAddr, format: Format) -> Response<Body> {
        let snapshot = self.routes.snapshot.load();
        let bare = host == "localhost" || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok();
        let known = if is_loopback(peer) && (bare || host.ends_with(".localhost")) {
            snapshot
                .table
                .routes()
                .iter()
                .map(|route| KnownHost {
                    host: &route.host,
                    owner: route.owner.as_deref(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let hint = if bare {
            "localhost and IP addresses reach the proxy, not a service. Use the service's hostname."
        } else {
            ""
        };
        error_response(
            &ErrorPage::new(ErrorCode::UnknownHost)
                .host(host)
                .hint(hint)
                .known_hosts(known),
            format,
        )
    }
}

/// An error page with the route's unit, so its commands name it.
fn route_page(route: &Route, code: ErrorCode) -> ErrorPage<'_> {
    ErrorPage::new(code)
        .unit(route.owner.as_deref(), route.unit.as_deref())
        .remote(route.remote)
}

/// Applies `HostHeader::Upstream` once the connection, and so the answering address, is known.
fn set_upstream_host(request: &mut Request<Incoming>, policy: HostHeader, connection: &Connection) {
    if policy == HostHeader::Upstream
        && let Ok(value) = HeaderValue::from_str(&connection.authority)
    {
        request.headers_mut().insert(header::HOST, value);
    }
}

/// Errors caused by one client's connection, which say nothing about the listener's health.
fn peer_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

/// State, upstream and insertion-order changes do not alter certificate authorization.
fn certificate_names(table: &RouteTable) -> Vec<String> {
    let mut names: Vec<_> = table
        .routes()
        .iter()
        .flat_map(|route| std::iter::once(&route.host).chain(&route.certificate_names))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Logs can contain secrets even when the route is deliberately exposed to the LAN.
fn logs_for_peer(logs: &str, peer: IpAddr) -> &str {
    if is_loopback(peer) { logs } else { "" }
}

/// Removes alternate proxy metadata that frameworks may trust over X-Forwarded-For.
fn strip_client_metadata(headers: &mut HeaderMap) {
    for name in [
        "x-real-ip",
        "x-forwarded-ssl",
        "x-forwarded-scheme",
        "x-forwarded-prefix",
        "x-forwarded-server",
        "x-original-url",
        "front-end-https",
    ] {
        headers.remove(name);
    }
}

/// RFC 9113 section 8.2.3: HTTP/2 may split `cookie`, but HTTP/1.1 needs one "; "-joined field.
fn join_cookies(headers: &mut HeaderMap) {
    let cookies: Vec<&[u8]> = headers
        .get_all(header::COOKIE)
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    if cookies.len() < 2 {
        return;
    }
    if let Ok(value) = HeaderValue::from_bytes(&cookies.join(b"; ".as_slice())) {
        headers.insert(header::COOKIE, value);
    }
}

/// Accepts all 127/8 addresses, ::1, and their IPv4-mapped representation.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}

/// Parses HTTP authorities without accepting duplicate Host fields (RFC 9112 section 3.2).
///
/// An HTTP/1 absolute-form target wins over Host, which the proxy then regenerates
/// (RFC 9112 section 3.2.2). HTTP/2 `:authority` and Host must agree (RFC 9113 section 8.3.1).
fn request_authority(request: &Request<Incoming>) -> Option<hyper::http::uri::Authority> {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next();
    if hosts.next().is_some() {
        return None;
    }
    let uri = request.uri().authority();
    let authority = match uri {
        Some(uri) if request.version() != hyper::Version::HTTP_2 => uri.clone(),
        _ => {
            let header = match host {
                Some(value) => Some(
                    value
                        .to_str()
                        .ok()?
                        .parse::<hyper::http::uri::Authority>()
                        .ok()?,
                ),
                None => None,
            };
            match (header, uri) {
                (Some(header), Some(uri))
                    if !header.as_str().eq_ignore_ascii_case(uri.as_str()) =>
                {
                    return None;
                }
                (Some(header), _) => header,
                (None, Some(uri)) => uri.clone(),
                (None, None) => return None,
            }
        }
    };
    if authority.as_str().contains('@') {
        return None;
    }
    if let Some((_, port)) = authority.as_str().rsplit_once(':')
        && port.parse::<u16>().is_err()
    {
        return None;
    }
    Some(authority)
}

/// Connection-nominated headers are hop-by-hop even when their names are unfamiliar.
fn strip_hop_headers(headers: &mut HeaderMap, upgrade: bool) {
    let nominated: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    for name in nominated {
        if !(upgrade && name == "upgrade") {
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove(header::UPGRADE);
    }
}

/// Handles comma-delimited header tokens case-insensitively.
fn header_has_token(headers: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}

/// Retries only failed TCP connections, so POST/upload bodies are never replayed.
async fn dial(target: &Upstream, timeout: Duration) -> io::Result<TcpStream> {
    let addresses = match target {
        Upstream::Local(port) => vec![
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), *port),
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), *port),
        ],
        Upstream::Address(address) => vec![*address],
        Upstream::Host { hostname, port } => {
            let mut addresses: Vec<_> =
                tokio::time::timeout(timeout, tokio::net::lookup_host((hostname.as_str(), *port)))
                    .await??
                    .collect();
            addresses.sort_by_key(|address| address.is_ipv6());
            addresses
        }
    };
    let mut last = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "upstream resolved to no addresses",
    );
    for address in addresses {
        let error = match tokio::time::timeout(timeout, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => error,
            Err(_) => io::Error::new(io::ErrorKind::TimedOut, "upstream dial timed out"),
        };
        // A refused address means the service may be restarting, which callers retry, so
        // it outranks later failures such as a loopback without IPv6.
        if last.kind() != io::ErrorKind::ConnectionRefused {
            last = error;
        }
    }
    Err(last)
}

/// Every `Accept` field joined into one list, as RFC 9110 section 5.3 allows.
fn accept(headers: &HeaderMap) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(","))
}

/// A complete local response; upstream bodies stay streaming.
fn local_response(status: StatusCode, body: String) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| -> BoxError { match never {} })
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    // Pages describe the moment they were served; a cached one would outlive the state.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// An error page in the format the client asked for, named by `x-devlanes-error`.
fn error_response(page: &ErrorPage<'_>, format: Format) -> Response<Body> {
    let code = page.code();
    let status = StatusCode::from_u16(code.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = local_response(status, page.render(format));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    headers.insert(header::VARY, HeaderValue::from_static("accept"));
    headers.insert("x-devlanes-error", HeaderValue::from_static(code.as_str()));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(devlanes_pages::CONTENT_SECURITY_POLICY),
    );
    response
}

/// A plain 308 to the HTTPS URL. It is not cached, so turning HTTPS off later takes
/// effect at once.
fn redirect(location: &str) -> Response<Body> {
    let mut response = local_response(StatusCode::PERMANENT_REDIRECT, String::new());
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LAN peers see the failure page without the service's log tail.
    #[test]
    fn log_tail_only_for_loopback_peers() {
        for peer in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert_eq!(logs_for_peer("secret", peer.parse().expect("ip")), "secret");
        }
        for peer in ["192.168.1.20", "::ffff:10.0.0.2", "fe80::1"] {
            assert_eq!(logs_for_peer("secret", peer.parse().expect("ip")), "");
        }
    }

    /// Split HTTP/2 cookie fields become one HTTP/1.1 field, in order.
    #[test]
    fn cookies_are_joined() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1"));
        headers.append(header::COOKIE, HeaderValue::from_static("b=2; c=3"));
        join_cookies(&mut headers);
        let cookies: Vec<_> = headers.get_all(header::COOKIE).iter().collect();
        assert_eq!(cookies, [HeaderValue::from_static("a=1; b=2; c=3")]);
    }
}
