//! The `/etc/hosts` fallback through the engine: route snapshots reach the helper,
//! `doctor` and `why` report its state, and `hosts.retry` reapplies at once.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use devlanes_daemon::{
    Engine, EngineOptions, Handler, Session,
    hosts::{HostsFile, HostsOptions},
    integrations::Integrations,
};
use devlanes_proto::{Method, Request};
use devlanes_resolver_hosts::{
    CommandOutput, FileKind, HostsClient, HostsFilePolicy, INSTALL_PATH, LocalhostResolution,
    PROTOCOL, SyncConfig,
    fake::{FakeExecutor, FakeFs},
};
use devlanes_state::Store;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{FakeRunner, LIMIT, cx, eventually, repo, temp};

/// `web` gets an exact host and a wildcard; the project asks for the hosts file.
const CONFIG: &str = r#"project: demo
lanes:
  hosts_file: always
services:
  web:
    run: serve
    hosts: ["web.${{ lane.domain }}", "*.pages.${{ lane.domain }}"]
"#;

/// A fake helper behind sudo; `deny` makes sudo refuse `apply`.
fn helper(deny: Arc<AtomicBool>) -> Arc<FakeExecutor> {
    Arc::new(FakeExecutor::new(move |command| {
        if command.args.iter().any(|arg| arg == "version") {
            return Ok(CommandOutput::ok(format!(
                "devlanes-hosts 0.1.0 protocol={PROTOCOL} build=release\n"
            )));
        }
        if deny.load(Ordering::SeqCst) {
            return Ok(CommandOutput::failed(1, "sudo: a password is required\n"));
        }
        let count = command
            .stdin
            .as_deref()
            .map_or(0, |stdin| stdin.iter().filter(|&&b| b == b'\n').count());
        Ok(CommandOutput::ok(format!("changed names={count}\n")))
    }))
}

/// stdin of every `apply` the helper received.
fn applies(executor: &FakeExecutor) -> Vec<String> {
    executor
        .calls()
        .into_iter()
        .filter(|command| command.args.iter().any(|arg| arg == "apply"))
        .map(|command| String::from_utf8(command.stdin.unwrap_or_default()).expect("utf-8"))
        .collect()
}

/// An engine over `home` with the fallback in `auto` mode and a working resolver,
/// so only the project's `always` hint selects names.
async fn engine(home: &Path, executor: Arc<FakeExecutor>) -> Engine {
    let fs = FakeFs::standard();
    fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
    let client = HostsClient::new(executor, Arc::new(fs), None).expect("client");
    let hosts = HostsFile::spawn(
        client,
        HostsOptions {
            policy: HostsFilePolicy::new(None),
            resolution: Some(LocalhostResolution::Loopback),
            installed: true,
            tld_error: None,
            sync: SyncConfig {
                debounce: Duration::from_millis(20),
                max_delay: Duration::from_millis(200),
                ..SyncConfig::default()
            },
            expected_version: "0.1.0".into(),
        },
    );
    let mut integ = Integrations::with_runner(Arc::new(FakeRunner::default()));
    integ.hosts = Some(Arc::new(hosts));
    std::fs::create_dir_all(home).expect("home");
    let store = Store::open(home.join("state.db")).await.expect("store");
    let mut options = EngineOptions::new(home, home.join("d.sock"));
    options.watch = false;
    let engine = Engine::new(options, store, integ, CancellationToken::new());
    engine.reconcile().await.expect("reconcile");
    engine
}

/// One request on a fresh local session.
async fn call(engine: &Engine, method: Method, params: Value, cwd: &Path) -> Value {
    let (session, _frames) = Session::local(1);
    let request = Request {
        id: 1,
        method,
        params,
        context: cx(cwd),
    };
    engine.handle(request, session).await.expect("request")
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_reach_the_helper_and_retry_reapplies() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let deny = Arc::new(AtomicBool::new(true));
        let executor = helper(deny.clone());
        let engine = engine(&tmp.path.join("home"), executor.clone()).await;
        call(&engine, Method::LaneList, json!({}), &main).await;

        // The wildcard never reaches the helper; sudo refuses the exact name.
        let first = eventually(Duration::from_secs(10), || async {
            applies(&executor).first().cloned()
        })
        .await
        .expect("an apply after the route snapshot");
        assert_eq!(first, "web.demo.localhost\n");
        let failed = eventually(Duration::from_secs(10), || async {
            let doctor = call(&engine, Method::Doctor, json!({}), &main).await;
            (doctor["hosts"]["state"] == "failed").then_some(doctor)
        })
        .await
        .expect("doctor reports the denial");
        assert_eq!(failed["hosts"]["mode"], "auto");
        assert_eq!(failed["hosts"]["probe"], "loopback");
        assert_eq!(failed["hosts"]["last_error"]["code"], "sudo_denied");
        let why = call(&engine, Method::Why, json!({"unit": "web"}), &main).await;
        let reasons = why["reasons"].as_array().expect("reasons");
        assert!(
            reasons.iter().any(|reason| reason["code"] == "sudo_denied"
                && reason["data"]["hosts"] == json!(["web.demo.localhost"])),
            "{why}"
        );

        // `trust --hosts` fixed sudo; the retry skips the five-minute backoff.
        deny.store(false, Ordering::SeqCst);
        let retried = call(&engine, Method::HostsRetry, json!({}), &main).await;
        assert_eq!(retried["retried"], true);
        assert_eq!(retried["hosts"]["state"], "applied", "{retried}");
        assert_eq!(retried["hosts"]["names"], 1);
        assert_eq!(applies(&executor).len(), 2);
        let _ = engine.shutdown(crate::signal_shutdown()).await;
        assert!(
            executor
                .argvs()
                .iter()
                .all(|argv| argv.last().map(String::as_str) != Some("clear")),
            "shutdown leaves the block in place"
        );
    })
    .await
    .expect("timeout");
}
