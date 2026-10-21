# devlanes-ipc

Length-delimited framing for daemon connections, and the daemon's local endpoint. `devlanes-client`, the daemon and `devlanes-tunnel` wrap a local stream or a tunneled one in a `Connection` and exchange `devlanes-proto` control messages and raw terminal bytes over it.

## Usage

```rust
use bytes::Bytes;
use devlanes_ipc::{Connection, Error, Frame};

async fn echo() -> Result<(), Error> {
    let (a, b) = tokio::io::duplex(1024);
    let (mut a, mut b) = (Connection::new(a), Connection::new(b));
    a.send(Frame::Bytes { stream_id: 7, data: Bytes::from_static(b"ls\r") }).await?;
    if let Frame::Bytes { stream_id, data } = b.recv().await? {
        assert_eq!((stream_id, data.as_ref()), (7, &b"ls\r"[..]));
    }
    Ok(())
}
```

## API

- `Connection`: `new` wraps any `AsyncRead + AsyncWrite` stream; `send` and `recv` move one frame.
- `Frame`: `Control` for JSON protocol messages, `Bytes` for a numbered byte stream such as an attached terminal.
- `Error`: I/O, malformed JSON, invalid or oversized frames, and `Disconnected` on orderly EOF.
- `local::LocalListener`, `local::connect`: the daemon's endpoint, a Unix socket or a Windows named pipe, reachable only by the user running the daemon.

## Behaviour

Frames are capped at `devlanes_proto::MAX_FRAME_SIZE`, checked before the payload is allocated. Byte frames carry a big-endian stream id. A malformed frame is an error and the connection is not resynchronised; hello negotiation is left to the caller.

## Local endpoint

On Unix the endpoint is a socket file; a stale one is replaced, and peers are checked with `SO_PEERCRED`. On Windows it is a named pipe whose instances only the current user can open. The daemon asks for the first instance of the name, so a pipe another account created first makes it fail to start. The client checks the serving process's account, and the daemon checks the client's by impersonation after its first frame, because a pipe server can only impersonate after reading.

## License

Apache-2.0
