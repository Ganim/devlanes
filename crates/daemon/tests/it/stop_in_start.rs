//! `stop <unit>` and `restart <unit>` while a start of the lane runs: the unit
//! leaves that start (never spawned, or stopped and no longer waited for) and is
//! reported as stopped; stopping the task the start runs interrupts the start.

use std::{sync::Arc, time::Duration};

use devlanes_core::{
    clock::TokioClock,
    gate::{StartGate, StartRequest},
};
use devlanes_daemon::Session;
use devlanes_ipc::Frame;
use devlanes_proto::{Context, LaneSummary, Method, Page, Request, RpcError, StartResult};
use devlanes_start_gate::{FixedWindow, GateOptions};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};

use crate::{Harness, LIMIT, Setup, cx, eventually, harness, repo, temp};

/// A request on its own session in the background; keep the frames alive.
fn spawn_call(
    h: &Harness,
    method: Method,
    params: Value,
    context: Context,
) -> (
    Session,
    mpsc::Receiver<Frame>,
    JoinHandle<Result<Value, RpcError>>,
) {
    let (session, frames) = h.session();
    let engine = h.engine.clone();
    let running = session.clone();
    let task = tokio::spawn(async move {
        let request = Request {
            id: 1,
            method,
            params,
            context,
        };
        devlanes_daemon::Handler::handle(&engine, request, running).await
    });
    (session, frames, task)
}

/// The start result of a background start.
async fn start_result(task: JoinHandle<Result<Value, RpcError>>) -> StartResult {
    let value = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the start answers")
        .expect("join")
        .expect("start");
    serde_json::from_value(value).expect("start result")
}

/// The state and reason code of `unit` in a start result.
fn reported(result: &StartResult, unit: &str) -> Option<(String, Option<String>)> {
    result
        .units
        .iter()
        .find(|summary| summary.name == unit)
        .map(|summary| {
            (
                summary.state.clone(),
                summary.reason.as_ref().map(|reason| reason.code.clone()),
            )
        })
}

/// The lane visible at `cwd`.
async fn lane(h: &Harness, cwd: &std::path::Path) -> LaneSummary {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items.into_iter().next().expect("lane")
}

/// How many spawned scripts start with `prefix`.
fn spawned(h: &Harness, prefix: &str) -> usize {
    h.runner
        .journal
        .entries()
        .iter()
        .filter(|script| script.starts_with(prefix))
        .count()
}

/// Stops `units` and fails unless the stop answers within a second.
async fn quick_stop(h: &Harness, units: &[&str], context: Context) -> Value {
    tokio::time::timeout(
        Duration::from_secs(1),
        h.call(Method::Stop, json!({"units": units}), context),
    )
    .await
    .expect("the stop does not wait for the start")
    .expect("stop")
}

const THREE: &str = r#"project: paced
services:
  a: { run: "echo a; sleep", ready: started }
  b: { run: "echo b; sleep", ready: started }
  c: { run: "echo c; sleep", ready: started }
"#;

