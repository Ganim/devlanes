# devlanes-client

The Unix-socket client shared by the devlanes CLI, shell completions, and TUI.
It uses `devlanes-ipc` and `devlanes-proto` to connect to the daemon and exchange requests and streaming frames.

## Usage

```rust
use devlanes_client::{Client, Context, Method, Paths};
use serde_json::json;

#[tokio::main]
async fn main() -> devlanes_client::Result<()> {
    let paths = Paths::discover()?;
    let mut client = Client::connect(&paths).await?;
    let status = client
        .request(Method::DaemonStatus, json!({}), Context::default())
        .await?;
    println!("{status}");
    Ok(())
}
```

`connect` never spawns or creates state. `connect_or_spawn` can launch the current
executable as `daemon run`, so use it from a binary that implements that command.
Auto-spawn is refused when sandbox markers are present or the socket-bind probe fails.

## API

- `Paths::discover()` resolves paths without writing files; `Paths::new(home)` selects an explicit home.
- `Client::request` returns response data or a typed error, including `Error::Rpc`.
- `begin_request`, `next_frame`, and `send_bytes` support events and raw terminal traffic.
- `Client::peer` exposes the daemon hello; `Client::spawned` records whether this connection started the daemon.

## Behaviour

Do not mix `request` with outstanding `begin_request` calls on the same connection.
Protocol mismatch returns the peer's version and managed-process count without stopping it.

The home is `DEVLANES_HOME` or `~/.devlanes`; persistent paths stay there.
The default socket is `run/daemon.sock`. On Linux, an absolute, non-symlink
`XDG_RUNTIME_DIR` owned by the current euid with mode `0700` instead supplies
`devlanes/daemon-<home hash>.sock`. Invalid or overlong runtime paths are ignored.

macOS ignores XDG. On either platform, an overlong home socket path falls back to
`$TMPDIR/devlanes-<uid>/daemon-<home hash>.sock`, or `/tmp` if that path is still too long.
`Paths::prepare()` creates private directories and writes `run/daemon.socket` for
relocated sockets. The lock remains at `run/daemon.lock` under the home.

## License

Apache-2.0
