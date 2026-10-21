# devlanes-tunnel

The box side of `devlanes remote connect`. The client's SSH transport runs `devlanes tunnel --stdio` on the box, and this crate answers the mux streams on that pipe from the box's own daemon, which it reaches over the daemon's Unix socket like any other client.

## Usage

```rust
use devlanes_client::Paths;

// `devlanes tunnel --stdio`: starts the box daemon if needed, then serves one
// client over stdin and stdout until it leaves.
devlanes_tunnel::run_stdio(Paths::discover()?).await?;
```

To serve over another pipe, such as the in-memory transport in tests:

```rust
use devlanes_proto::remote::RemoteClient;
use devlanes_transport::{ServeOptions, serve};

let client = RemoteClient { pid: std::process::id(), ssh: None, since_ms: 0 };
let services = devlanes_tunnel::services(paths, client);
serve(stream, services, ServeOptions::new(devlanes_tunnel::server_label())).await?;
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

- The registry comes from the daemon's `status --all` and `hosts`, refreshed on daemon events. Hidden services and containers are listed but never mirrored.
- Data streams only dial loopback ports that a non-hidden unit in the registry advertises.
- Each forwarded request gets its own daemon connection. Attach bytes travel base64-encoded, because transport RPC values are JSON.
- Syncs stream the entries the daemon lists in `sync.status`; the client names an entry, never a path.
- Forwarded `unlock`, `connect`, `disconnect` and `host ls` return `not_supported_yet`.
- stdout carries only mux frames. Errors go to stderr, which ssh returns to the client as the connection error.

## License

Apache-2.0
