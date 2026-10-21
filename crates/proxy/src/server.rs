use std::{
    convert::Infallible,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{HostHeader, RouteState, RouteTable, Upstream};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;

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
    generation: u64,
    table: RouteTable,
}

#[derive(Debug)]
struct Routes {
    next_generation: AtomicU64,
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
}

impl Proxy {
    /// Does not bind sockets or start background tasks.
    pub fn new(config: ProxyConfig, routes: RouteTable) -> Self {
        Self {
            config,
            routes: Arc::new(Routes {
                next_generation: AtomicU64::new(1),
                snapshot: ArcSwap::from_pointee(Snapshot {
                    generation: 0,
                    table: routes,
                }),
            }),
        }
    }

    /// Publishes a complete snapshot without interrupting in-flight streams.
    pub fn replace_routes(&self, routes: RouteTable) {
        let generation = self.routes.next_generation.fetch_add(1, Ordering::Relaxed);
        self.routes.snapshot.store(Arc::new(Snapshot {
            generation,
            table: routes,
        }));
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
    async fn serve_listener(
        &self,
        listener: TcpListener,
        tls: Option<tokio_rustls::TlsAcceptor>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let shutdown = shutdown.child_token();
        let _shutdown_guard = shutdown.clone().drop_guard();
        let local = listener.local_addr()?;
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let (stream, peer) = accepted?;
                    if !self.config.lan && !is_loopback(peer.ip()) {
                        continue;
                    }
                    let proxy = self.clone();
                    let tls = tls.clone();
                    let shutdown = shutdown.clone();
                    connections.spawn(async move {
                        if let Some(tls) = tls {
                            match tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await {
                                Ok(Ok(stream)) => proxy.serve_connection(stream, peer, local, true, shutdown).await,
                                Ok(Err(error)) => tracing::debug!(%error, "TLS handshake refused"),
                                Err(_) => tracing::debug!("TLS handshake timed out"),
                            }
                        } else {
                            proxy.serve_connection(stream, peer, local, false, shutdown).await;
                        }
                    });
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }

    /// Keeps streaming bodies and upgrades attached to the listener's shutdown lifetime.
    async fn serve_connection<I>(
        &self,
        stream: I,
        peer: SocketAddr,
        local: SocketAddr,
        secure: bool,
        shutdown: CancellationToken,
    ) where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let proxy = self.clone();
        let service = service_fn(move |request| {
            let proxy = proxy.clone();
            let shutdown = shutdown.clone();
            async move { proxy.handle(request, peer, local, secure, shutdown).await }
        });
        let builder = Builder::new(TokioExecutor::new());
        let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
        if let Err(error) = connection.await {
            tracing::debug!(%error, "proxy connection ended");
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
                    log_tail,
                ));
            }
            RouteState::Ready => {}
        }
        let stream = match dial(route.target(), self.config.connect_timeout).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(%error, host = %route.host, "upstream dial failed");
                return Ok(page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service unavailable",
                    &route.start_hint,
                    "The upstream is not accepting connections.",
                ));
            }
        };
        let upgrading = header_has_token(request.headers(), header::CONNECTION, "upgrade")
            && request.headers().contains_key(header::UPGRADE);
        let downstream_upgrade = upgrading.then(|| hyper::upgrade::on(&mut request));
        strip_hop_headers(request.headers_mut(), upgrading);
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
        let upstream_authority = if route.host_header == HostHeader::Upstream {
            stream.peer_addr().map_or_else(
                |_| route.target().authority(),
                |addr| {
                    if matches!(route.target(), Upstream::Local(_)) {
                        addr.to_string()
                    } else {
                        route.target().authority()
                    }
                },
            )
        } else {
            authority.to_string()
        };
        if let Ok(value) = HeaderValue::from_str(&upstream_authority) {
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
        let (mut sender, connection) =
            match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
                Ok(pair) => pair,
                Err(_) => {
                    return Ok(page(
                        StatusCode::BAD_GATEWAY,
                        "Upstream handshake failed",
                        &route.start_hint,
                        "",
                    ));
                }
            };
        let connection_shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = connection_shutdown.cancelled() => {},
                result = connection.with_upgrades() => {
                    if let Err(error) = result { tracing::debug!(%error, "upstream connection ended"); }
                }
            }
        });
        let mut response = match sender.send_request(request).await {
            Ok(response) => response,
            Err(_) => {
                return Ok(page(
                    StatusCode::BAD_GATEWAY,
                    "Upstream request failed",
                    &route.start_hint,
                    "",
                ));
            }
        };
        let switched = response.status() == StatusCode::SWITCHING_PROTOCOLS;
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
