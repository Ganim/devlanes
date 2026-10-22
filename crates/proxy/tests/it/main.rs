use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use devlanes_pki::CertificateAuthority;
use devlanes_proxy::{
    HostHeader, Proxy, ProxyConfig, ProxyLimits, Route, RouteState, RouteTable, Upstream,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Empty, Full};
use hyper::{Request, body::Incoming};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Running {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for Running {
    /// Test failures close only test-owned listeners and connections.
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

/// Runs protocol fixtures in-process because the testing crate exposes its server only as a binary.
async fn fixture(ipv6: bool, websocket: bool) -> Result<Running> {
    let listener = TcpListener::bind(if ipv6 { "[::1]:0" } else { "127.0.0.1:0" }).await?;
    let address = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let task = tokio::spawn(async move {
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                Some(_) = clients.join_next(), if !clients.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break; };
                    clients.spawn(async move {
                        if websocket {
                            let mut websocket = tokio_tungstenite::accept_async(socket).await?;
                            while let Some(message) = websocket.next().await {
                                let message = message?;
                                if message.is_close() { break; }
                                websocket.send(message).await?;
                            }
                        } else {
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") {
                                request.push(socket.read_u8().await?);
                                if request.len() > 65536 { return Err("oversized fixture request".into()); }
                            }
                            if request.starts_with(b"GET /sse ") {
                                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n9\r\ndata: 1\n\n\r\n").await?;
                                // The body never ends. Receiving this frame proves the proxy does not collect it.
                                std::future::pending::<()>().await;
                            } else {
                                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", request.len());
                                socket.write_all(head.as_bytes()).await?;
                                socket.write_all(&request).await?;
                            }
                        }
                        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                    });
                }
            }
        }
    });
    Ok(Running {
        address,
        shutdown,
        task,
    })
}

/// Binds before returning so tests never race readiness against a sleep.
async fn run_proxy(proxy: Proxy, tls: Option<Arc<rustls::ServerConfig>>) -> Result<Running> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let task = tokio::spawn(async move {
        let result = if let Some(tls) = tls {
            proxy.serve_tls(listener, tls, stop).await
        } else {
            proxy.serve(listener, stop).await
        };
        assert!(result.is_ok(), "proxy listener failed: {result:?}");
    });
    Ok(Running {
        address,
        shutdown,
        task,
    })
}

/// Leaves the response body uncollected for streaming tests.
async fn request<I>(
    io: I,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<hyper::Response<Incoming>>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = Request::builder().uri(path).header("host", host);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    Ok(sender
        .send_request(request.body(Empty::<Bytes>::new())?)
        .await?)
}

/// Validates route precedence independently of insertion order and named-port choice.
#[test]
fn routing_precedence_and_validation() -> Result {
    let mut fallback = Route::new("*.lane.localhost", Upstream::Local(1));
    fallback.fallback = true;
    let mut exact = Route::new("APP.LANE.LOCALHOST.", Upstream::Local(2));
    exact
        .named_ports
        .insert("inspector".into(), Upstream::Local(3));
    exact.port = Some("inspector".into());
    let wildcard = Route::new("*.deep.lane.localhost", Upstream::Local(4));
    let table = RouteTable::new(vec![fallback, exact, wildcard])?;
    assert_eq!(
        table
            .lookup("app.lane.localhost")
            .ok_or("exact missing")?
            .target(),
        &Upstream::Local(3)
    );
    assert_eq!(
        table
            .lookup("x.deep.lane.localhost")
            .ok_or("wildcard missing")?
            .target(),
        &Upstream::Local(4)
    );
    assert_eq!(
        table
            .lookup("x.lane.localhost")
            .ok_or("fallback missing")?
            .target(),
        &Upstream::Local(1)
    );
    assert!(table.lookup("lane.localhost").is_none());
    assert!(table.lookup("evillane.localhost").is_none());
    // A fallback applies only when nothing else matches, even if it is exact.
    let mut exact_fallback = Route::new("api.lane.localhost", Upstream::Local(5));
    exact_fallback.fallback = true;
    let mut wildcard_fallback = Route::new("*.lane.localhost", Upstream::Local(6));
    wildcard_fallback.fallback = true;
    let primary = Route::new("*.lane.localhost", Upstream::Local(7));
    let table = RouteTable::new(vec![
        exact_fallback.clone(),
        wildcard_fallback.clone(),
        primary,
    ])?;
    assert_eq!(
        table
            .lookup("api.lane.localhost")
            .ok_or("route missing")?
            .target(),
        &Upstream::Local(7)
    );
    let table = RouteTable::new(vec![wildcard_fallback, exact_fallback])?;
    assert_eq!(
        table
            .lookup("api.lane.localhost")
            .ok_or("route missing")?
            .target(),
        &Upstream::Local(5)
    );
    let mut missing = Route::new("app.localhost", Upstream::Local(1));
    missing.port = Some("absent".into());
    assert!(RouteTable::new(vec![missing]).is_err());
    assert!(RouteTable::new(vec![Route::new("bad/host", Upstream::Local(1))]).is_err());
    assert!(
        RouteTable::new(vec![Route::new(
            "app.localhost",
            Upstream::Host {
                hostname: "user@localhost".into(),
                port: 3000,
            }
        )])
        .is_err()
    );
    let mut invalid_san = Route::new("app.localhost", Upstream::Local(1));
    invalid_san.certificate_names.push("*.localhost".into());
    assert!(RouteTable::new(vec![invalid_san]).is_err());
    Ok(())
}

