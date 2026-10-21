//! Lane lifecycle edges: interrupting a hung start, `run --timeout`, `lane
//! clean` against late or stubborn units, concurrent `resume`, a stopping
//! daemon, and the start gate's arrival order.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use devlanes_core::{
    clock::TokioClock,
    gate::{StartGate, StartRequest},
};
use devlanes_daemon::{Handler, Session};
use devlanes_ipc::Frame;
use devlanes_proto::{
    Context, Control, Event, LaneSummary, Method, Page, Request, RpcError, StartResult, events,
};
use devlanes_start_gate::{FixedWindow, GateOptions};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    FakeProcesses, Harness, LIMIT, Setup, cx, drain_events, eventually, harness, repo, temp,
    worktree,
};

/// Runs a request on its own session in the background. Keep the frames: a
/// dropped receiver reads as a client that left.
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
        engine.handle(request, running).await
    });
    (session, frames, task)
}

/// The next event on `frames`; panics after five seconds without one.
async fn next_event(frames: &mut mpsc::Receiver<Frame>) -> Event {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), frames.recv()).await {
            Ok(Some(Frame::Control(Control::Event(event)))) => return event,
            Ok(Some(_)) => {}
            _ => panic!("no event"),
        }
    }
}

/// The lane visible at `cwd`.
async fn lane(h: &Harness, cwd: &Path) -> LaneSummary {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items.into_iter().next().expect("lane")
}

/// Units of `summary` whose process runs.
fn alive(summary: &LaneSummary) -> Vec<String> {
    summary
        .units
        .iter()
        .filter(|u| {
            matches!(
                u.state.as_str(),
                "starting" | "ready" | "running" | "stopping"
            )
        })
        .map(|u| u.name.clone())
        .collect()
}

/// A fixed gate on the real clock.
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

/// A `task` event of `task` in `state`, from a bus subscription.
fn task_event(
    bus: &mut tokio::sync::broadcast::Receiver<Event>,
    task: &str,
    state: &str,
) -> Option<Event> {
    std::iter::from_fn(|| bus.try_recv().ok())
        .find(|e| e.event == events::TASK && e.data["task"] == task && e.data["state"] == state)
}

const HANG: &str = r#"project: hang
profiles:
  default: [web]
  slow: [web]
tasks:
  hang: { run: sleep, when: lane.start, profiles: [slow] }
services:
  web: { run: sleep, ready: started }
"#;

