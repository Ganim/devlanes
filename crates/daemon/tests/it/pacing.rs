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

const THREE: &str = r#"project: paced
services:
  a: { run: "echo a; sleep", ready: started }
  b: { run: "echo b; sleep", ready: started }
  c: { run: "echo c; sleep", ready: started }
"#;

const SOLO: &str = r#"project: paced
services:
  solo: { run: "echo solo; sleep", ready: started }
"#;

/// Reports every sampled unit's leader with a fixed CPU and footprint, so slot
/// holders can measure settling and startup peaks.
struct FakeMetrics {
    cpu_percent: f64,
    footprint_bytes: u64,
}

#[async_trait::async_trait]
impl devlanes_metrics::MetricsSource for FakeMetrics {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn sample(
        &self,
        request: &devlanes_metrics::SampleRequest,
    ) -> Result<Vec<devlanes_metrics::Sample>, devlanes_metrics::Error> {
        let ts_ms = devlanes_metrics::unix_ms(std::time::SystemTime::now());
        Ok(request
            .units
            .iter()
            .filter_map(|target| {
                let leader = target.processes.first()?;
                Some(devlanes_metrics::Sample {
                    subject: devlanes_metrics::Subject::Process {
                        unit: target.unit.clone(),
                        pid: leader.pid,
                        start_time: leader.start_time,
                    },
                    cpu_percent: self.cpu_percent,
                    footprint_bytes: self.footprint_bytes,
                    rss_bytes: self.footprint_bytes,
                    ts_ms,
                })
            })
            .collect())
    }
}

/// Metrics that never let a `ready: started` unit settle, so it holds its
/// slot until the slot timeout.
pub(crate) fn busy_metrics() -> Vec<Arc<dyn devlanes_metrics::MetricsSource>> {
    vec![Arc::new(FakeMetrics {
        cpu_percent: 50.0,
        footprint_bytes: 64 << 20,
    })]
}

/// A fixed gate on the real clock with a short slot timeout.
pub(crate) fn gate(max: usize, slot_timeout: Duration) -> Arc<FixedWindow> {
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
                metrics: busy_metrics(),
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
        let main = repo(&tmp.path, "r", THREE);
        let gate = gate(1, Duration::from_secs(30));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                metrics: busy_metrics(),
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
        assert_eq!(started.started, ["paced@0:a", "paced@0:b", "paced@0:c"]);

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
        // Further back, `why` names the start everyone waits behind.
        let why = h
            .call(Method::Why, json!({"unit": "c"}), cx(&main))
            .await
            .expect("why c");
        let queued = why["reasons"]
            .as_array()
            .and_then(|reasons| reasons.iter().find(|r| r["code"] == "queued"))
            .cloned()
            .expect("queued reason for c");
        assert_eq!(queued["data"]["position"], 2);
        assert_eq!(queued["data"]["hold"], "window_full");
        assert_eq!(queued["data"]["head"]["unit"], "b");
        assert_eq!(queued["data"]["head"]["lane"], "paced@0");

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        assert!(gate.snapshot().queued.is_empty(), "stop withdrew b and c");
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

#[tokio::test(flavor = "multi_thread")]
/// Without process metrics, settling cannot be measured: `ready: started`
/// units give their slot back when they start instead of at the slot timeout.
async fn started_units_without_metrics_release_at_once() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", THREE);
        let gate = gate(1, Duration::from_secs(30));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                ..Setup::default()
            },
        )
        .await;
        // Held slots would keep `b` queued for the 30 s slot timeout, so any
        // start timeout below that tells the two apart; loaded runners have
        // taken 2 s per spawn.
        let started: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({"timeout_ms": 20_000}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("start result");
        assert!(started.ready && !started.timed_out, "{started:?}");
        assert_eq!(started.started.len(), 3);
        let drained = eventually(Duration::from_secs(10), || async {
            (gate.snapshot().in_flight == 0).then_some(())
        })
        .await;
        assert!(drained.is_some());
        h.close().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
/// A `ready: started` unit releases its slot once its CPU settles, and the
/// footprint seen while it started becomes its memory need next time.
async fn settled_startup_peak_becomes_the_memory_need() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", SOLO);
        let gate = gate(1, Duration::from_secs(30));
        let footprint: u64 = 300 << 20;
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                metrics: vec![Arc::new(FakeMetrics {
                    cpu_percent: 1.0,
                    footprint_bytes: footprint,
                })],
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({"timeout_ms": 5000}), cx(&main))
            .await
            .expect("start");
        let settled = eventually(Duration::from_secs(15), || async {
            (gate.snapshot().in_flight == 0).then_some(())
        })
        .await;
        assert!(settled.is_some(), "slot not released by settling");
        // Hold the slot so the restart has to queue and show its need.
        let holder = gate
            .acquire(StartRequest {
                lane: "elsewhere@0".into(),
                unit: "holder".into(),
                order: 0,
                memory_need: 0,
            })
            .await;
        let need = eventually(Duration::from_secs(5), || async {
            h.call(
                Method::Restart,
                json!({"units": ["solo"], "timeout_ms": 200}),
                cx(&main),
            )
            .await
            .ok()?;
            let lanes = status(&h, cx(&main)).await;
            lanes
                .iter()
                .flat_map(|lane| &lane.units)
                .find(|unit| unit.name == "solo")
                .and_then(|unit| unit.reason.clone())
                .map(|reason| reason.data["memory_need_bytes"].clone())
        })
        .await;
        assert_eq!(need, Some(json!(footprint)));
        drop(holder);
        h.close().await;
    })
    .await
    .expect("test timed out");
}