/// `accept` drops an invalid host and a trailing-dot duplicate one by one and
/// keeps every other route of the snapshot.
#[test]
fn accept_drops_bad_entries_only() -> Result {
    let (table, rejected) = RouteTable::accept(vec![
        Route::new("web.acme.localhost", Upstream::Local(1)),
        Route::new("api_v2.acme.localhost", Upstream::Local(2)),
        Route::new("WEB.acme.localhost.", Upstream::Local(3)),
        Route::new("docs.other.localhost", Upstream::Local(4)),
    ]);
    let hosts: Vec<&str> = table
        .routes()
        .iter()
        .map(|route| route.host.as_str())
        .collect();
    assert_eq!(hosts, ["docs.other.localhost", "web.acme.localhost"]);
    assert_eq!(
        table.lookup("web.acme.localhost").ok_or("kept")?.target(),
        &Upstream::Local(1),
        "the first claim keeps the host"
    );
    assert_eq!(
        rejected,
        [
            devlanes_proxy::Rejected {
                index: 1,
                host: "api_v2.acme.localhost".into(),
                error: devlanes_proxy::RouteError::InvalidHost("api_v2.acme.localhost".into()),
            },
            devlanes_proxy::Rejected {
                index: 2,
                host: "WEB.acme.localhost.".into(),
                error: devlanes_proxy::RouteError::Duplicate("web.acme.localhost".into()),
            },
        ]
    );
    Ok(())
}

