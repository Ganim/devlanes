# devlanes-tunnel

The box side of `devlanes remote connect`. The client's SSH transport runs `devlanes tunnel --stdio` on the box, and this crate answers the mux streams on that pipe from the box's own daemon, which it reaches over the daemon's Unix socket like any other client. Remote is mirroring only: the client gets the registry, data streams and syncs, and never runs commands on the box.

## Usage

```rust
use devlanes_client::Paths;

// `devlanes tunnel --stdio`: writes the preamble line, starts the box daemon
// if needed and allowed (the box's `remote.start_daemon`), then serves one
// client over stdin and stdout until it leaves.
async fn tunnel(start_daemon: bool) -> Result<(), Box<dyn std::error::Error>> {
    devlanes_tunnel::run_stdio(Paths::discover()?, start_daemon).await?;
    Ok(())
}
```

To serve over another pipe, such as the in-memory transport in tests:

```rust
use devlanes_client::Paths;
use devlanes_proto::remote::RemoteClient;
use devlanes_transport::{Duplex, ServeError, ServeOptions, serve};

async fn serve_pipe(
    paths: Paths,
    stream: impl Duplex + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = ServeOptions::new(devlanes_tunnel::server_label());
    options.box_info = devlanes_tunnel::box_info(&paths).await?;
    let client = RemoteClient { pid: std::process::id(), ..RemoteClient::default() };
    let services = devlanes_tunnel::services(paths, client);
    serve(stream, services, options).await?;
    Ok(())
}
```

## API

- `run_stdio(paths, start_daemon)`: the `tunnel --stdio` entry point.
- `services(paths, client)`: `BoxServices` for `devlanes_transport::serve`; `client`, with the id and name from each client's handshake, is listed by the box's `status` while the session lasts.
- `box_info(paths)`: the box daemon's HTTPS port and `ports.range`, for `ServeOptions::box_info`.
- `server_label()`: the version label sent in the handshake.
- `TunnelError`: the box daemon was unreachable or stopped, or the session failed; `exit_code` gives the process status.

## Config example

```yaml
# devlanes.yaml on the box
services:
  extension:
    run_script:
      file: apps/extension/package.json
      name: dev
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

- The registry is the daemon's `remote.registry` feed: a snapshot, then each change, built from the same lanes as `status --all` and the same routes as `hosts`. It carries ports, hostnames and states, never environments or secrets. Hidden services and containers are listed but never mirrored; tasks are listed with their state only. A feed that ends or sends something malformed ends the session, and a daemon older than the tunnel is reported with the fix (`devlanes daemon restart`).
- Data streams only dial loopback ports of running services and containers that are not hidden.
- There is nothing to send the box a command with: remote is mirroring only.
- Syncs stream the entries the daemon lists in `sync.status`; the client names an entry, never a path. It then sends the files it holds in batches and an end marker. Each batch is compared with the source on arrival and only its outcome is kept; after the marker the box sends new and changed files, then deletions. A client that leaves before the marker leaves nothing behind, so its next subscription starts over. Files over 256 MiB are skipped, and one client follows at most 16 syncs.
- A daemon the tunnel starts does not inherit the SSH session's variables (`SSH_AUTH_SOCK` and friends). On Linux the handshake label says `(linger off)` when the box would stop it at logout.
- With `start_daemon` false and no daemon running, the tunnel fails with `box daemon is not running` and exit status `devlanes_transport::EXIT_DAEMON_STOPPED`, which the client's SSH transport reports as temporary.
- stdout carries the `devlanes-tunnel 4` preamble line, then only mux frames. Errors go to stderr, which ssh returns to the client as the connection error.

## License

Apache-2.0
