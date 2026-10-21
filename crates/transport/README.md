# devlanes-transport

The remote protocol between a client's daemon and the daemon on a box, independent of how the bytes travel. The client daemon uses `Session` or `Link` to mirror the box (registry, data streams, syncs); the box daemon runs `serve` behind `devlanes tunnel --stdio`. Transports such as `devlanes-transport-ssh` implement `Transport`.

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

## Config example

```yaml
# devlanes.yaml on the box
services:
  admin:
    run: ./admin-server
    # Not mirrored to machines connected with devlanes remote connect.
    hidden: true
infra:
  # Containers whose ports are not mirrored either.
  hidden: [postgres]
```

A client running `devlanes remote connect <box>` binds every other service and container port locally under the same number; the box refuses streams to `admin`'s and `postgres`'s ports.

## Behaviour

Only the client opens streams, at most 512 at once; an open beyond that fails alone and the session stays up. The box dials only ports of running, non-hidden units in its registry, rejects everything else, and closes open streams when their unit stops, disappears or becomes hidden. A data stream whose one side finished and that stays silent for 60 s is closed. RPC streams exist in the protocol, but devlanes' box refuses every request: remote is mirroring only. On a sync stream the client may send input after its request (`SyncSubscription::send`, then `finish_input` for an explicit end marker); the `SyncSource` reads it from `SyncInput`, and a client sending faster than the source reads waits. Pings run every 10 s; a session dies after 20 s with neither a pong nor any other incoming frame, so a large transfer does not starve it. On reconnect, the box's snapshot replaces the client's registry; until then the client keeps the last known state. Both ends must speak the same `PROTOCOL_VERSION`, which also covers devlanes' payload shapes, and a mismatch is a permanent error.

## License

Apache-2.0