/// Host, forwarded metadata and hop headers are checked against a real upstream socket.
#[tokio::test]
async fn forwarding_headers_named_port_and_snapshot_updates() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let upstream = fixture(false, false).await?;
        let mut route = Route::new("app.lane.localhost", Upstream::Local(1));
        route
            .named_ports
            .insert("http".into(), Upstream::Address(upstream.address));
        route.port = Some("http".into());
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![route.clone()])?,
        );
        let running = run_proxy(proxy.clone(), None).await?;
        let response = request(
            TcpStream::connect(running.address).await?,
            "APP.lane.localhost:8080",
            "/path?query=1",
            &[
                ("connection", "x-remove"),
                ("x-remove", "secret"),
                ("x-forwarded-for", "spoofed"),
                ("x-real-ip", "spoofed"),
                ("x-forwarded-ssl", "spoofed"),
                ("x-forwarded-scheme", "spoofed"),
                ("x-forwarded-prefix", "spoofed"),
                ("x-forwarded-server", "spoofed"),
                ("x-original-url", "spoofed"),
                ("front-end-https", "spoofed"),
            ],
        )
        .await?;
        assert_eq!(response.status(), 200);
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
        for name in [
            "x-real-ip",
            "x-forwarded-ssl",
            "x-forwarded-scheme",
            "x-forwarded-prefix",
            "x-forwarded-server",
            "x-original-url",
            "front-end-https",
        ] {
            assert!(!text.contains(&format!("{name}:")), "{name} forwarded");
        }
        assert!(text.contains("host: APP.lane.localhost:8080\r\n"));
        assert!(text.contains("x-forwarded-proto: http\r\n"));
        assert!(text.contains("x-forwarded-for: 127.0.0.1\r\n"));
        assert!(text.contains("x-forwarded-port: 8080\r\n"));
        assert!(!text.contains("spoofed"));
        assert!(!text.contains("secret"));
        assert!(text.starts_with("GET /path?query=1 HTTP/1.1\r\n"));
        route.host_header = HostHeader::Upstream;
        proxy.replace_routes(RouteTable::new(vec![route])?);
        let response = request(
            TcpStream::connect(running.address).await?,
            "app.lane.localhost",
            "/",
            &[],
        )
        .await?;
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
        assert!(text.contains(&format!("host: {}\r\n", upstream.address)));
        proxy.replace_routes(RouteTable::default());
        assert_eq!(
            request(
                TcpStream::connect(running.address).await?,
                "app.lane.localhost",
                "/",
                &[]
            )
            .await?
            .status(),
            404
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// IPv6-only services work through the Local target and unending SSE stays incremental.
#[tokio::test]
async fn ipv6_fallback_and_sse_streaming() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let upstream = fixture(true, false).await?;
        let route = Route::new(
            "app.lane.localhost",
            Upstream::Local(upstream.address.port()),
        );
        let running = run_proxy(
            Proxy::new(ProxyConfig::default(), RouteTable::new(vec![route])?),
            None,
        )
        .await?;
        let mut response = request(
            TcpStream::connect(running.address).await?,
            "app.lane.localhost",
            "/sse",
            &[],
        )
        .await?;
        assert_eq!(response.status(), 200);
        let frame = response.body_mut().frame().await.ok_or("no SSE frame")??;
        assert_eq!(frame.data_ref().ok_or("not data")?.as_ref(), b"data: 1\n\n");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Status output escapes logs, and loop markers stop re-entry before dialing.
#[tokio::test]
async fn status_pages_loop_detection_and_redirect() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut route = Route::new("app.lane.localhost", Upstream::Local(1));
        route.state = RouteState::Failed {
            log_tail: "<script>bad()</script>".into(),
        };
        let table = RouteTable::new(vec![route])?;
        let running = run_proxy(Proxy::new(ProxyConfig::default(), table.clone()), None).await?;
        let response = request(
            TcpStream::connect(running.address).await?,
            "app.lane.localhost",
            "/",
            &[],
        )
        .await?;
        assert_eq!(response.status(), 503);
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
        assert!(text.contains("&lt;script&gt;"));
        let response = request(
            TcpStream::connect(running.address).await?,
            "app.lane.localhost",
            "/",
            &[("x-devlanes-via", "app.lane.localhost")],
        )
        .await?;
        assert_eq!(response.status(), 508);
        let config = ProxyConfig {
            https_redirect_port: Some(8443),
            ..ProxyConfig::default()
        };
        let redirect = run_proxy(Proxy::new(config, table), None).await?;
        let response = request(
            TcpStream::connect(redirect.address).await?,
            "app.lane.localhost:8080",
            "/a?b=c",
            &[],
        )
        .await?;
        assert_eq!(response.status(), 308);
        assert_eq!(
            response.headers()["location"],
            "https://app.lane.localhost:8443/a?b=c"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Real WebSocket frames survive both upgrades and the bidirectional bridge.
#[tokio::test]
async fn websocket_echo() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let upstream = fixture(false, true).await?;
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![Route::new("app.lane.localhost", upstream.address)])?,
        );
        let running = run_proxy(proxy, None).await?;
        let (mut websocket, _) = tokio_tungstenite::client_async(
            "ws://app.lane.localhost/ws",
            TcpStream::connect(running.address).await?,
        )
        .await?;
        websocket
            .send(tokio_tungstenite::tungstenite::Message::text("echo me"))
            .await?;
        assert_eq!(
            websocket.next().await.ok_or("missing echo")??.into_text()?,
            "echo me"
        );
        websocket.close(None).await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Configures a client that verifies the generated root and requested DNS SAN.