#[tokio::test(flavor = "multi_thread")]
/// A unit queued in the start gate leaves the start: it never spawns, the start
/// goes on with the others and reports it as stopped; a later start runs it.
async fn a_queued_unit_leaves_the_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", THREE);
        let gate = Arc::new(FixedWindow::new(
            1,
            GateOptions::default(),
            Arc::new(TokioClock::new()),
        ));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                ..Setup::default()
            },
        )
        .await;
        let holder = gate
            .acquire(StartRequest {
                lane: "elsewhere@0".into(),
                unit: "holder".into(),
                order: 0,
                memory_need: 0,
            })
            .await;
        let (_session, _frames, starting) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 10_000}), cx(&main));
        let queued = eventually(Duration::from_secs(5), || async {
            (gate.snapshot().queued.len() == 3).then_some(())
        })
        .await;
        assert!(queued.is_some(), "a, b and c wait for a slot");

        let stopped = quick_stop(&h, &["b"], cx(&main)).await;
        assert_eq!(stopped["items"][0]["name"], "b", "{stopped}");
        assert_eq!(stopped["items"][0]["state"], "stopped");
        drop(holder);
        let result = start_result(starting).await;
        assert_eq!(result.started, ["paced@0:a", "paced@0:c"], "{result:?}");
        assert!(result.ready && result.failures.is_empty(), "{result:?}");
        assert_eq!(
            reported(&result, "b"),
            Some(("stopped".into(), Some("cancelled_by_stop".into())))
        );
        assert_eq!(spawned(&h, "echo b"), 0);

        let again: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({"units": ["b"]}), cx(&main))
                .await
                .expect("start b"),
        )
        .expect("result");
        assert_eq!(again.started, ["paced@0:b"]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

const SLOW: &str = r#"project: slow
services:
  a: { run: "echo a; sleep", ready: started }
  slow: { run: "echo slow; sleep", ready: { log: never } }
"#;

#[tokio::test(flavor = "multi_thread")]
/// A spawned unit the start waits for stops at once, and the start stops
/// waiting for it.
async fn a_starting_unit_stops_and_the_start_moves_on() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", SLOW);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let (_session, _frames, starting) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 30_000}), cx(&main));
        let up = eventually(Duration::from_secs(5), || async {
            (spawned(&h, "echo slow") == 1).then_some(())
        })
        .await;
        assert!(up.is_some(), "slow spawned");

        let stopped = quick_stop(&h, &["slow"], cx(&main)).await;
        assert_eq!(stopped["items"][0]["name"], "slow", "{stopped}");
        let result = start_result(starting).await;
        assert_eq!(result.started, ["slow@0:a"], "{result:?}");
        assert!(result.ready && !result.timed_out, "{result:?}");
        assert_eq!(
            reported(&result, "slow"),
            Some(("stopped".into(), Some("cancelled_by_stop".into())))
        );
        let summary = lane(&h, &main).await;
        let slow = summary
            .units
            .iter()
            .find(|u| u.name == "slow")
            .expect("slow");
        assert!(
            !matches!(slow.state.as_str(), "starting" | "ready"),
            "{slow:?}"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

const TASKED: &str = r#"project: tasked
profiles:
  default: [web, worker]
  slow: [web, worker]
tasks:
  pause: { run: "nap 1500", when: lane.start }
  hang: { run: sleep, when: lane.start, profiles: [slow] }
services:
  web: { run: "echo web; sleep", ready: started }
  worker: { run: "echo worker; sleep", ready: started }
"#;

#[tokio::test(flavor = "multi_thread")]
/// A service stopped while the start runs its tasks is skipped by the services
/// phase; the start itself goes on.
async fn a_service_stopped_before_the_services_phase_is_skipped() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", TASKED);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let (_session, _frames, starting) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 10_000}), cx(&main));
        let napping = eventually(Duration::from_secs(5), || async {
            (spawned(&h, "nap") == 1).then_some(())
        })
        .await;
        assert!(napping.is_some(), "the task runs");

        let stopped = quick_stop(&h, &["web"], cx(&main)).await;
        assert_eq!(stopped["items"][0]["state"], "stopped", "{stopped}");
        let result = start_result(starting).await;
        assert_eq!(result.started, ["tasked@0:worker"], "{result:?}");
        assert!(result.ready, "{result:?}");
        assert_eq!(
            reported(&result, "web"),
            Some(("stopped".into(), Some("cancelled_by_stop".into())))
        );
        assert_eq!(spawned(&h, "echo web"), 0);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Stopping the task a start runs stops it and interrupts that start, as a
/// whole-lane stop does; the next start is unaffected.
async fn stopping_the_running_task_interrupts_the_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", TASKED);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let (_session, _frames, starting) = spawn_call(
            &h,
            Method::Start,
            json!({"profiles": ["slow"], "timeout_ms": 60_000}),
            cx(&main),
        );
        // The runner journals the spawn before the lane records the task, and
        // only a recorded task can be stopped by name.
        let hung = eventually(Duration::from_secs(5), || async {
            (h.runner.journal.entries() == ["nap 1500", "sleep"]
                && lane(&h, &main).await.units.iter().any(|u| u.name == "hang"))
            .then_some(())
        })
        .await;
        assert!(
            hung.is_some(),
            "hang runs: {:?}",
            h.runner.journal.entries()
        );

        quick_stop(&h, &["hang"], cx(&main)).await;
        let error = tokio::time::timeout(Duration::from_secs(5), starting)
            .await
            .expect("the start answers")
            .expect("join")
            .expect_err("interrupted");
        assert_eq!(error.code, "start_cancelled", "{error:?}");
        assert_eq!(spawned(&h, "echo"), 0, "no service spawned");

        let next: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({"profiles": ["default"]}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("result");
        assert!(next.ready, "{next:?}");
        assert_eq!(next.started.len(), 2);
        h.close().await;
    })
    .await
    .expect("timeout");
}

