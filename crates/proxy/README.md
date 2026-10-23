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
- `Proxy::listener_health` reports each accept loop's `ListenerHealth`: whether it is running, its accept error count and the latest error.
- `Proxy::with_limits` replaces the default `ProxyLimits` (connection cap, header timeout, idle timeout).

## Config example

```yaml
# ~/.devlanes/config.yaml: ports the proxy listens on.
proxy:
  https_port: 443
  http_port: 80
  # Also accept connections from other machines on the network.
  lan: false
```

The daemon binds these ports on both address families. When a port is denied or taken it falls back to 8443 for HTTPS and 8080 for HTTP and reports why in `devlanes doctor`.

## Behaviour

Host is preserved unless a route selects `HostHeader::Upstream`. `Upstream::Local` tries IPv4 loopback before IPv6. WebSockets and SSE stream without an idle timeout. Idle upstream connections are reused for a few seconds, only for requests to the same target; WebSocket upgrades always get their own connection.

Routes marked `fallback` apply only when no other route matches, even an exact fallback route against a wildcard. Within each group an exact host beats the longest matching wildcard.

The proxy replaces `Forwarded` and the `X-Forwarded-*` headers and drops client-sent `X-Real-IP`, `X-Forwarded-Ssl`, `X-Forwarded-Scheme`, `X-Forwarded-Prefix`, `X-Forwarded-Server`, `X-Original-URL` and `Front-End-Https`. Upstream apps always connect from loopback, so with `lan: true` they see 127.0.0.1 as the peer address and must read the client address from `X-Forwarded-For` or `Forwarded`. Failure pages include the service's log tail only for loopback clients.

`bind_listeners(80, 8080, false)` binds both address families and falls back when the requested port is denied or occupied; use `(443, 8443, false)` for TLS. If IPv6 is unavailable on the host, it serves IPv4 only and sets `ipv6_error`. Serve every listener from `BoundListeners::listeners` through `Proxy`, which rejects non-loopback peers unless `ProxyConfig::lan` is enabled. Set `https_redirect_port` to the actual TLS port when enabling HTTP redirects.

A failed `accept` never stops a listener: client resets are skipped and descriptor exhaustion is retried after a short pause. Check `listener_health` instead of the bind result to know whether a listener is still serving.

## Windows

Wildcard listeners (`lan: true`) set `SO_EXCLUSIVEADDRUSE`, so no other program can bind the same port on a more specific address and take connections. Loopback listeners do not need it. Low ports need no privilege on Windows; access denied means another program holds the port (port 80 is often http.sys, see `netsh http show servicestate`) or the port lies in a range Hyper-V, WSL or Docker reserved (`netsh int ipv4 show excludedportrange protocol=tcp`). Both fall back, and the fallback message names these commands.

## License

Apache-2.0
