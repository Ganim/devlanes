//! Attach handoff and connection churn must never stop the daemon from answering.

use std::{sync::Arc, time::Duration};

use devlanes_daemon::{Handler, Options, Server};
use devlanes_ipc::{Connection, Frame};
use devlanes_proto::{Context, Control, Hello, Method, Outcome, Request};
use serde_json::{Value, json};
use tokio::net::UnixStream;

use crate::{LIMIT, Setup, cx, harness, repo, temp};

const CONFIG: &str = "project: churn\nservices:\n  one: { run: sleep }\n  two: { run: sleep }\n  three: { run: sleep }\n";

/// Bound for the request that must still be answered.
const ANSWER: Duration = Duration::from_secs(2);

/// A handshaken client connection.
pub(crate) async fn client(socket: &std::path::Path) -> Connection<UnixStream> {
    client_with(socket, Hello::current(0)).await
}

/// A connection that introduced itself with `hello`.
pub(crate) async fn client_with(socket: &std::path::Path, hello: Hello) -> Connection<UnixStream> {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let mut connection = Connection::new(stream);
    connection
        .send(Frame::Control(Control::Hello(hello)))
        .await
        .expect("hello");
    let Frame::Control(Control::Hello(_)) = connection.recv().await.expect("hello reply") else {
        panic!("expected hello");
    };
    connection
}

