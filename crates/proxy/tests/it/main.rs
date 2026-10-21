use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use devlanes_pki::CertificateAuthority;
use devlanes_proxy::{HostHeader, Proxy, ProxyConfig, Route, RouteState, RouteTable, Upstream};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Empty};
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
            ],
        )
        .await?;
        assert_eq!(response.status(), 200);
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
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
                    .body(Empty::<Bytes>::new())?,
            )
            .await?;
        assert_eq!(response.status(), 200);
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
        assert!(text.contains("x-forwarded-proto: https\r\n"));
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
    assert_eq!(
        first.ipv4.local_addr()?.port(),
        first.ipv6.local_addr()?.port()
    );
    assert!(first.fallback_reason.is_none());
    let second = devlanes_proxy::bind_listeners(first.port, 0, false)?;
    assert_ne!(first.port, second.port);
    assert!(second.fallback_reason.is_some());
    assert!(second.ipv4.local_addr()?.ip().is_loopback());
    assert!(second.ipv6.local_addr()?.ip().is_loopback());
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