fn tls_client(ca: &CertificateAuthority, alpn: &[u8]) -> Result<tokio_rustls::TlsConnector> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.certificate_der())?;
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// TLS tests verify h1, h2 ALPN, SSE and rejection after a routing snapshot changes.
#[tokio::test]
async fn tls_h1_h2_sse_and_unknown_host() -> Result {
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ca");
        let ca = Arc::new(
            tokio::task::spawn_blocking(move || CertificateAuthority::load_or_create(&path))
                .await??,
        );
        let upstream = fixture(false, false).await?;
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![Route::new("*.lane.localhost", upstream.address)])?,
        );
        let running = run_proxy(proxy.clone(), Some(proxy.tls_config(ca.clone())?)).await?;
        let h1 = tls_client(&ca, b"http/1.1")?;
        let stream = h1
            .connect(
                "deep.app.lane.localhost".try_into()?,
                TcpStream::connect(running.address).await?,
            )
            .await?;
        assert_eq!(
            stream.get_ref().1.alpn_protocol(),
            Some(b"http/1.1".as_slice())
        );
        let mut response = request(stream, "deep.app.lane.localhost", "/sse", &[]).await?;
        assert_eq!(
            response
                .body_mut()
                .frame()
                .await
                .ok_or("missing TLS SSE")??
                .data_ref()
                .ok_or("not data")?
                .as_ref(),
            b"data: 1\n\n"
        );
        let stream = tls_client(&ca, b"h2")?
            .connect(
                "app.lane.localhost".try_into()?,
                TcpStream::connect(running.address).await?,
            )
            .await?;
        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        let (mut sender, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let response = sender
            .send_request(
                Request::builder()
                    .uri("https://app.lane.localhost/")
                    .header("cookie", "a=1")
                    .header("cookie", "b=2")
                    .body(Empty::<Bytes>::new())?,
            )
            .await?;
        assert_eq!(response.status(), 200);
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
        assert!(text.contains("x-forwarded-proto: https\r\n"));
        assert!(text.contains("cookie: a=1; b=2\r\n"), "{text}");
        assert_eq!(text.matches("cookie:").count(), 1);
        assert!(
            h1.connect(
                "unknown.localhost".try_into()?,
                TcpStream::connect(running.address).await?
            )
            .await
            .is_err()
        );
        proxy.replace_routes(RouteTable::default());
        assert!(
            h1.connect(
                "app.lane.localhost".try_into()?,
                TcpStream::connect(running.address).await?
            )
            .await
            .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// TLS WebSockets remain HTTP/1.1 while ordinary clients may negotiate h2.
#[tokio::test]
async fn tls_websocket_echo() -> Result {
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ca");
        let ca = Arc::new(
            tokio::task::spawn_blocking(move || CertificateAuthority::load_or_create(&path))
                .await??,
        );
        let upstream = fixture(false, true).await?;
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![Route::new("app.lane.localhost", upstream.address)])?,
        );
        let running = run_proxy(proxy.clone(), Some(proxy.tls_config(ca.clone())?)).await?;
        let stream = tls_client(&ca, b"http/1.1")?
            .connect(
                "app.lane.localhost".try_into()?,
                TcpStream::connect(running.address).await?,
            )
            .await?;
        let (mut websocket, _) =
            tokio_tungstenite::client_async("wss://app.lane.localhost/ws", stream).await?;
        websocket
            .send(tokio_tungstenite::tungstenite::Message::text("secure echo"))
            .await?;
        assert_eq!(
            websocket
                .next()
                .await
                .ok_or("missing secure echo")??
                .into_text()?,
            "secure echo"
        );
        websocket.close(None).await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// IPv4-mapped peers cannot bypass the default loopback filter.
#[test]
fn loopback_policy() -> Result {
    for address in ["127.0.0.1", "127.20.30.40", "::1", "::ffff:127.0.0.2"] {
        assert!(devlanes_proxy::is_loopback(address.parse()?));
    }
    for address in ["192.168.1.1", "::ffff:192.168.1.1", "::", "0.0.0.0"] {
        assert!(!devlanes_proxy::is_loopback(address.parse()?));
    }
    Ok(())
}

/// Both families share an ephemeral port and occupied ports produce explicit fallback metadata.
#[tokio::test]
async fn dual_stack_bind_and_fallback() -> Result {
    let first = devlanes_proxy::bind_listeners(0, 0, false)?;
    let first_ipv6 = first.ipv6.as_ref().ok_or("IPv6 listener missing")?;
    assert_eq!(
        first.ipv4.local_addr()?.port(),
        first_ipv6.local_addr()?.port()
    );
    assert!(first.fallback_reason.is_none());
    assert!(first.ipv6_error.is_none());
    let second = devlanes_proxy::bind_listeners(first.port, 0, false)?;
    assert_ne!(first.port, second.port);
    assert!(second.fallback_reason.is_some());
    assert!(second.ipv4.local_addr()?.ip().is_loopback());
    let second_ipv6 = second.ipv6.as_ref().ok_or("IPv6 listener missing")?;
    assert!(second_ipv6.local_addr()?.ip().is_loopback());
    assert_eq!(second.listeners().count(), 2);
    Ok(())
}

/// A service pointed back at the listener terminates with 508 instead of opening unbounded sockets.
#[tokio::test]
async fn actual_proxy_loop() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let proxy = Proxy::new(ProxyConfig::default(), RouteTable::default());
        let running = run_proxy(proxy.clone(), None).await?;
        proxy.replace_routes(RouteTable::new(vec![Route::new(
            "app.lane.localhost",
            running.address,
        )])?);
        let response = request(
            TcpStream::connect(running.address).await?,
            "app.lane.localhost",
            "/",
            &[],
        )
        .await?;
        assert_eq!(response.status(), 508);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Malformed authorities cannot be confused with the same hostname's valid route.
#[tokio::test]
async fn invalid_authorities_are_rejected() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![Route::new("app.lane.localhost", Upstream::Local(1))])?,
        );
        let running = run_proxy(proxy, None).await?;
        for host in [
            "user@app.lane.localhost",
            "app.lane.localhost:notaport",
            "app.lane.localhost:99999",
        ] {
            let response =
                request(TcpStream::connect(running.address).await?, host, "/", &[]).await?;
            assert_eq!(response.status(), 400, "authority {host}");
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// A keep-alive upstream that answers `<name> <connection number>` and counts its connections.
async fn keepalive_fixture(name: &'static str) -> Result<(Running, Arc<AtomicUsize>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let task = tokio::spawn(async move {
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                Some(_) = clients.join_next(), if !clients.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { break; };
                    let number = count.fetch_add(1, Ordering::SeqCst) + 1;
                    clients.spawn(async move {
                        let service = hyper::service::service_fn(move |_request| async move {
                            let body = Full::new(Bytes::from(format!("{name} {number}")));
                            Ok::<_, Infallible>(hyper::Response::new(body))
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(socket), service)
                            .await;
                    });
                }
            }
        }
    });
    Ok((
        Running {
            address,
            shutdown,
            task,
        },
        connections,
    ))
}

