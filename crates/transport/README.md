# devlanes-transport

The remote protocol between a client's daemon and the daemon on a box, independent of how the bytes travel. The client daemon uses `Session` or `Link` to mirror the box (registry, data streams, syncs); the box daemon runs `serve` behind `devlanes tunnel --stdio`. Transports such as `devlanes-transport-ssh` implement `Transport`.

## Usage

Client side, reconnecting with backoff:

```rust
use std::sync::Arc;
use devlanes_transport::{Backoff, DialTarget, Link, SessionOptions, Transport};

async fn client(transport: Arc<dyn Transport>) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = SessionOptions::new("devlanes 0.1.0");
    options.client_id = "0f3c...".into(); // kept by the client machine across restarts
    options.client_name = "laptop".into();
    let link = Link::spawn(transport, options, Backoff::default());
    let session = link.connected().await?;
    println!("box proxy on {:?}", session.box_info().https_port);

    // Ports of non-hidden services and containers, bound locally under the same numbers.
    let ports: Vec<u16> = link.registry().current().mirrored_ports().into_keys().collect();
    if let Some(&port) = ports.first() {
        let _stream = session.open(DialTarget::Port { port }).await?;
    }
    Ok(())
}
```

Box side, over the tunnel's stdio:

```rust
use std::sync::Arc;
use devlanes_transport::{
    BoxServices, LoopbackDialer, NoSync, RegistrySnapshot, ServeOptions, SharedRegistry,
    TUNNEL_PREAMBLE, serve, stdio,
};
use tokio::io::AsyncWriteExt;

async fn tunnel() -> Result<(), Box<dyn std::error::Error>> {
    // Before any mux byte, so the client can skip what a login shell printed.
    let mut stdout = tokio::io::stdout();
    stdout.write_all(TUNNEL_PREAMBLE.as_bytes()).await?;
    stdout.flush().await?;
    let registry = SharedRegistry::new(RegistrySnapshot::default(), 256);
    let services = BoxServices {
        registry: Arc::new(registry.clone()),
        dialer: Arc::new(LoopbackDialer),
        sync: Arc::new(NoSync),
    };
    serve(stdio(), services, ServeOptions::new("devlanes 0.1.0")).await?;
    Ok(())
}
```

Publish changes with `registry.publish(event)`. A daemon that only has full snapshots can publish `old.diff(&new)`. A transport that runs the tunnel reads its output with `read_preamble(stream, wait)` and hands `Preamble::stream` to the session. A preamble naming another protocol revision is `PreambleError::Protocol`, which `PreambleError::is_permanent` reports as permanent.

## API

- `Transport`: produces one duplex byte stream per `connect`.
- `read_preamble`: skips output before the tunnel's `TUNNEL_PREAMBLE` line, keeping it for errors.
- `Session`: one connection; `open`, `subscribe_sync`, `features`, `has_feature`, `box_info`, `closed`.
- `Link`: keeps a session alive and shares one `ClientRegistry` across reconnects.
- `Registry`: snapshot plus events; `mirrored_ports`, `advertised_ports`, `authorize`, `diff`.
- `serve` with `BoxServices`: `RegistrySource` (told each client's `ClientInfo`), `Dialer`, `SyncSource`.
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

Only the client opens streams, at most 512 at once; an open beyond that fails alone and the session stays up. The box dials only ports of running, non-hidden services and containers in its registry, rejects everything else, and closes open streams when their unit stops, disappears or becomes hidden. Tasks are listed with their state but never mirrored or dialed. A data stream whose one side finished and that stays silent for 60 s is closed. There are no request streams: a client never runs commands on the box. On a sync stream the client may send input after its request (`SyncSubscription::send`, then `finish_input` for an explicit end marker); the `SyncSource` reads it from `SyncInput`, and a client sending faster than the source reads waits. Pings run every 10 s; a session dies after 20 s with neither a pong nor any other incoming frame, so a large transfer does not starve it. On reconnect, the box's snapshot replaces the client's registry; until then the client keeps the last known state. The session ends when the registry feed fails or ends.

Both ends must speak the same `PROTOCOL_VERSION`, which also covers devlanes' payload shapes, and a mismatch is a permanent error. It changes only for breaking changes. Additive behaviour is a named feature: both ends list theirs in the handshake (`FEATURES` by default), and a side uses a feature only when the other lists it too.

## License

Apache-2.0
