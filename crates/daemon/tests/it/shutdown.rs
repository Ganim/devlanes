//! Daemon shutdown: an explicit one (`daemon stop`, `daemon restart`) runs the
//! `lane.stop` tasks of lanes with live units before their units stop; a signal
//! stops the units without them.

use std::time::Duration;

use devlanes_daemon::{Handler, Shutdown, ShutdownReason};
use devlanes_proto::Method;
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

/// One service and two stop tasks, the first of which fails.
const CONFIG: &str = "project: bye\nservices:\n  api: { run: sleep }\ntasks:\n  first: { run: fail, when: lane.stop }\n  second: { run: echo stop-hook, when: lane.stop }\n";

/// How often `script` was spawned.
fn spawned(journal: &[String], script: &str) -> usize {
    journal
        .iter()
        .filter(|entry| entry.as_str() == script)
        .count()
}

#[tokio::test(flavor = "multi_thread")]
/// With `idle_exit`, the engine goes idle only after a subscriber came and
/// left, and only while nothing runs for the whole grace period.
async fn the_engine_goes_idle_after_the_last_subscriber_with_nothing_running() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(
            &tmp.path,
            "r",
            "project: quiet\nservices:\n  api: { run: sleep }\n",
        );
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                idle_exit: Some(Duration::from_millis(200)),
                ..Setup::default()
            },
        )
        .await;
        let wait = Duration::from_millis(800);
        // Never watched: a daemon only commands used stays.
        assert!(tokio::time::timeout(wait, h.engine.idle()).await.is_err());
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        // A TUI watches while a unit runs, then leaves: still not idle.
        let tui = h.engine.subscribe();
        assert!(tokio::time::timeout(wait, h.engine.idle()).await.is_err());
        drop(tui);
        assert!(tokio::time::timeout(wait, h.engine.idle()).await.is_err());
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        tokio::time::timeout(Duration::from_secs(5), h.engine.idle())
            .await
            .expect("idle once the lane stopped");
        // A subscriber keeps it up again.
        let _watching = h.engine.subscribe();
        assert!(tokio::time::timeout(wait, h.engine.idle()).await.is_err());
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Every stop task runs, a failing one included, and the report names the
/// stopped units and each task's outcome.
async fn explicit_shutdown_runs_lane_stop_tasks() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let report = h
            .engine
            .shutdown(Shutdown::new(ShutdownReason::Explicit))
            .await
            .expect("shutdown");
        assert_eq!(report["reason"], "explicit", "{report}");
        assert_eq!(
            report["stopped"],
            json!([{"lane": "bye@0", "unit": "api"}]),
            "{report}"
        );
        assert_eq!(
            report["tasks"],
            json!([
                {"lane": "bye@0", "task": "first", "exit_code": 1},
                {"lane": "bye@0", "task": "second", "exit_code": 0},
            ]),
            "{report}"
        );
        assert_eq!(spawned(&h.runner.journal.entries(), "echo stop-hook"), 1);
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A signal shutdown stops the units and runs no stop task.
async fn signal_shutdown_skips_lane_stop_tasks() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let report = h
            .engine
            .shutdown(crate::signal_shutdown())
            .await
            .expect("shutdown");
        assert_eq!(report["reason"], "signal", "{report}");
        assert_eq!(
            report["stopped"],
            json!([{"lane": "bye@0", "unit": "api"}]),
            "{report}"
        );
        assert_eq!(report["tasks"], json!([]), "{report}");
        let journal = h.runner.journal.entries();
        assert_eq!(spawned(&journal, "fail"), 0, "{journal:?}");
        assert_eq!(spawned(&journal, "echo stop-hook"), 0, "{journal:?}");
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A lane with nothing running gets no stop tasks at an explicit shutdown.
async fn explicit_shutdown_skips_idle_lanes() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        // A named stop runs no stop task and leaves the lane idle.
        h.call(Method::Stop, json!({"units": ["api"]}), cx(&main))
            .await
            .expect("stop api");
        let report = h
            .engine
            .shutdown(Shutdown::new(ShutdownReason::Explicit))
            .await
            .expect("shutdown");
        assert_eq!(report["stopped"], json!([]), "{report}");
        assert_eq!(report["tasks"], json!([]), "{report}");
        assert_eq!(spawned(&h.runner.journal.entries(), "echo stop-hook"), 0);
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `doctor` lists each service's stop timeout, for the CLI to compare with the
/// service manager's stop window: written, defaulted, and an expression once a
/// run resolved it.
async fn doctor_lists_stop_timeouts() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: slow\nservices:\n  api: { run: sleep, stop: { timeout: 90s } }\n  web: { run: sleep }\n  worker: { run: sleep, autostart: false, stop: { timeout: \"${{ lane.index }}s\" } }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        assert_eq!(
            doctor["stop_timeouts"],
            json!([
                {"project": "slow", "unit": "api", "timeout_ms": 90_000},
                {"project": "slow", "unit": "web", "timeout_ms": 10_000},
            ]),
            "{doctor}"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A signal during an explicit shutdown interrupts the stop task that runs,
/// skips the rest, and the units still stop.
async fn interrupt_ends_lane_stop_tasks() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: bye\nservices:\n  api: { run: sleep }\ntasks:\n  stuck: { run: hold, when: lane.stop }\n  after: { run: echo after, when: lane.stop }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let request = Shutdown::new(ShutdownReason::Explicit);
        let interrupt = request.interrupt.clone();
        let engine = h.engine.clone();
        let shutdown = tokio::spawn(async move { engine.shutdown(request).await });
        let journal = h.runner.journal.clone();
        eventually(Duration::from_secs(10), || {
            let held = spawned(&journal.entries(), "hold") == 1;
            async move { held.then_some(()) }
        })
        .await
        .expect("the stop task runs");
        interrupt.cancel();
        let report: Value = tokio::time::timeout(Duration::from_secs(20), shutdown)
            .await
            .expect("the shutdown ends")
            .expect("join")
            .expect("shutdown");
        assert_eq!(
            report["stopped"],
            json!([{"lane": "bye@0", "unit": "api"}]),
            "{report}"
        );
        assert_eq!(report["tasks"][0]["task"], "stuck", "{report}");
        assert_eq!(report["tasks"][0]["exit_code"], Value::Null, "{report}");
        assert_eq!(report["tasks"][1]["task"], "after", "{report}");
        assert_eq!(report["tasks"][1]["error"]["code"], "cancelled", "{report}");
        assert_eq!(spawned(&h.runner.journal.entries(), "echo after"), 0);
    })
    .await
    .expect("timeout");
}