/// Reads a whole response body as text.
async fn text(response: hyper::Response<Incoming>) -> Result<String> {
    Ok(String::from_utf8(
        response.into_body().collect().await?.to_bytes().to_vec(),
    )?)
}

/// Sequential requests reuse one upstream connection per target and never cross targets.
#[tokio::test]
async fn upstream_connections_are_pooled_per_target() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (a, a_connections) = keepalive_fixture("a").await?;
        let (b, b_connections) = keepalive_fixture("b").await?;
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![
                Route::new("a.lane.localhost", a.address),
                Route::new("b.lane.localhost", b.address),
            ])?,
        );
        let running = run_proxy(proxy, None).await?;
        for _ in 0..5 {
            for (host, expected) in [("a.lane.localhost", "a 1"), ("b.lane.localhost", "b 1")] {
                let response =
                    request(TcpStream::connect(running.address).await?, host, "/", &[]).await?;
                assert_eq!(text(response).await?, expected);
                // The connection returns to the pool on a task once the body has been read.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        assert_eq!(a_connections.load(Ordering::SeqCst), 1);
        assert_eq!(b_connections.load(Ordering::SeqCst), 1);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Clients reset before accept (SO_LINGER 0) never stop the listener, and the cap holds.
#[tokio::test]
async fn accept_errors_do_not_stop_the_listener() -> Result {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (upstream, _) = keepalive_fixture("up").await?;
        let limits = ProxyLimits {
            max_connections: 1,
            ..ProxyLimits::default()
        };
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![Route::new("app.lane.localhost", upstream.address)])?,
        )
        .with_limits(limits);
        let running = run_proxy(proxy.clone(), None).await?;
        for _ in 0..3 {
            // Holding the only permit keeps the resets queued in the backlog until after they abort.
            let mut holder = TcpStream::connect(running.address).await?;
            holder
                .write_all(b"GET / HTTP/1.1\r\nHost: app.lane.localhost\r\n\r\n")
                .await?;
            let mut buffer = [0u8; 1024];
            assert!(holder.read(&mut buffer).await? > 0);
            for _ in 0..20 {
                let socket = TcpStream::connect(running.address).await?;
                socket2::SockRef::from(&socket).set_linger(Some(Duration::ZERO))?;
                drop(socket);
            }
            let address = running.address;
            let waiting = tokio::spawn(async move {
                request(
                    TcpStream::connect(address).await?,
                    "app.lane.localhost",
                    "/",
                    &[],
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!waiting.is_finished(), "connection cap not enforced");
            drop(holder);
            assert_eq!(waiting.await??.status(), 200);
        }
        let health = proxy.listener_health();
        let listener = health
            .iter()
            .find(|listener| listener.address == running.address)
            .ok_or("listener health missing")?;
        assert!(listener.running);
        assert!(!listener.tls);
        assert!(listener.last_error.is_none());
        running.shutdown.cancel();
        while proxy
            .listener_health()
            .iter()
            .any(|listener| listener.address == running.address && listener.running)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Restores the file descriptor limit even when the test fails.
#[cfg(unix)]
struct NofileGuard(rustix::process::Rlimit);

#[cfg(unix)]
impl Drop for NofileGuard {
    /// Puts back the soft limit captured before the test lowered it.
    fn drop(&mut self) {
        let _ = rustix::process::setrlimit(rustix::process::Resource::Nofile, self.0);
    }
}

/// EMFILE on accept is reported through listener health, and the listener recovers.
#[cfg(unix)]
#[tokio::test]
async fn emfile_is_reported_and_recovered() -> Result {
    use std::os::fd::AsRawFd;
    // The descriptor limit is per process; nextest runs each test in its own process,
    // but a threaded `cargo test` run would starve concurrent tests.
    if std::env::var_os("NEXTEST").is_none() {
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let (upstream, _) = keepalive_fixture("up").await?;
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![Route::new("app.lane.localhost", upstream.address)])?,
        );
        let running = run_proxy(proxy.clone(), None).await?;
        let mut client = TcpStream::connect(running.address).await?;
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: app.lane.localhost\r\n\r\n")
            .await?;
        let original = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let guard = NofileGuard(original);
        // Descriptors are allocated lowest-first, so this leaves the proxy none to accept with.
        let lowest_free = std::fs::File::open("/dev/null")?.as_raw_fd();
        rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: Some(u64::try_from(lowest_free)?),
                maximum: original.maximum,
            },
        )?;
        let failing = loop {
            let health = proxy.listener_health();
            if let Some(listener) = health
                .into_iter()
                .find(|listener| listener.address == running.address && listener.accept_errors > 0)
            {
                break listener;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        drop(guard);
        assert!(failing.running);
        assert!(failing.last_error.is_some());
        // Linux keeps the pending connection queued; macOS drops it when the fd allocation fails.
        let mut head = [0u8; 12];
        match client.read_exact(&mut head).await {
            Ok(_) => assert_eq!(&head, b"HTTP/1.1 200"),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset),
        }
        let response = request(
            TcpStream::connect(running.address).await?,
            "app.lane.localhost",
            "/",
            &[],
        )
        .await?;
        assert_eq!(response.status(), 200);
        let health = proxy.listener_health();
        let recovered = health
            .iter()
            .find(|listener| listener.address == running.address)
            .ok_or("listener health missing")?;
        assert!(recovered.running);
        assert!(recovered.last_error.is_none());
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Incomplete request heads are cut off by the header timeout.
#[tokio::test]
async fn slow_headers_time_out() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let limits = ProxyLimits {
            header_timeout: Duration::from_millis(300),
            ..ProxyLimits::default()
        };
        let proxy = Proxy::new(ProxyConfig::default(), RouteTable::default()).with_limits(limits);
        let running = run_proxy(proxy, None).await?;
        let mut socket = TcpStream::connect(running.address).await?;
        socket.write_all(b"GET / HTTP/1.1\r\nHost: app").await?;
        let mut rest = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut rest)).await;
        assert!(closed.is_ok(), "slow client was not disconnected");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Idle HTTP/2 connections close, while an open SSE body keeps its connection active.
