//! Real processes through the PTY runner and the fixture server.
//!
//! Build the server with `cargo build -p devlanes-testing` and run with
//! `DEVLANES_FIXTURE_SERVER=<path> cargo test -p devlanes-daemon -- --ignored`.

use std::{sync::Arc, time::Duration};

use devlanes_daemon::{
    Engine, EngineOptions, Handler,
    integrations::{CoreRunner, Integrations},
};
use devlanes_proto::{Method, StartResult};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{LIMIT, cx, repo, temp};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs fixture-server (DEVLANES_FIXTURE_SERVER)"]
/// Two fixture servers in one lane become ready on their allocated ports and stop.
async fn two_services_with_pty_runner() {
    tokio::time::timeout(LIMIT, async {
        let server = std::env::var("DEVLANES_FIXTURE_SERVER").expect("DEVLANES_FIXTURE_SERVER");
        let tmp = temp();
        let config = format!(
            "project: two\nservices:\n  web:\n    run: [\"{server}\"]\n    hosts: [\"web.${{{{ lane.domain }}}}\"]\n  api:\n    run: [\"{server}\"]\n    ready: {{ tcp: true }}\n"
        );
        let main = repo(&tmp.path, "r", &config);
        let home = tmp.path.join("home");
        std::fs::create_dir_all(&home).expect("home");
        let store = devlanes_state::Store::open(home.join("state.db")).await.expect("store");
        let integ = Integrations::with_runner(Arc::new(CoreRunner(devlanes_runner_pty::PtyRunner::default())));
        let mut options = EngineOptions::new(&home, home.join("d.sock"));
        options.watch = false;
        let engine = Engine::new(options, store, integ, CancellationToken::new());
        engine.reconcile().await.expect("reconcile");
        let (session, _frames) = devlanes_daemon::Session::local(1);
        let request = devlanes_proto::Request {
            id: 1,
            method: Method::Start,
            params: json!({"timeout_ms": 20_000}),
            context: cx(&main),
        };
        let result: StartResult =
            serde_json::from_value(engine.handle(request, session.clone()).await.expect("start")).expect("result");
        assert!(result.ready, "{result:?}");
        let ports: Vec<u16> = result.units.iter().filter_map(|u| u.ports.get("default").copied()).collect();
        assert_eq!(ports.len(), 2);
        for port in &ports {
            assert!(tokio::net::TcpStream::connect(("127.0.0.1", *port)).await.is_ok());
        }
        engine.shutdown().await.expect("shutdown");
        tokio::time::sleep(Duration::from_millis(200)).await;
        for port in ports {
            assert!(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err(), "port {port} still open");
        }
    })
    .await
    .expect("timeout");
}
