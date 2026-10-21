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
async fn client(socket: &std::path::Path) -> Connection<UnixStream> {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let mut connection = Connection::new(stream);
    connection
        .send(Frame::Control(Control::Hello(Hello::current(0))))
        .await
        .expect("hello");
    let Frame::Control(Control::Hello(_)) = connection.recv().await.expect("hello reply") else {
        panic!("expected hello");
    };
    connection
}

/// Sends a request and waits for its response, skipping events and bytes.
async fn request(
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