#[tokio::test]
async fn idle_h2_connections_close_but_streams_stay() -> Result {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (upstream, _) = keepalive_fixture("up").await?;
        let sse = fixture(false, false).await?;
        let limits = ProxyLimits {
            idle_timeout: Duration::from_millis(300),
            ..ProxyLimits::default()
        };
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![
                Route::new("app.lane.localhost", upstream.address),
                Route::new("sse.lane.localhost", sse.address),
            ])?,
        )
        .with_limits(limits);
        let running = run_proxy(proxy, None).await?;
        let h2 = |address| async move {
            let (sender, connection) = hyper::client::conn::http2::handshake(
                TokioExecutor::new(),
                TokioIo::new(TcpStream::connect(address).await?),
            )
            .await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((sender, tokio::spawn(connection)))
        };
        let get = |uri: &str| Request::builder().uri(uri).body(Empty::<Bytes>::new());

        let (mut sender, driver) = h2(running.address).await?;
        let response = sender
            .send_request(get("http://app.lane.localhost/")?)
            .await?;
        assert_eq!(text(response).await?, "up 1");
        // GOAWAY may end the client driver cleanly or with an error; either way it must end.
        let _ended = tokio::time::timeout(Duration::from_secs(3), driver).await??;

        let (mut sender, driver) = h2(running.address).await?;
        let mut response = sender
            .send_request(get("http://sse.lane.localhost/sse")?)
            .await?;
        let frame = response.body_mut().frame().await.ok_or("no SSE frame")??;
        assert_eq!(frame.data_ref().ok_or("not data")?.as_ref(), b"data: 1\n\n");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!driver.is_finished(), "streaming connection closed as idle");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}

