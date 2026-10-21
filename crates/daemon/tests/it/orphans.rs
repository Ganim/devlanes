//! Units a crashed daemon left running stay running as `orphaned` (decision 16)
//! until an explicit stop, restart or lane clean terminates them.

use std::time::Duration;

use devlanes_proto::{LaneSummary, Method, Page, StartResult, UnitSummary};
use serde_json::{Value, json};

use crate::{FakeProcesses, LIMIT, Setup, cx, eventually, harness, repo, temp};

const CONFIG: &str = "project: demo\nservices:\n  web: { run: sleep }\n  worker: { run: sleep }\n  api: { run: sleep }\n";

/// The lane at `cwd`.
async fn lane(h: &crate::Harness, cwd: &std::path::Path) -> LaneSummary {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items.into_iter().next().expect("lane")
}

/// `unit` of `lane`.
fn unit<'a>(lane: &'a LaneSummary, name: &str) -> &'a UnitSummary {
    lane.units
        .iter()
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("no {name}"))
}

/// Starts every unit in a first daemon, then "crashes" it: the second daemon
/// on the same home sees `alive` as still running (by pid), the rest gone.
async fn crashed(
    tmp: &crate::Temp,
    main: &std::path::Path,
    alive: &[&str],
) -> (
    crate::Harness,
    FakeProcesses,
    std::collections::BTreeMap<String, u32>,
) {
    let home = tmp.path.join("home");
    let first = harness(&home, Setup::default()).await;
    let started = first
        .call(Method::Start, json!({}), cx(main))
        .await
        .expect("start");
    let started: StartResult = serde_json::from_value(started).expect("start result");
    let pids: std::collections::BTreeMap<String, u32> = started
        .units
        .iter()
        .map(|u| (u.name.clone(), u.pid.expect("pid")))
        .collect();
    // The crash: the daemon goes away without stopping anything.
    drop(first);
    let processes = FakeProcesses::default();
    for name in alive {
        processes.alive.lock().expect("lock").insert(pids[*name]);
    }
    let second = harness(
        &home,
        Setup {
            processes: Some(processes.clone()),
            logs: Some(tmp.path.join("logs-after-crash")),
            ..Setup::default()
        },
    )
    .await;
    (second, processes, pids)
}

/// Unfinished runs in the store.
async fn active_runs(home: &std::path::Path) -> Vec<String> {
    let store = devlanes_state::Store::open(home.join("state.db"))
        .await
        .expect("store");
    store
        .active_unit_runs()
        .await
        .expect("runs")
        .into_iter()
        .map(|run| run.unit)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
/// Verified survivors stay running as `orphaned`, a dead record clears; start
/// refuses orphans but starts the rest; attach refuses; `why` explains; stop and
/// restart terminate them with verified signals.
async fn a_crashed_daemons_units_stay_running_as_orphans() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let (h, processes, pids) = crashed(&tmp, &main, &["web", "worker"]).await;
        let home = tmp.path.join("home");

        assert!(
            processes.killed.lock().expect("lock").is_empty(),
            "nothing is killed at startup"
        );
        let summary = lane(&h, &main).await;
        assert_eq!(summary.state, "running");
        let web = unit(&summary, "web");
        assert_eq!(web.state, "orphaned");
        assert_eq!(web.pid, Some(pids["web"]));
        assert!(web.started_at_ms.is_some() && web.orphaned_since_ms.is_some());
        assert_eq!(unit(&summary, "worker").state, "orphaned");
        assert_eq!(unit(&summary, "api").state, "idle", "a dead record clears");
        let mut active = active_runs(&home).await;
        active.sort();
        assert_eq!(active, ["web", "worker"]);

        let started = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let raw = started.clone();
        let started: StartResult = serde_json::from_value(started).expect("start result");
        assert_eq!(
            started.started,
            ["demo@0:api"],
            "other units proceed: {raw}"
        );
        let refused: Vec<&str> = started
            .failures
            .iter()
            .filter(|f| f.reason.as_ref().is_some_and(|r| r.code == "unit_orphaned"))
            .map(|f| f.unit.as_str())
            .collect();
        assert_eq!(refused, ["web", "worker"]);
        let reason = started.failures[0].reason.as_ref().expect("reason");
        assert_eq!(reason.exit_code, 1);
        assert_eq!(reason.data["pid"], pids["web"]);
        assert_eq!(reason.data["command"], "devlanes stop web --lane demo@0");

        let (session, _frames) = h.session();
        let attach = h
            .call_on(
                Method::Attach,
                json!({"unit": "web", "interactive": true}),
                cx(&main),
                &session,
            )
            .await
            .expect_err("no attach to an orphan");
        assert_eq!(attach.code, "unit_orphaned");

        let why = h
            .call(Method::Why, json!({"unit": "web"}), cx(&main))
            .await
            .expect("why");
        assert_eq!(why["state"], "orphaned");
        assert_eq!(why["reasons"][0]["code"], "orphaned");
        assert_eq!(why["reasons"][0]["data"]["pid"], pids["web"]);

        let stopped = h
            .call(Method::Stop, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("stop");
        let stopped: Page<UnitSummary> = serde_json::from_value(stopped).expect("page");
        assert_eq!(stopped.items.len(), 1);
        assert_eq!(stopped.items[0].name, "web");
        assert_eq!(stopped.items[0].state, "stopped");
        assert!(
            stopped.items[0].orphaned_since_ms.is_some(),
            "named as a terminated orphan"
        );
        assert_eq!(*processes.killed.lock().expect("lock"), [pids["web"]]);
        assert_eq!(unit(&lane(&h, &main).await, "web").state, "idle");
        assert_eq!(
            active_runs(&home)
                .await
                .iter()
                .filter(|u| *u == "web")
                .count(),
            0
        );

        let restarted: Value = h
            .call(Method::Restart, json!({"units": ["worker"]}), cx(&main))
            .await
            .expect("restart");
        assert_eq!(restarted["started"], json!(["demo@0:worker"]));
        assert_eq!(
            *processes.killed.lock().expect("lock"),
            [pids["web"], pids["worker"]]
        );
        let summary = lane(&h, &main).await;
        assert_ne!(unit(&summary, "worker").state, "orphaned");
        assert_ne!(unit(&summary, "worker").pid, Some(pids["worker"]));

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A full lane stop terminates orphans; one whose process exits on its own is
/// cleared by the periodic check.
async fn lane_stop_and_liveness_clear_orphans() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let (h, processes, pids) = crashed(&tmp, &main, &["web", "worker"]).await;

        processes
            .alive
            .lock()
            .expect("lock")
            .remove(&pids["worker"]);
        let cleared = eventually(Duration::from_secs(10), || async {
            (unit(&lane(&h, &main).await, "worker").state == "idle").then_some(())
        })
        .await;
        assert!(cleared.is_some(), "an exited orphan clears");
        assert!(processes.killed.lock().expect("lock").is_empty());

        let stopped = h
            .call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        assert_eq!(stopped["items"][0]["name"], "web");
        assert_eq!(*processes.killed.lock().expect("lock"), [pids["web"]]);
        let summary = lane(&h, &main).await;
        assert_eq!(summary.state, "idle");
        assert!(active_runs(&tmp.path.join("home")).await.is_empty());
        h.close().await;
    })
    .await
    .expect("timeout");
}
