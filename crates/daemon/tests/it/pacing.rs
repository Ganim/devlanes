//! Startup pacing through the engine: the start gate's window across lanes,
//! declaration order, `queued` in status and why, stop withdrawing queued
//! starts, `all_at_once` and restarts.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use devlanes_core::{
    clock::TokioClock,
    gate::{StartGate, StartRequest},
};
use devlanes_proto::{LaneSummary, Method, Page, StartResult};
use devlanes_start_gate::{FixedWindow, GateOptions};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp, worktree};

/// Five `ready: started` services that stay up; `run` names the lane so the
/// runner's journal shows who spawned.
const FIVE: &str = r#"project: paced
services:
  s1: { run: "echo s1@${{ lane.index }}; sleep", ready: started }
  s2: { run: "echo s2@${{ lane.index }}; sleep", ready: started }
  s3: { run: "echo s3@${{ lane.index }}; sleep", ready: started }
  s4: { run: "echo s4@${{ lane.index }}; sleep", ready: started }
  s5: { run: "echo s5@${{ lane.index }}; sleep", ready: started }
"#;

const TWO: &str = r#"project: paced
services:
  a: { run: "echo a; sleep", ready: started }
  b: { run: "echo b; sleep", ready: started }
"#;

/// A fixed gate on the real clock with a short slot timeout.
fn gate(max: usize, slot_timeout: Duration) -> Arc<FixedWindow> {
    Arc::new(FixedWindow::new(
        max,
        GateOptions {
            slot_timeout,
            ..GateOptions::default()
        },
        Arc::new(TokioClock::new()),
    ))
}

/// The lane summaries visible from the context.
async fn status(h: &crate::Harness, context: devlanes_proto::Context) -> Vec<LaneSummary> {
    let value = h
        .call(Method::Status, json!({}), context)
        .await
        .expect("status");
    serde_json::from_value::<Page<LaneSummary>>(value)
        .expect("page")
        .items
}

/// State word of `unit` in the first lane summary.
fn state_of(lanes: &[LaneSummary], unit: &str) -> Option<String> {
    lanes
        .iter()
        .flat_map(|lane| &lane.units)
        .find(|summary| summary.name == unit)
        .map(|summary| summary.state.clone())
}

#[tokio::test(flavor = "multi_thread")]
/// Ten units over two lanes behind a window of 3: never more than 3 in flight,
/// and each lane spawns in declaration order.
async fn window_of_three_across_two_lanes() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", FIVE);
        let other = tmp.path.join("wt");
        worktree(&main, &other, "feat/other");
        let gate = gate(3, Duration::from_millis(300));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        let done = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicUsize::new(0));
        let monitor = {
            let (gate, done, peak) = (gate.clone(), done.clone(), peak.clone());
            tokio::spawn(async move {
                while !done.load(Ordering::SeqCst) {
                    peak.fetch_max(gate.snapshot().in_flight, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
        };
        let (first, second) = tokio::join!(
            h.call(Method::Start, json!({}), cx(&main)),
            h.call(Method::Start, json!({}), cx(&other)),
        );
        for result in [first, second] {
            let result: StartResult =
                serde_json::from_value(result.expect("start")).expect("start result");
            assert_eq!(result.started.len(), 5, "{result:?}");
            assert!(!result.timed_out);
        }
        // The last slots drain by timeout.
        let drained = eventually(Duration::from_secs(5), || async {
            (gate.snapshot().in_flight == 0).then_some(())
        })
        .await;
        done.store(true, Ordering::SeqCst);
        monitor.await.expect("monitor");
        assert!(drained.is_some(), "slots never drained");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            3,
            "window not used or exceeded"
        );
        let journal = h.runner.journal.entries();
        assert_eq!(journal.len(), 10, "{journal:?}");
        for lane in ["@0", "@1"] {
            let order: Vec<&str> = journal
                .iter()
                .filter_map(|script| script.split(';').next())
                .filter_map(|echo| echo.strip_prefix("echo "))
                .filter(|unit| unit.ends_with(lane))
                .collect();
            let expected: Vec<String> = (1..=5).map(|i| format!("s{i}{lane}")).collect();
            assert_eq!(order, expected, "lane {lane}");
        }
        h.close().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
/// A unit waiting for a slot is `queued` in status and why, `stop` withdraws
/// it, `all_at_once` bypasses a full gate, and a restart queues like a start.
async fn queued_units_stop_bypass_and_restart() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", TWO);
        let gate = gate(1, Duration::from_secs(30));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                ..Setup::default()
            },
        )
        .await;
        let started: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({"timeout_ms": 400}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("start result");
        assert!(started.timed_out, "b is still waiting: {started:?}");
        assert_eq!(started.started, ["paced@0:a", "paced@0:b"]);

        let lanes = status(&h, cx(&main)).await;
        assert_eq!(state_of(&lanes, "a").as_deref(), Some("ready"));
        assert_eq!(state_of(&lanes, "b").as_deref(), Some("queued"));
        let reason = lanes[0]
            .units
            .iter()
            .find(|unit| unit.name == "b")
            .and_then(|unit| unit.reason.clone())
            .expect("queued reason");
        assert_eq!(reason.code, "queued");
        assert_eq!(reason.data["position"], 1);
        assert_eq!(reason.data["in_flight"], 1);
        assert_eq!(reason.data["window"], 1);
        assert_eq!(reason.data["hold"], "window_full");
        // No run history yet: the default need.
        assert_eq!(
            reason.data["memory_need_bytes"],
            devlanes_core::gate::DEFAULT_MEMORY_NEED
        );
        let why = h
            .call(Method::Why, json!({"unit": "b"}), cx(&main))
            .await
            .expect("why");
        assert_eq!(why["state"], "queued");
        assert!(
            why["reasons"]
                .as_array()
                .is_some_and(|reasons| reasons.iter().any(|r| r["code"] == "queued")),
            "{why}"
        );

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        assert!(gate.snapshot().queued.is_empty(), "stop withdrew b");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let spawned = h.runner.journal.entries();
        assert_eq!(spawned, ["echo a; sleep"], "b never spawned");

        // Hold the only slot so a gated start could not proceed.
        let holder = gate
            .acquire(StartRequest {
                lane: "elsewhere@0".into(),
                unit: "holder".into(),
                order: 0,
                memory_need: 0,
            })
            .await;
        let bypass: StartResult = serde_json::from_value(
            h.call(
                Method::Start,
                json!({"all_at_once": true, "timeout_ms": 5000}),
                cx(&main),
            )
            .await
            .expect("bypass start"),
        )
        .expect("start result");
        assert!(bypass.ready && !bypass.timed_out, "{bypass:?}");
        assert_eq!(gate.snapshot().in_flight, 1, "bypass took no slot");

        let restart: Value = h
            .call(
                Method::Restart,
                json!({"units": ["a"], "timeout_ms": 300}),
                cx(&main),
            )
            .await
            .expect("restart");
        assert_eq!(restart["timed_out"], true, "{restart}");
        assert_eq!(
            state_of(&status(&h, cx(&main)).await, "a").as_deref(),
            Some("queued")
        );
        drop(holder);
        let ready = eventually(Duration::from_secs(5), || async {
            (state_of(&status(&h, cx(&main)).await, "a").as_deref() == Some("ready")).then_some(())
        })
        .await;
        assert!(ready.is_some(), "restart spawned after the slot freed");
        h.close().await;
    })
    .await
    .expect("test timed out");
}