#[tokio::test(flavor = "multi_thread")]
/// A whole-lane stop interrupts a start hung in a task: the task's unit stops,
/// the start answers `start_cancelled`, and a start after the stop is untouched.
async fn stop_interrupts_a_hung_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", HANG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let mut bus = h.engine.subscribe();
        let (_session, _frames, starting) = spawn_call(
            &h,
            Method::Start,
            json!({"profiles": ["slow"], "timeout_ms": 60_000}),
            cx(&main),
        );
        let hung = eventually(Duration::from_secs(5), || async {
            (h.runner.journal.entries() == ["sleep"]).then_some(())
        })
        .await;
        assert!(hung.is_some(), "the task runs");

        tokio::time::timeout(
            Duration::from_secs(10),
            h.call(Method::Stop, json!({}), cx(&main)),
        )
        .await
        .expect("the stop is not held up by the start")
        .expect("stop");
        let error = starting.await.expect("join").expect_err("interrupted");
        assert_eq!(error.code, "start_cancelled", "{error:?}");
        assert!(alive(&lane(&h, &main).await).is_empty());
        assert_eq!(h.runner.journal.entries(), ["sleep"], "web never spawned");
        let closed = task_event(&mut bus, "hang", "failed").expect("closing task event");
        assert_eq!(closed.data["reason"]["code"], "cancelled");

        let started: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({"profiles": ["default"]}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("result");
        assert!(started.ready, "{started:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(alive(&lane(&h, &main).await), ["web"]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `run --timeout` stops the task's unit, finishes its run (so the lane can be
/// cleaned) and emits the closing `task` event before answering `timeout`.
async fn run_timeout_stops_the_task_and_closes_its_run() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: slow\ntasks:\n  slow: { run: sleep }\nservices:\n  web: { run: sleep, ready: started }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let mut bus = h.engine.subscribe();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            h.call(Method::Run, json!({"task": "slow", "timeout_ms": 300}), cx(&main)),
        )
        .await
        .expect("answers after the limit")
        .expect_err("timeout");
        assert_eq!((error.code.as_str(), error.exit_code), ("timeout", 124));
        assert!(alive(&lane(&h, &main).await).is_empty(), "the task unit stopped");
        let closed = task_event(&mut bus, "slow", "failed").expect("closing task event");
        assert_eq!(closed.data["reason"]["code"], "cancelled");
        let cleaned = h
            .call(
                Method::LaneClean,
                json!({}),
                Context {
                    yes: true,
                    ..cx(&main)
                },
            )
            .await
            .expect("no active run blocks the clean");
        assert_eq!(cleaned["lane"], "slow@0");
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// Units that hold their start slot until the gate's slot timeout.
const QUEUED: &str = r#"project: paced
services:
  a: { run: "echo a; sleep", ready: { log: never } }
  b: { run: "echo b; sleep", ready: { log: never } }
  c: { run: "echo c; sleep", ready: { log: never } }
"#;

#[tokio::test(flavor = "multi_thread")]
/// A unit a timed-out start left queued spawns after that start returned;
/// `lane clean` stops it with the rest, withdraws what is still queued, and
/// leaves no live unit behind.
async fn clean_stops_units_a_timed_out_start_left_queued() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", QUEUED);
        let gate = gate(1, Duration::from_millis(500));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate.clone()),
                ..Setup::default()
            },
        )
        .await;
        let started: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({"timeout_ms": 200}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("result");
        assert!(started.timed_out, "{started:?}");
        let late = eventually(Duration::from_secs(5), || async {
            (h.runner.journal.entries().len() == 2).then_some(())
        })
        .await;
        assert!(late.is_some(), "b spawned after the start returned");

        let cleaned = h
            .call(
                Method::LaneClean,
                json!({}),
                Context {
                    yes: true,
                    ..cx(&main)
                },
            )
            .await
            .expect("clean");
        let stopped: BTreeSet<&str> = cleaned["stopped"]
            .as_array()
            .expect("stopped")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(stopped, BTreeSet::from(["a", "b"]), "{cleaned}");
        assert_eq!(h.engine.managed_processes(), 0);
        // c would have had its slot by now.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            h.runner.journal.entries(),
            ["echo a; sleep", "echo b; sleep"]
        );
        assert_eq!(h.engine.managed_processes(), 0);
        assert!(gate.snapshot().queued.is_empty());
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A unit whose stop timed out still runs: `lane clean` refuses with
/// `lane_running` and keeps the lane's records instead of losing the unit.
async fn clean_refuses_while_a_unit_would_not_stop() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: stub\nservices:\n  web: { run: \"stubborn; sleep\", ready: started, stop: { timeout: 0s } }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        // A live identity, so the overdue stop finds the "process" still there.
        let me = devlanes_sys::identity(std::process::id())
            .expect("process table")
            .expect("own identity");
        *h.runner.stubborn_identity.lock().expect("lock") = Some(me);
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let yes = Context {
            yes: true,
            ..cx(&main)
        };
        let refused = h
            .call(Method::LaneClean, json!({}), yes.clone())
            .await
            .expect_err("web outlived its stop");
        assert_eq!(refused.code, "lane_running", "{refused:?}");
        assert_eq!(refused.data["units"], json!(["web"]));
        let summary = lane(&h, &main).await;
        assert_eq!(summary.reference, "stub@0", "the lane keeps its records");
        assert_eq!(alive(&summary), ["web"]);

        // The second request ends it; then the clean goes through.
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.call(Method::LaneClean, json!({}), yes)
            .await
            .expect("clean");
        assert_eq!(h.engine.managed_processes(), 0);
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// Two services per lane that hold their slot until the slot timeout.
const RESUMED: &str = r#"project: resumed
services:
  a: { run: "echo a@${{ lane.index }}; sleep", ready: { log: never } }
  b: { run: "echo b@${{ lane.index }}; sleep", ready: { log: never } }
"#;

/// Two lanes started by a first daemon that then shut down; returns a second
/// daemon on the same home (with `gate`) and both worktrees.
async fn restarted(tmp: &crate::Temp, gate: Arc<FixedWindow>) -> (Harness, PathBuf, PathBuf) {
    let main = repo(&tmp.path, "r", RESUMED);
    let other = tmp.path.join("wt");
    worktree(&main, &other, "feat/other");
    let home = tmp.path.join("home");
    let first = harness(&home, Setup::default()).await;
    for dir in [&main, &other] {
        first
            .call(Method::Start, json!({"timeout_ms": 200}), cx(dir))
            .await
            .expect("start");
    }
    first.close().await;
    drop(first);
    let second = harness(
        &home,
        Setup {
            gate: Some(gate),
            logs: Some(tmp.path.join("logs-2")),
            ..Setup::default()
        },
    )
    .await;
    (second, main, other)
}

#[tokio::test(flavor = "multi_thread")]
/// `resume` starts its lanes together, so their units queue in the start gate
/// at once, and streams each lane's start under its own `start_id`.
async fn resume_starts_lanes_together_and_streams_each_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let gate = gate(1, Duration::from_millis(300));
        let (h, main, _other) = restarted(&tmp, gate.clone()).await;
        let monitor = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let mut most = 0;
                for _ in 0..1000 {
                    let lanes: BTreeSet<String> = gate
                        .snapshot()
                        .queued
                        .iter()
                        .map(|entry| entry.request.lane.clone())
                        .collect();
                    most = most.max(lanes.len());
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                most
            })
        };
        let (session, mut frames) = h.session();
        let resumed = h
            .call_on(
                Method::Resume,
                json!({"timeout_ms": 1500, "events": true}),
                cx(&main),
                &session,
            )
            .await
            .expect("resume");
        let items = resumed["items"].as_array().expect("items");
        assert_eq!(items.len(), 2, "{resumed}");
        assert_eq!(
            monitor.await.expect("monitor"),
            2,
            "both lanes queued together"
        );

        let streamed = drain_events(&mut frames);
        let mut ids = BTreeSet::new();
        for item in items {
            let lane = item["lane"].as_str().expect("lane");
            let id = item["start_id"].as_u64().expect("start id");
            ids.insert(id);
            let own: BTreeSet<u64> = streamed
                .iter()
                .filter(|e| e.event == events::PHASE && e.data["lane"] == lane)
                .filter_map(|e| e.data["start_id"].as_u64())
                .collect();
            assert_eq!(own, BTreeSet::from([id]), "{lane}");
        }
        assert_eq!(ids.len(), 2, "each lane's start has its own id");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `resume --all-at-once` bypasses the start gate like `start --all-at-once`;
