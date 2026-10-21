# devlanes-tunnel

The box side of `devlanes remote connect`. The client's SSH transport runs `devlanes tunnel --stdio` on the box, and this crate answers the mux streams on that pipe from the box's own daemon, which it reaches over the daemon's Unix socket like any other client. Remote is mirroring only: the client gets the registry, data streams and syncs, and never runs commands on the box.

## Usage

```rust
use devlanes_client::Paths;

// `devlanes tunnel --stdio`: starts the box daemon if needed, then serves one
// client over stdin and stdout until it leaves.
async fn tunnel() -> Result<(), Box<dyn std::error::Error>> {
    devlanes_tunnel::run_stdio(Paths::discover()?).await?;
    Ok(())
}
```

To serve over another pipe, such as the in-memory transport in tests:

```rust
use devlanes_client::Paths;
use devlanes_proto::remote::RemoteClient;
use devlanes_transport::{Duplex, ServeError, ServeOptions, serve};

async fn serve_pipe(paths: Paths, stream: impl Duplex + 'static) -> Result<(), ServeError> {
    let client = RemoteClient { pid: std::process::id(), ssh: None, since_ms: 0 };
    let services = devlanes_tunnel::services(paths, client);
    serve(stream, services, ServeOptions::new(devlanes_tunnel::server_label())).await
}
```

## API

- `run_stdio(paths)`: the `tunnel --stdio` entry point.
- `services(paths, client)`: `BoxServices` for `devlanes_transport::serve`; `client` is listed by the box's `status` while the session lasts.
- `server_label()`: the version label sent in the handshake.
- `TunnelError`: the box daemon was unreachable or the session failed.

## Config example

```yaml
# devlanes.yaml on the box
services:
  extension:
    run_script: apps/extension/package.json#dev
    # Streamed to the connected machine while the lane runs.
    sync:
      - from: apps/extension/.output/chrome-mv3
  admin:
    run: ./admin-server
    # Listed in the registry but never mirrored.
    hidden: true
```

The tunnel offers the `extension` sync to the client that ran `devlanes remote connect`, and refuses data streams to `admin`'s port.

## Behaviour

- The registry comes from the daemon's `status --all` and `hosts`, refreshed on daemon events. It carries ports, hostnames and states, never environments or secrets. Hidden services and containers are listed but never mirrored.
- Data streams only dial loopback ports of running units that are not hidden.
- Every daemon request a client sends is refused with `not_supported`.
- Syncs stream the entries the daemon lists in `sync.status`; the client names an entry and sends what it already holds, never a path. Files over 256 MiB are skipped, and one client follows at most 16 syncs.
- A daemon the tunnel starts does not inherit the SSH session's variables (`SSH_AUTH_SOCK` and friends). On Linux the handshake label says `(linger off)` when the box would stop it at logout.
- stdout carries only mux frames. Errors go to stderr, which ssh returns to the client as the connection error.

## License

Apache-2.0