const AGAIN: &str = r#"project: again
tasks:
  pause: { run: "nap 1000", when: lane.start }
  seed: { run: "nap 3000", when: lane.start, after: services }
services:
  a: { run: "echo a; sleep", ready: started }
  slow: { run: "echo slow; sleep", ready: started }
"#;

#[tokio::test(flavor = "multi_thread")]
/// `restart` of a unit in a running start takes it out of that start and starts
/// it fresh once the start's services phase is over, while the start's later
/// tasks still run.
async fn restart_takes_a_unit_out_of_a_running_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", AGAIN);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let (_session, _frames, starting) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 20_000}), cx(&main));
        let napping = eventually(Duration::from_secs(5), || async {
            (spawned(&h, "nap 1000") == 1).then_some(())
        })
        .await;
        assert!(napping.is_some(), "the start runs its first task");

        let asked = Instant::now();
        let restarted: StartResult = serde_json::from_value(
            tokio::time::timeout(
                Duration::from_secs(5),
                h.call(
                    Method::Restart,
                    json!({"units": ["slow"], "timeout_ms": 2000}),
                    cx(&main),
                ),
            )
            .await
            .expect("the restart does not wait for the whole start")
            .expect("restart"),
        )
        .expect("result");
        assert_eq!(restarted.started, ["again@0:slow"], "{restarted:?}");
        assert!(restarted.ready, "{restarted:?}");
        assert!(
            !starting.is_finished(),
            "the restart answered while the start's after-services task ran"
        );
        assert!(asked.elapsed() < Duration::from_secs(3));

        let result = start_result(starting).await;
        assert_eq!(result.started, ["again@0:a"], "{result:?}");
        assert_eq!(
            reported(&result, "slow"),
            Some(("stopped".into(), Some("cancelled_by_stop".into())))
        );
        // Spawned once: by the restart, never by the start.
        assert_eq!(spawned(&h, "echo slow"), 1);
        let summary = lane(&h, &main).await;
        let slow = summary
            .units
            .iter()
            .find(|u| u.name == "slow")
            .expect("slow");
        assert_eq!(slow.state, "ready", "{slow:?}");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `restart` of a unit the start already spawned and waits for: stopped in the
/// start, then started fresh.
async fn restart_replaces_a_unit_the_start_waits_for() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", SLOW);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let (_session, _frames, starting) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 30_000}), cx(&main));
        let up = eventually(Duration::from_secs(5), || async {
            (spawned(&h, "echo slow") == 1).then_some(())
        })
        .await;
        assert!(up.is_some(), "slow spawned");

        let restarted: StartResult = serde_json::from_value(
            tokio::time::timeout(
                Duration::from_secs(5),
                h.call(
                    Method::Restart,
                    json!({"units": ["slow"], "timeout_ms": 300}),
                    cx(&main),
                ),
            )
            .await
            .expect("the restart does not wait for the start's readiness")
            .expect("restart"),
        )
        .expect("result");
        assert_eq!(restarted.started, ["slow@0:slow"], "{restarted:?}");
        let result = start_result(starting).await;
        assert_eq!(result.started, ["slow@0:a"], "{result:?}");
        assert_eq!(
            reported(&result, "slow"),
            Some(("stopped".into(), Some("cancelled_by_stop".into())))
        );
        assert_eq!(spawned(&h, "echo slow"), 2);
        h.close().await;
    })
    .await
    .expect("timeout");
}