/// without a shell env it refuses with `needs_env`, exit 1 like `start`.
async fn resume_all_at_once_bypasses_the_gate() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let gate = gate(1, Duration::from_secs(30));
        let (h, main, _other) = restarted(&tmp, gate.clone()).await;
        let no_env = h
            .call(
                Method::Resume,
                json!({}),
                Context {
                    env: None,
                    ..cx(&main)
                },
            )
            .await
            .expect_err("needs env");
        assert_eq!((no_env.code.as_str(), no_env.exit_code), ("needs_env", 1));

        let holder = gate
            .acquire(StartRequest {
                lane: "elsewhere@0".into(),
                unit: "holder".into(),
                order: 0,
                memory_need: 0,
            })
            .await;
        h.call(
            Method::Resume,
            json!({"timeout_ms": 500, "all_at_once": true}),
            cx(&main),
        )
        .await
        .expect("resume");
        let mut spawned: Vec<String> = h
            .runner
            .journal
            .entries()
            .into_iter()
            .filter_map(|script| script.split(';').next().map(str::to_owned))
            .collect();
        spawned.sort();
        assert_eq!(spawned, ["echo a@0", "echo a@1", "echo b@0", "echo b@1"]);
        assert_eq!(gate.snapshot().in_flight, 1, "only the holder's slot");
        drop(holder);
        h.close().await;
    })
    .await
    .expect("timeout");
}

const ORDER: &str = r#"project: order
profiles:
  default: [web]
  slow: [web]
