# devlanes-transport

The remote protocol between a client's daemon and the daemon on a box, independent of how the bytes travel. The client daemon uses `Session` or `Link` to mirror the box and forward CLI requests; the box daemon runs `serve` behind `devlanes tunnel --stdio`. Transports such as `devlanes-transport-ssh` implement `Transport`.

## Usage

Client side, reconnecting with backoff:

```rust
use std::sync::Arc;
use devlanes_transport::{Backoff, DialTarget, Link, SessionOptions, Transport};
use serde_json::json;

async fn client(transport: Arc<dyn Transport>) -> Result<(), Box<dyn std::error::Error>> {
    let link = Link::spawn(transport, SessionOptions::new("devlanes 0.1.0"), Backoff::default());
    let session = link.connected().await?;

    // Ports of non-hidden units, bound locally under the same numbers.
    let ports: Vec<u16> = link.registry().current().mirrored_ports().into_keys().collect();
    if let Some(&port) = ports.first() {
        let _stream = session.open(DialTarget::Port { port }).await?;
    }

    let _status = session.request(json!({"method": "status"})).await?;
    Ok(())
}
```

Box side, over the tunnel's stdio:

```rust
use std::sync::Arc;
use devlanes_transport::{
    BoxServices, LoopbackDialer, NoSync, RegistrySnapshot, RpcHandler, ServeOptions,
    SharedRegistry, serve, stdio,
};

async fn tunnel(rpc: Arc<dyn RpcHandler>) -> Result<(), Box<dyn std::error::Error>> {
    let registry = SharedRegistry::new(RegistrySnapshot::default(), 256);
    let services = BoxServices {
        registry: Arc::new(registry.clone()),
        rpc,
        dialer: Arc::new(LoopbackDialer),
        sync: Arc::new(NoSync),
    };
    serve(stdio(), services, ServeOptions::new("devlanes 0.1.0")).await?;
    Ok(())
}
```

Publish changes with `registry.publish(event)`. A daemon that only has full snapshots can publish `old.diff(&new)`.

## API

- `Transport`: produces one duplex byte stream per `connect`.
- `Session`: one connection; `open`, `call`, `request`, `subscribe_sync`, `closed`.
- `Link`: keeps a session alive and shares one `ClientRegistry` across reconnects.
- `Registry`: snapshot plus events; `mirrored_ports`, `authorize`, `diff`.
- `serve` with `BoxServices`: `RegistrySource`, `RpcHandler`, `Dialer`, `SyncSource`.
- `memory::memory_transport`: in-process transport with freeze and sever controls for tests.

## Behaviour

Only the client opens streams. The box dials only ports advertised by non-hidden units in its registry and rejects everything else. Each forwarded request gets its own stream, and dropping the call cancels it on the box. Pings run every 10 s and a session dies after 20 s without a pong. On reconnect, the box's snapshot replaces the client's registry; until then the client keeps the last known state. Both ends must speak the same `PROTOCOL_VERSION`, and a mismatch is a permanent error.

## License

Apache-2.0
