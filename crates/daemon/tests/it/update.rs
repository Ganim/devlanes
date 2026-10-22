//! Release checks against a local manifest server: the daemon reports a newer
//! release once it is old enough, keeps its cache when the server fails, and
//! makes no request when checks are off. Needs `curl`.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use devlanes_daemon::{
    Engine, EngineOptions, Handler, Session, integrations::Integrations, update::UpdateCheckOptions,
};
use devlanes_proto::{Context, Method, Request};
use devlanes_state::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::FakeRunner;

/// Serves `status` and `body` for every request and counts them.
async fn manifest_server(status: &'static str, body: String) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!(
        "http://{}/release-manifest.json",
        listener.local_addr().expect("address")
    );
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (url, hits)
}

/// Options that check at once against `url`.
fn checks(url: &str, min_age: Duration) -> UpdateCheckOptions {
    UpdateCheckOptions {
        url: url.into(),
        off: None,
        first_delay: Duration::ZERO,
        interval: Duration::from_secs(3600),
        min_age,
        timeout: Duration::from_secs(5),
    }
}

/// An engine over `home` with `checks`, reconciled so background work runs.
async fn engine(home: &Path, checks: UpdateCheckOptions, check_enabled: bool) -> Engine {
    std::fs::create_dir_all(home).expect("home");
    let store = Store::open(home.join("state.db")).await.expect("store");
    let mut options = EngineOptions::new(home, home.join("d.sock"));
    options.watch = false;
    options.update_check = checks;
    options.user.updates.check = check_enabled;
    let integ = Integrations::with_runner(Arc::new(FakeRunner::default()));
    let engine = Engine::new(options, store, integ, CancellationToken::new());
    engine.reconcile().await.expect("reconcile");
    engine
}

/// `daemon.status` on a fresh local session.
async fn daemon_status(engine: &Engine) -> Value {
    let (session, _frames) = Session::local(1);
    let request = Request {
        id: 1,
        method: Method::DaemonStatus,
        params: json!({}),
        context: Context::default(),
    };
    engine
        .handle(request, session)
        .await
        .expect("daemon.status")
}

/// Polls `daemon.status` until `done` holds, for up to ten seconds.
async fn status_until(engine: &Engine, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let status = daemon_status(engine).await;
        if done(&status) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon_status(engine).await
}

#[tokio::test]
async fn newer_release_is_reported_and_cached() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, hits) = manifest_server("200 OK", json!({"version": "99.0.0"}).to_string()).await;
        let engine = engine(dir.path(), checks(&url, Duration::ZERO), true).await;
        let status = status_until(&engine, |s| s.get("update").is_some()).await;
        assert_eq!(status["update"]["latest"], "99.0.0", "{status}");
        assert_eq!(status["release_check"]["state"], "ok");
        assert_eq!(status["release_check"]["latest"], "99.0.0");
        assert!(hits.load(Ordering::SeqCst) >= 1);
        let cache: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("update-check.json")).expect("cache"),
        )
        .expect("cache json");
        assert_eq!(cache["latest"], "99.0.0");
        assert!(cache["first_seen_at"].as_u64().is_some());
    })
    .await
    .expect("timed out");
}

#[tokio::test]
async fn fresh_release_waits_and_older_release_is_not_reported() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, _) = manifest_server("200 OK", json!({"version": "99.0.0"}).to_string()).await;
        let young = engine(
            &dir.path().join("young"),
            checks(&url, Duration::from_secs(7200)),
            true,
        )
        .await;
        let status = status_until(&young, |s| s["release_check"]["state"] == "ok").await;
        assert_eq!(status["release_check"]["latest"], "99.0.0", "{status}");
        assert!(status.get("update").is_none(), "{status}");

        let (url, _) = manifest_server("200 OK", json!({"version": "0.0.1"}).to_string()).await;
        let older = engine(
            &dir.path().join("older"),
            checks(&url, Duration::ZERO),
            true,
        )
        .await;
        let status = status_until(&older, |s| s["release_check"]["state"] == "ok").await;
        assert!(status.get("update").is_none(), "{status}");
    })
    .await
    .expect("timed out");
}