tasks:
  pause: { run: "nap 800", when: lane.start, profiles: [slow] }
services:
  web: { run: "echo web@${{ lane.index }}; sleep", ready: started }
"#;

#[tokio::test(flavor = "multi_thread")]
/// The start gate serves starts by request arrival: a start that arrived first
/// keeps its place through a slow task phase.
async fn the_gate_serves_starts_by_arrival() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", ORDER);
        let other = tmp.path.join("wt");
        worktree(&main, &other, "feat/other");
        let gate = gate(1, Duration::from_secs(30));
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
            .expect("lanes");
        let holder = gate
            .acquire(StartRequest {
                lane: "elsewhere@0".into(),
                unit: "holder".into(),
                order: 0,
                memory_need: 0,
            })
            .await;
        let (_s1, _f1, first) = spawn_call(
            &h,
            Method::Start,
            json!({"profiles": ["slow"], "timeout_ms": 10_000}),
            cx(&main),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_s2, _f2, second) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 10_000}), cx(&other));
        let both = eventually(Duration::from_secs(5), || async {
            (gate.snapshot().queued.len() == 2).then_some(())
        })
        .await;
        assert!(both.is_some(), "both starts queued");
        drop(holder);
        let first: StartResult =
            serde_json::from_value(first.await.expect("join").expect("first")).expect("result");
        let second: StartResult =
            serde_json::from_value(second.await.expect("join").expect("second")).expect("result");
        let webs: Vec<String> = h
            .runner
            .journal
            .entries()
            .into_iter()
            .filter_map(|script| {
                let echo = script
                    .split(';')
                    .next()?
                    .strip_prefix("echo web@")?
                    .to_owned();
                Some(format!("order@{echo}"))
            })
            .collect();
        assert_eq!(webs, [first.lane, second.lane]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Once shutdown began, methods that would start work answer `daemon_stopping`.
async fn a_stopping_daemon_takes_no_new_work() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", HANG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.close().await;
        for method in [Method::Start, Method::Exec, Method::Run, Method::LaneClean] {
            let error = h
                .call(method, json!({}), cx(&main))
                .await
                .expect_err("refused");
            assert_eq!(
                (error.code.as_str(), error.exit_code),
                ("daemon_stopping", 4),
                "{error:?}"
            );
        }
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// The `status --watch` snapshot has a sequence number of its own, below the
/// events that follow it.
async fn the_watch_snapshot_has_its_own_seq() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", HANG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Status, json!({}), cx(&main))
            .await
            .expect("register");
        let (session, mut frames, watching) =
            spawn_call(&h, Method::Status, json!({"watch": true}), cx(&main));
        let snapshot = next_event(&mut frames).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let change = next_event(&mut frames).await;
        assert!(
            change.seq > snapshot.seq,
            "snapshot {} then {}",
            snapshot.seq,
            change.seq
        );
        session.close();
        let _ = watching.await;
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `exec --name` refuses the names of configured services and tasks.
async fn exec_refuses_configured_names() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", HANG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Status, json!({}), cx(&main))
            .await
            .expect("register");
        for (name, kind) in [("web", "service"), ("hang", "task")] {
            let error = h
                .call(
                    Method::Exec,
                    json!({"name": name, "command": ["sleep"]}),
                    cx(&main),
                )
                .await
                .expect_err("taken");
            assert_eq!(error.code, "name_taken");
            assert_eq!(error.data["kind"], kind);
        }
        h.call(
            Method::Exec,
            json!({"name": "shell", "command": ["sleep"]}),
            cx(&main),
        )
        .await
        .expect("a free name");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A previous daemon's orphan counts as running: the next start adds units
/// to the lane instead of starting it whole.
async fn an_orphan_makes_a_start_partial() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: demo\nservices:\n  web: { run: sleep, ready: started }\n  worker: { run: sleep, ready: started }\n";
        let main = repo(&tmp.path, "r", config);
        let home = tmp.path.join("home");
        let first = harness(&home, Setup::default()).await;
        let started: StartResult = serde_json::from_value(
            first
                .call(Method::Start, json!({"units": ["web"]}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("result");
        let pid = started.units[0].pid.expect("pid");
        // A crash: nothing is stopped.
        drop(first);
        let processes = FakeProcesses::default();
        processes.alive.lock().expect("lock").insert(pid);
        let h = harness(
            &home,
            Setup {
                processes: Some(processes),
                logs: Some(tmp.path.join("logs-2")),
                ..Setup::default()
            },
        )
        .await;
        let started: StartResult = serde_json::from_value(
            h.call(Method::Start, json!({}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("result");
        assert!(!started.whole_lane, "{started:?}");
        assert_eq!(started.started, ["demo@0:worker"]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A config edited in the middle of a start does not change what it spawns;
/// the lane picks the edit up once the start is over.
async fn an_edit_mid_start_keeps_the_selected_config() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        // The nap holds the start open long enough for the watcher to see the edit
        // even on a loaded machine; every wait below ends on an observed state.
        let config = "project: reload\ntasks:\n  pause: { run: \"nap 8000\", when: lane.start }\nservices:\n  web: { run: \"echo first; sleep\", ready: started }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                watch: true,
                ..Setup::default()
            },
        )
        .await;
        let (_session, _frames, starting) =
            spawn_call(&h, Method::Start, json!({"timeout_ms": 40_000}), cx(&main));
        let napping = eventually(Duration::from_secs(15), || async {
            (h.runner.journal.entries() == ["nap 8000"]).then_some(())
        })
        .await;
        assert!(napping.is_some(), "the start reached its task");
        let edited = config.replace("echo first", "echo second");
        // Rewritten on every try: a watcher that took its first snapshot after an
        // earlier write still sees a newer modification time.
        let seen = eventually(Duration::from_secs(15), || async {
            std::fs::write(main.join("devlanes.yaml"), &edited).expect("rewrite");
            lane(&h, &main).await.config_changed.then_some(())
        })
        .await;
        assert!(seen.is_some(), "the watcher recorded the edit");
        assert!(!starting.is_finished(), "the edit came mid-start");
        starting.await.expect("join").expect("start");
        assert_eq!(
            h.runner.journal.entries(),
            ["nap 8000", "echo first; sleep"]
        );
        let reloaded = eventually(Duration::from_secs(15), || async {
            lane(&h, &main).await.config_changed.then_some(())
        })
        .await;
        assert!(reloaded.is_some(), "the edit is picked up afterwards");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `wait` stops polling once its client left.
async fn wait_ends_when_its_client_leaves() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: slow\nservices:\n  web: { run: sleep, ready: { log: never } }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({"timeout_ms": 200}), cx(&main))
            .await
            .expect("start");
        let (session, _frames, waiting) = spawn_call(
            &h,
            Method::Wait,
            json!({"target": "web", "timeout_ms": 60_000}),
            cx(&main),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.close();
        let error = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("wait ended")
            .expect("join")
            .expect_err("disconnected");
        assert_eq!(error.code, "client_disconnected");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `logs --all` shares one byte budget across its units.
async fn logs_of_several_units_share_the_budget() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let lines = "echo 0123456789012345678901234567890123456789; ".repeat(20);
        let config = format!(
            "project: chatty\nservices:\n  a: {{ run: \"{lines}sleep\", ready: started }}\n  b: {{ run: \"{lines}sleep\", ready: started }}\n"
        );
        let main = repo(&tmp.path, "r", &config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let budget = 1500;
        let all = eventually(Duration::from_secs(5), || async {
            let all = h
                .call(Method::Logs, json!({"all": true, "max_bytes": budget}), cx(&main))
                .await
                .ok()?;
            let groups = all["units"].as_array()?;
            groups
                .iter()
                .all(|group| group["truncated"] == true)
                .then_some(all)
        })
        .await
        .expect("both units logged past their share");
        let used: usize = all["units"]
            .as_array()
            .expect("units")
            .iter()
            .flat_map(|group| group["lines"].as_array().cloned().unwrap_or_default())
            .map(|line| serde_json::to_vec(&line).map_or(0, |bytes| bytes.len() + 1))
            .sum();
        assert!(used <= budget, "{used} bytes over a budget of {budget}");
        h.close().await;
    })
    .await
    .expect("timeout");
}
