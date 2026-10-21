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

/// Records `members` as escaped descendants of the unfinished run of `unit`.
async fn record_members(home: &std::path::Path, unit: &str, members: &[u32]) {
    let store = devlanes_state::Store::open(home.join("state.db"))
        .await
        .expect("store");
    let run = store
        .active_unit_runs()
        .await
        .expect("runs")
        .into_iter()
        .find(|run| run.unit == unit)
        .expect("run");
    let members = members
        .iter()
        .map(|&pid| devlanes_core::ProcessIdentity {
            pid,
            sid: pid,
            start_time: 1,
        })
        .collect();
    store
        .record_unit_run_members(run.id, members)
        .await
        .expect("members");
    store.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread")]
/// Descendants that left the session are signalled with their orphan, and keep
/// it orphaned after the leader itself exited.
async fn escaped_descendants_are_stopped_with_their_orphan() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let first = harness(&home, Setup::default()).await;
        let started: StartResult = serde_json::from_value(
            first
                .call(Method::Start, json!({}), cx(&main))
                .await
                .expect("start"),
        )
        .expect("start result");
        let pids: std::collections::BTreeMap<String, u32> = started
            .units
            .iter()
            .map(|u| (u.name.clone(), u.pid.expect("pid")))
            .collect();
        drop(first);
        record_members(&home, "web", &[900_001]).await;
        record_members(&home, "worker", &[900_002]).await;
        let processes = FakeProcesses::default();
        // The worker's leader exited; only its escaped daemon still runs.
        processes
            .alive
            .lock()
            .expect("lock")
            .extend([pids["web"], 900_001, 900_002]);
        let h = harness(
            &home,
            Setup {
                processes: Some(processes.clone()),
                logs: Some(tmp.path.join("logs-after-crash")),
                ..Setup::default()
            },
        )
        .await;
        let summary = lane(&h, &main).await;
        assert_eq!(unit(&summary, "web").state, "orphaned");
        assert_eq!(unit(&summary, "worker").state, "orphaned");
        assert_eq!(unit(&summary, "api").state, "idle");

        h.call(Method::Stop, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("stop");
        let mut killed = processes.killed.lock().expect("lock").clone();
        killed.sort_unstable();
        let mut expected = vec![pids["web"], 900_001];
        expected.sort_unstable();
        assert_eq!(killed, expected);
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop all");
        assert!(processes.killed.lock().expect("lock").contains(&900_002));
        assert!(active_runs(&home).await.is_empty());
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A unit whose run cannot be recorded is stopped instead of running untracked,
/// and the task that started it still gets its task run finished.
async fn an_unrecorded_spawn_is_refused_and_its_task_run_finished() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(
            &tmp.path,
            "r",
            "project: demo\nservices:\n  web: { run: sleep }\ntasks:\n  seed: { run: sleep }\n",
        );
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        lane(&h, &main).await;
        // A stale unfinished row this daemon does not know holds the unit's slot.
        let store = devlanes_state::Store::open(home.join("state.db"))
            .await
            .expect("store");
        let lane_id = store.lanes(None).await.expect("lanes")[0].id;
        let actor = devlanes_core::Actor {
            kind: devlanes_core::ActorKind::Human,
            agent_id: None,
            session_id: None,
        };
        store
            .record_unit_run(devlanes_state::UnitRunInput {
                lane_id,
                unit: "seed".into(),
                process: devlanes_core::ProcessIdentity {
                    pid: 900_004,
                    sid: 900_004,
                    start_time: 1,
                },
                actor,
                log_path: None,
                boot_id: None,
            })
            .await
            .expect("stale run");

        let error = h
            .call(Method::Run, json!({"task": "seed"}), cx(&main))
            .await
            .expect_err("refused");
        assert_eq!(error.code, "state_conflict", "{error:?}");
        let tasks = store.task_runs(lane_id, 1).await.expect("task runs");
        assert_eq!(tasks[0].task, "seed");
        assert!(tasks[0].ended_at.is_some() && tasks[0].exit_code.is_none());
        let summary = lane(&h, &main).await;
        assert!(
            summary.units.iter().all(|u| u.name != "seed"),
            "the unrecorded unit is not kept: {:?}",
            summary.units
        );
        store.close().await.expect("close");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `daemon reset-state` terminates orphans, including those of a lane this
/// daemon never loaded, and only then empties the store.
async fn reset_state_terminates_orphans_before_resetting() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let (h, processes, pids) = crashed(&tmp, &main, &["web"]).await;
        let home = tmp.path.join("home");
        // A run of a lane the daemon does not know (no project config directory).
        let store = devlanes_state::Store::open(home.join("state.db"))
            .await
            .expect("store");
        let ghost = store
            .upsert_project(devlanes_state::ProjectInput {
                name: "ghost".into(),
                common_dir: None,
                root_path: tmp.path.join("ghost"),
                primary_branch: "main".into(),
            })
            .await
            .expect("project");
        let worktree = store
            .upsert_worktree(devlanes_state::WorktreeInput {
                project_id: ghost.id,
                identity: "main".into(),
                path: tmp.path.join("ghost"),
                branch: None,
                present: true,
            })
            .await
            .expect("worktree");
        let ghost_lane = store
            .upsert_lane(devlanes_state::LaneInput {
                project_id: ghost.id,
                worktree_id: worktree.id,
                index: 0,
                name: "ghost".into(),
                desired_state: devlanes_state::DesiredState::Running,
                profile: None,
                profiles: Vec::new(),
            })
            .await
            .expect("lane");
        store
            .record_unit_run(devlanes_state::UnitRunInput {
                lane_id: ghost_lane.id,
                unit: "daemon".into(),
                process: devlanes_core::ProcessIdentity {
                    pid: 900_003,
                    sid: 900_003,
                    start_time: 1,
                },
                actor: devlanes_core::Actor {
                    kind: devlanes_core::ActorKind::Human,
                    agent_id: None,
                    session_id: None,
                },
                log_path: None,
                boot_id: None,
            })
            .await
            .expect("ghost run");
        store.close().await.expect("close");
        processes.alive.lock().expect("lock").insert(900_003);

        let reset = h
            .call(
                Method::DaemonResetState,
                json!({}),
                devlanes_proto::Context {
                    yes: true,
                    ..cx(&main)
                },
            )
            .await
            .expect("reset");
        assert_eq!(reset["terminated"], 2, "{reset}");
        let mut killed = processes.killed.lock().expect("lock").clone();
        killed.sort_unstable();
        let mut expected = vec![pids["web"], 900_003];
        expected.sort_unstable();
        assert_eq!(killed, expected);
        assert!(active_runs(&home).await.is_empty());
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
