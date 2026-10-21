# devlanes-proto

The data the daemon and its clients exchange: requests, responses, events and the shared payload types. `devlanes-ipc` frames these messages, the daemon answers them, and `devlanes-cli`, `devlanes-client` and the TUI build and render them. It depends only on serde.

## Usage

```rust
use devlanes_proto::{Context, Control, Method, Outcome, Request, Response};
use serde_json::json;

fn status_request() -> Result<String, serde_json::Error> {
    let request = Request {
        id: 1,
        method: Method::Status,
        params: json!({}),
        context: Context { cwd: "/work/acme".into(), ..Context::default() },
    };
    serde_json::to_string(&Control::Request(request))
}

fn data(response: Response) -> Option<serde_json::Value> {
    match response.result {
        Outcome::Ok { data } => Some(data),
        Outcome::Error { error } => {
            eprintln!("{} (exit {})", error.code, error.exit_code);
            None
        }
    }
}
```

## API

- `Hello` and `PROTOCOL_VERSION`: the first message on a connection. A client refuses a daemon with another protocol version, except for `daemon.status` and `daemon.shutdown`, which every version answers.
- `Control`: the JSON frame kinds (`hello`, `request`, `response`, `event`). An unknown kind decodes as `Control::Unknown` so older peers skip it.
- `Request`, `Method` and `Context`: one call with its stable method name, parameters and the caller's cwd, environment, actor, project, lane and `--yes`.
- `Response`, `Outcome` and `RpcError`: the result, or an error with a machine code, data and exit code that clients turn into wording.
- `Event` and `events`: sequenced notifications and their names, such as `unit.changed` and `resync`.
- `Selection`, `LogQuery`, `ExecParams`, `RunParams`, `AttachParams`: request parameters shared by several commands.
- `UnitSummary`, `LaneSummary`, `ContainerSummary`, `Page`: listing payloads; lists are `{items, truncated, next_cursor}`.
- `remote`: messages for connected boxes.
- `release`: semver ordering (`is_newer`) and the `update` and `release_check` fields of `daemon.status`.
- `MAX_FRAME_SIZE`: the largest frame either side accepts.

## Behaviour

`Context` and `Request` don't implement `Debug`, because the context can carry the client's environment and secrets; never log or persist them. An unknown method decodes as `Method::Unknown`, which the daemon answers with an `unknown_method` error instead of dropping the connection.

## License

Apache-2.0