/// Returns the leaf certificate the proxy presents for `name`.
async fn presented_leaf(
    ca: &CertificateAuthority,
    address: SocketAddr,
    name: &str,
) -> Result<Vec<u8>> {
    let stream = tls_client(ca, b"http/1.1")?
        .connect(
            rustls::pki_types::ServerName::try_from(name.to_owned())?,
            TcpStream::connect(address).await?,
        )
        .await?;
    let certificates = stream
        .get_ref()
        .1
        .peer_certificates()
        .ok_or("no peer certificate")?;
    Ok(certificates.first().ok_or("empty chain")?.to_vec())
}

/// State-only route updates keep cached leaves; a changed name set mints new ones.
#[tokio::test]
async fn certificate_cache_survives_state_updates() -> Result {
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ca");
        let ca = Arc::new(
            tokio::task::spawn_blocking(move || CertificateAuthority::load_or_create(&path))
                .await??,
        );
        let mut route = Route::new("web.lane.localhost", Upstream::Local(1));
        route.certificate_names = vec!["lane.localhost".into()];
        route.owner = Some("acme@0".into());
        let proxy = Proxy::new(
            ProxyConfig::default(),
            RouteTable::new(vec![route.clone()])?,
        );
        let running = run_proxy(proxy.clone(), Some(proxy.tls_config(ca.clone())?)).await?;
        let first = presented_leaf(&ca, running.address, "web.lane.localhost").await?;
        // The requested host leads the SANs, so it is the CN although the apex sorts first.
        let (_, leaf) = x509_parser::parse_x509_certificate(&first)?;
        let subject = leaf.subject();
        let common_name = subject.iter_common_name().next().map(|cn| cn.as_str());
        assert_eq!(common_name.transpose()?, Some("web.lane.localhost"));
        let unit = subject
            .iter_organizational_unit()
            .next()
            .map(|ou| ou.as_str());
        assert_eq!(unit.transpose()?, Some("acme@0"));
        let mut starting = route.clone();
        starting.state = RouteState::Starting;
        proxy.replace_routes(RouteTable::new(vec![starting])?);
        let same = presented_leaf(&ca, running.address, "web.lane.localhost").await?;
        assert_eq!(first, same);
        proxy.replace_routes(RouteTable::new(vec![
            route,
            Route::new("api.lane.localhost", Upstream::Local(2)),
        ])?);
        let rotated = presented_leaf(&ca, running.address, "web.lane.localhost").await?;
        assert_ne!(first, rotated);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?
}
