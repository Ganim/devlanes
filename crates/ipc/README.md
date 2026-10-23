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
- `local`: `LocalListener` (`bind`, `accept`), `connect`, `LocalStream`, `remove` and `probe` for the daemon's endpoint.

## Behaviour

Frames are capped at `devlanes_proto::MAX_FRAME_SIZE`, checked before the payload is allocated. Byte frames carry a big-endian stream id. A malformed frame is an error and the connection is not resynchronised; hello negotiation is left to the caller.

The local endpoint is a Unix socket whose peers are checked by uid, or on Windows a named pipe (`\\.\pipe\...`) that only the current user can open and whose owner the client checks. A pipe name another process created first is refused at bind.

## License

Apache-2.0