/// Sends a request and waits for its response, skipping events and bytes.
pub(crate) async fn request(
    connection: &mut Connection<UnixStream>,
    id: u64,
    method: Method,
    params: Value,
    context: Context,
) -> Outcome {
    connection
        .send(Frame::Control(Control::Request(Request {
            id,
            method,
            params,
            context,
        })))
        .await
        .expect("send");
    loop {
        if let Frame::Control(Control::Response(response)) = connection.recv().await.expect("recv")
            && response.id == id
        {
            return response.result;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
/// Three interactive attaches opened and closed in quick succession on separate
/// sessions (the TUI moving its selection) leave `status` answering promptly.
async fn attach_handoff_keeps_status_answering() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        for unit in ["one", "two", "three"] {
            let (session, _frames) = h.session();
            h.call_on(
                Method::Attach,
                json!({"unit": unit, "interactive": true, "columns": 150, "rows": 40}),
                cx(&main),
                &session,
            )
            .await
            .expect("attach");
            session.close();
        }
        let (status, _frames) = h.session();
        let answered = tokio::time::timeout(
            ANSWER,
            h.call_on(Method::Status, json!({"all": true}), cx(&main), &status),
        )
        .await
        .expect("status answers within 2 s")
        .expect("status");
        assert_eq!(
            answered["items"][0]["units"].as_array().map(Vec::len),
            Some(3)
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Clients that vanish right after connecting (before their credentials are read)
/// and attach connections dropped mid-stream must not end the accept loop.
async fn server_survives_connection_churn() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let options = Options {
            home: tmp.path.join("srv"),
            socket: tmp.path.join("srv/run/d.sock"),
        };
        let socket = options.socket.clone();
        let server = Server::bind(options).await.expect("bind");
        let token = server.shutdown_token();
        let handler: Arc<dyn Handler> = Arc::new(h.engine.clone());
        let running = tokio::spawn(server.run(handler, || Ok(())));
        for _ in 0..50 {
            drop(UnixStream::connect(&socket).await.expect("connect"));
        }
        for (id, unit) in ["one", "two", "three"].into_iter().enumerate() {
            let mut connection = client(&socket).await;
            let outcome = request(
                &mut connection,
                id as u64 + 1,
                Method::Attach,
                json!({"unit": unit, "interactive": true, "columns": 150, "rows": 40}),
                cx(&main),
            )
            .await;
            assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
            drop(connection);
        }
        let mut status = client(&socket).await;
        let outcome = tokio::time::timeout(
            ANSWER,
            request(
                &mut status,
                99,
                Method::DaemonStatus,
                json!({}),
                Context::default(),
            ),
        )
        .await
        .expect("daemon.status answers within 2 s");
        assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
        assert!(!running.is_finished(), "the accept loop is still running");
        token.cancel();
        running.await.expect("join").expect("clean shutdown");
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Input to a unit that never reads queues per stream: the same connection keeps
/// getting responses, a full queue drops its oldest input with an `input.dropped`
/// event, and other streams still deliver in order.
async fn a_unit_that_never_reads_does_not_stall_the_connection() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config =
            "project: deaf\nservices:\n  deaf: { run: \"deaf; sleep\" }\n  echo: { run: sleep }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let options = Options {
            home: tmp.path.join("srv"),
            socket: tmp.path.join("srv/run/d.sock"),
        };
        let socket = options.socket.clone();
        let server = Server::bind(options).await.expect("bind");
        let token = server.shutdown_token();
        let handler: Arc<dyn Handler> = Arc::new(h.engine.clone());
        let running = tokio::spawn(server.run(handler, || Ok(())));

        let mut connection = client(&socket).await;
        let mut streams = Vec::new();
        for (id, unit) in [(1, "deaf"), (2, "echo")] {
            let Outcome::Ok { data } = request(
                &mut connection,
                id,
                Method::Attach,
                json!({"unit": unit, "interactive": true, "columns": 80, "rows": 24}),
                cx(&main),
            )
            .await
            else {
                panic!("attach {unit}");
            };
            streams.push(data["stream_id"].as_u64().expect("stream id"));
        }
        let (deaf, echo) = (streams[0], streams[1]);
        // This client writes without reading; a real one reads concurrently. Take
        // the attach snapshots first, so the daemon has nothing left to send it.
        while let Ok(frame) =
            tokio::time::timeout(Duration::from_millis(300), connection.recv()).await
        {
            frame.expect("recv");
        }
        // Well past the queue's bounds (frames and bytes).
        let chunk = bytes::Bytes::from(vec![b'x'; 1024]);
        for _ in 0..2048 {
            connection
                .send(Frame::Bytes {
                    stream_id: deaf,
                    data: chunk.clone(),
                })
                .await
                .expect("input");
        }
        for word in ["first", "second"] {
            connection
                .send(Frame::Bytes {
                    stream_id: echo,
                    data: bytes::Bytes::from(word),
                })
                .await
                .expect("input");
        }

        connection
            .send(Frame::Control(Control::Request(Request {
                id: 9,
                method: Method::DaemonStatus,
                params: json!({}),
                context: Context::default(),
            })))
            .await
            .expect("send");
        let mut answered = false;
        let mut warned = None;
        let mut echoed = String::new();
        let deadline = tokio::time::Instant::now() + ANSWER;
        while !(answered && warned.is_some() && echoed.contains("second")) {
            let frame = tokio::time::timeout_at(deadline, connection.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("answered {answered}, warned {warned:?}, echoed {echoed:?}")
                })
                .expect("recv");
            match frame {
                Frame::Control(Control::Response(response)) if response.id == 9 => {
                    assert!(matches!(response.result, Outcome::Ok { .. }));
                    answered = true;
                }
                Frame::Control(Control::Event(event))
                    if event.event == devlanes_proto::events::INPUT_DROPPED =>
                {
                    warned = Some(event.data);
                }
                Frame::Bytes { stream_id, data } if stream_id == echo => {
                    echoed.push_str(&String::from_utf8_lossy(&data));
                }
                _ => {}
            }
        }
        let warned = warned.expect("warning");
        assert_eq!(warned["stream_id"], deaf);
        assert_eq!(warned["code"], "input_overflow");
        assert!(warned["dropped_bytes"].as_u64().is_some_and(|b| b > 0));
        let first = echoed.find("first").expect("first echoed");
        assert!(first < echoed.find("second").expect("second"), "{echoed}");

        drop(connection);
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        token.cancel();
        running.await.expect("join").expect("clean shutdown");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Another protocol still gets the frozen `daemon.status` and `daemon.shutdown`
/// and a structured refusal for the rest; unknown frame types are skipped and
/// unknown methods answer `unknown_method`, all without closing the connection.
async fn other_protocols_and_unknown_frames_keep_the_connection() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let options = Options {
            home: tmp.path.join("srv"),
            socket: tmp.path.join("srv/run/d.sock"),
        };
        let socket = options.socket.clone();
        let server = Server::bind(options).await.expect("bind");
        let token = server.shutdown_token();
        let handler: Arc<dyn Handler> = Arc::new(h.engine.clone());
        let running = tokio::spawn(server.run(handler, || Ok(())));

        let mut newer = Hello::current(0);
        newer.protocol += 1;
        let mut old = client_with(&socket, newer).await;
        let status = request(
            &mut old,
            1,
            Method::DaemonStatus,
            json!({}),
            Context::default(),
        )
        .await;
        assert!(matches!(status, Outcome::Ok { .. }), "{status:?}");
        let Outcome::Error { error } =
            request(&mut old, 2, Method::Status, json!({}), Context::default()).await
        else {
            panic!("status needs the same protocol");
        };
        assert_eq!(error.code, "protocol_mismatch");
        // Plan 3.5: incompatible is exit 4.
        assert_eq!(error.exit_code, 4);
        assert_eq!(
            error.data["client_protocol"],
            Hello::current(0).protocol + 1
        );

        let mut same = client(&socket).await;
        same.send(Frame::Control(Control::Unknown))
            .await
            .expect("unknown frame");
        let Outcome::Error { error } =
            request(&mut same, 3, Method::Unknown, json!({}), Context::default()).await
        else {
            panic!("unknown method");
        };
        assert_eq!(error.code, "unknown_method");
        assert_eq!(error.exit_code, 2);
        let status = request(
            &mut same,
            4,
            Method::DaemonStatus,
            json!({}),
            Context::default(),
        )
        .await;
        assert!(matches!(status, Outcome::Ok { .. }), "still connected");

        // The upgrade path: a mismatched client can still stop the daemon.
        let stopped = request(
            &mut old,
            5,
            Method::DaemonShutdown,
            json!({}),
            Context::default(),
        )
        .await;
        assert!(matches!(stopped, Outcome::Ok { .. }), "{stopped:?}");
        running.await.expect("join").expect("clean shutdown");
        assert!(token.is_cancelled());
        h.close().await;
    })
    .await
    .expect("timeout");
}
