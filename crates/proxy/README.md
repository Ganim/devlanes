# devlanes-proxy

The daemon's HTTP and TLS reverse proxy routes hostnames to service ports, including remote tunnel endpoints. It uses Hyper for streaming requests and `devlanes-pki` for TLS certificates authorized by the current routes.

## Usage

```rust
use devlanes_proxy::{Proxy, ProxyConfig, Route, RouteTable, Upstream};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

async fn serve(shutdown: CancellationToken) -> Result<(), Box<dyn std::error::Error>> {
    let routes = RouteTable::new(vec![Route::new(
        "app.example.localhost",
        Upstream::Local(3000),
    )])?;
    let proxy = Proxy::new(ProxyConfig::default(), routes);
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    proxy.serve(listener, shutdown).await?;
    Ok(())
}
```

## API

- `RouteTable::new` validates exact, wildcard and fallback routes, including named-port selections.
- `Proxy::replace_routes` publishes a snapshot without interrupting active streams.
- `Proxy::tls_config` and `serve_tls` use a shared CA with HTTP/1.1 and HTTP/2 ALPN.
- `RouteState` supplies idle, starting, failed and reconnecting status pages.

## Connections

Host is preserved unless a route selects `HostHeader::Upstream`. `Upstream::Local` tries IPv4 loopback before IPv6. WebSockets and SSE stream without an idle timeout.

`bind_listeners(80, 8080, false)` binds both address families and falls back when the requested port is denied or occupied; use `(443, 8443, false)` for TLS. Serve both returned listeners through `Proxy`, which rejects non-loopback peers unless `ProxyConfig::lan` is enabled. Set `https_redirect_port` to the actual TLS port when enabling HTTP redirects.

## License

Apache-2.0
