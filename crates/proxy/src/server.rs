use std::{
    convert::Infallible,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, PoisonError},
    time::Duration,
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use devlanes_pki::{
    CertificateAuthority, CertificateIdentity, CertificatePolicy, DynamicCertResolver,
};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode, Uri,
    body::Incoming,
    header::{self, HeaderMap, HeaderValue},
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
    HostHeader, RouteState, RouteTable, Upstream,
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
}

impl Default for ProxyLimits {
    /// Allows browser and dev-server bursts while bounding idle and slow clients.
    fn default() -> Self {
        Self {
            max_connections: 512,
            header_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
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
}

impl Default for ProxyConfig {
    /// Starts in loopback-only plain HTTP mode.
    fn default() -> Self {
        Self {
            lan: false,
            connect_timeout: Duration::from_secs(5),
            https_redirect_port: None,
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
}

impl CertificatePolicy for Routes {
    /// Certificate SANs and authorization are taken from the same route generation.
    fn lookup(&self, server_name: &str) -> Option<CertificateIdentity> {
        let snapshot = self.snapshot.load();
        let route = snapshot.table.lookup(server_name)?;
        let mut names = route.certificate_names.clone();
        names.push(server_name.to_string());
        if let Some(parent) = route.host.strip_prefix("*.") {
            if parent.contains('.') {
                names.push(route.host.clone());
            }
            names.push(parent.to_string());
        }
        names.sort();
        names.dedup();
        Some(CertificateIdentity {
            generation: snapshot.generation,
            names,
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
        let Some(authority) = request_authority(&request) else {
            return Ok(page(
                StatusCode::BAD_REQUEST,
                "Invalid host",
                "Supply one valid Host or :authority.",
                "",
            ));
        };
        let host = authority.host().trim_end_matches('.').to_ascii_lowercase();
        let route = self.routes.snapshot.load().table.lookup(&host).cloned();
        let Some(route) = route else {
            return Ok(page(
                StatusCode::NOT_FOUND,
                "Unknown host",
                "No devlanes route is registered for this host.",
                "",
            ));
        };
        if request.method() == hyper::Method::CONNECT {
            return Ok(page(
                StatusCode::METHOD_NOT_ALLOWED,
                "CONNECT unsupported",
                "Use an HTTP/1.1 WebSocket upgrade.",
                "",
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
            let location = format!("https://{host}{suffix}{path}");
            let mut response = page(
                StatusCode::PERMANENT_REDIRECT,
                "HTTPS required",
                "Use the HTTPS URL for this service.",
                "",
            );
            if let Ok(value) = HeaderValue::from_str(&location) {
                response.headers_mut().insert(header::LOCATION, value);
            }
            return Ok(response);
        }
        let marker = route.host.as_str();
        if request
            .headers()
            .get_all("x-devlanes-via")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|value| value.trim() == marker)
        {
            return Ok(page(
                StatusCode::LOOP_DETECTED,
                "Proxy loop",
                "An upstream sent this route back to devlanes. Check its proxy target and Host rewrite.",
                "",
            ));
        }
        match &route.state {
            RouteState::Idle => {
                return Ok(page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Lane idle",
                    &route.start_hint,
                    "",
                ));
            }
            RouteState::Starting => {
                return Ok(page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service starting",
                    "Wait for readiness, then reload.",
                    "",
                ));
            }
            RouteState::Failed { log_tail } => {
                return Ok(page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service failed",
                    &route.start_hint,
                    logs_for_peer(log_tail, peer.ip()),
                ));
            }
            RouteState::Reconnecting => {
                return Ok(page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Reconnecting",
                    "The connection to the box running this service dropped; devlanes is reconnecting. Reload in a moment.",
                    "",
                ));
            }
            RouteState::Ready => {}
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
        if let Ok(value) = HeaderValue::from_str(marker) {
            request.headers_mut().append("x-devlanes-via", value);
        }
        if route.host_header == HostHeader::Preserve
            && let Ok(value) = HeaderValue::from_str(authority.as_str())
        {
            request.headers_mut().insert(header::HOST, value);
        }
        // HTTP/2 pseudo-headers and absolute-form requests become origin-form upstream.
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        match path.parse::<Uri>() {
            Ok(uri) => *request.uri_mut() = uri,
            Err(_) => {
                return Ok(page(
                    StatusCode::BAD_REQUEST,
                    "Invalid URI",
                    "Use a valid request path.",
                    "",
                ));
            }
        }
        *request.version_mut() = hyper::Version::HTTP_11;
        let target = route.target();
        let unavailable = || {
            page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Service unavailable",
                &route.start_hint,
                "The upstream is not accepting connections.",
            )
        };
        let failed = || {
            page(
                StatusCode::BAD_GATEWAY,
                "Upstream request failed",
                &route.start_hint,
                "",
            )
        };
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
                            match self.connect(target, shutdown.clone()).await {
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
            None => match self.connect(target, shutdown.clone()).await {
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
                return Ok(page(
                    StatusCode::BAD_GATEWAY,
                    "Unexpected upstream upgrade",
                    "The upstream upgraded an ordinary HTTP request.",
                    "",
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

/// Parses HTTP authorities without accepting duplicate or conflicting Host values.
fn request_authority(request: &Request<Incoming>) -> Option<hyper::http::uri::Authority> {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next();
    if hosts.next().is_some() {
        return None;
    }
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
    let authority = match (header, request.uri().authority()) {
        (Some(header), Some(uri)) if !header.as_str().eq_ignore_ascii_case(uri.as_str()) => None,
        (Some(header), _) => Some(header),
        (None, Some(uri)) => Some(uri.clone()),
        _ => None,
    }?;
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
        match tokio::time::timeout(timeout, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last = error,
            Err(_) => last = io::Error::new(io::ErrorKind::TimedOut, "upstream dial timed out"),
        }
    }
    Err(last)
}

/// Escapes daemon-provided status and logs before embedding them in HTML.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Status responses are local and non-cacheable; ordinary upstream bodies stay streaming.
fn page(status: StatusCode, title: &str, hint: &str, logs: &str) -> Response<Body> {
    let html = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><body><h1>{}</h1><p>{}</p><pre>{}</pre></body></html>",
        escape(title),
        escape(title),
        escape(hint),
        escape(logs)
    );
    let mut response = Response::new(
        Full::new(Bytes::from(html))
            .map_err(|never| -> BoxError { match never {} })
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
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