#[tokio::test]
async fn failures_keep_the_cached_release() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path()).expect("home");
        // Checked an hour ago: the daemon waits out the rest of the interval
        // unless the interval is shorter, as here.
        let hour_ago = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("ms")
            - 3_600_000;
        std::fs::write(
            dir.path().join("update-check.json"),
            json!({"checked_at": hour_ago, "latest": "98.0.0", "first_seen_at": hour_ago})
                .to_string(),
        )
        .expect("cache");
        let (url, hits) = manifest_server("500 Internal Server Error", "{}".into()).await;
        let mut options = checks(&url, Duration::ZERO);
        options.interval = Duration::from_millis(100);
        let engine = engine(dir.path(), options, true).await;
        let status = status_until(&engine, |s| s.get("update").is_some()).await;
        assert_eq!(status["update"]["latest"], "98.0.0", "{status}");
        for _ in 0..100 {
            if hits.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(hits.load(Ordering::SeqCst) >= 1, "the check never ran");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let status = daemon_status(&engine).await;
        assert_eq!(status["update"]["latest"], "98.0.0", "{status}");
        assert_eq!(status["release_check"]["checked_at"], hour_ago);
    })
    .await
    .expect("timed out");
}

#[tokio::test(flavor = "multi_thread")]
/// Through the socket, `daemon.status` carries the engine's release check for
/// a client of the same protocol, and the frozen fields alone for another.
async fn daemon_status_through_the_server_reports_the_release() {
    use devlanes_proto::{Hello, Outcome};

    tokio::time::timeout(Duration::from_secs(30), async {
        let tmp = crate::temp();
        let (url, _) = manifest_server("200 OK", json!({"version": "99.0.0"}).to_string()).await;
        let engine = engine(&tmp.path.join("home"), checks(&url, Duration::ZERO), true).await;
        status_until(&engine, |s| s.get("update").is_some()).await;
        let options = devlanes_daemon::Options {
            home: tmp.path.join("srv"),
            socket: crate::endpoint(&tmp.path.join("srv")),
        };
        let socket = options.socket.clone();
        let server = devlanes_daemon::Server::bind(options).await.expect("bind");
        let token = server.shutdown_token();
        let handler: Arc<dyn Handler> = Arc::new(engine.clone());
        let running = tokio::spawn(server.run(handler, || Ok(())));

        let mut same = crate::server::client(&socket).await;
        let Outcome::Ok { data } = crate::server::request(
            &mut same,
            1,
            Method::DaemonStatus,
            json!({}),
            Context::default(),
        )
        .await
        else {
            panic!("daemon.status");
        };
        assert_eq!(data["update"]["latest"], "99.0.0", "{data}");
        assert_eq!(data["release_check"]["state"], "ok", "{data}");
        assert!(data["managed_processes"].is_u64() && data["socket"].is_string());

        let mut newer = Hello::current(0);
        newer.protocol += 1;
        let mut other = crate::server::client_with(&socket, newer).await;
        let Outcome::Ok { data } = crate::server::request(
            &mut other,
            2,
            Method::DaemonStatus,
            json!({}),
            Context::default(),
        )
        .await
        else {
            panic!("frozen daemon.status");
        };
        assert!(data.get("release_check").is_none(), "{data}");
        assert!(data.get("update").is_none(), "{data}");
        assert!(data["version"].is_string() && data["pid"].is_u64());

        token.cancel();
        running.await.expect("join").expect("clean shutdown");
    })
    .await
    .expect("timed out");
}

#[tokio::test]
async fn disabled_checks_make_no_request() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, hits) = manifest_server("200 OK", json!({"version": "99.0.0"}).to_string()).await;
        let by_config = engine(
            &dir.path().join("config"),
            checks(&url, Duration::ZERO),
            false,
        )
        .await;
        let mut options = checks(&url, Duration::ZERO);
        options.off = Some("env");
        let by_env = engine(&dir.path().join("env"), options, true).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let status = daemon_status(&by_config).await;
        assert_eq!(
            status["release_check"],
            json!({"state": "off", "reason": "config"})
        );
        assert!(status.get("update").is_none());
        let status = daemon_status(&by_env).await;
        assert_eq!(status["release_check"]["reason"], "env");
    })
    .await
    .expect("timed out");
}
