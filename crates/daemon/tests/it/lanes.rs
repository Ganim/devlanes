//! Lane bookkeeping around running units: config edits, worktree presence,
//! lane moves, host conflicts and the order of published state.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use devlanes_daemon::integrations::{RouteEntry, RouteSink, RouteStatus};
use devlanes_proto::{Context, LaneSummary, Method, Page, events};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, git, harness, repo, temp, worktree};

const WEB: &str = r#"project: demo
services:
  web:
    run: sleep
    ready: started
    hosts: ["web.${{ lane.domain }}", shared.localhost]
"#;

/// Every lane visible from `cwd`.
async fn lanes(h: &crate::Harness, cwd: &Path) -> Vec<LaneSummary> {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    serde_json::from_value::<Page<LaneSummary>>(value)
        .expect("page")
        .items
}

/// The lane at `path`.
async fn lane_at(h: &crate::Harness, cwd: &Path, path: &Path) -> LaneSummary {
    lanes(h, cwd)
        .await
        .into_iter()
        .find(|lane| lane.path == path)
        .unwrap_or_else(|| panic!("no lane at {}", path.display()))
}

/// `cx(cwd)` aimed at `lane`, confirmed.
fn on_lane(cwd: &Path, lane: &str) -> Context {
    Context {
        lane: Some(lane.into()),
        yes: true,
        ..cx(cwd)
    }
}

#[tokio::test(flavor = "multi_thread")]
/// Writing a config into a worktree without one, while another lane runs,
/// assigns the new lane and marks no worktree missing, in memory or in state.db.
async fn new_config_assigns_without_marking_anything_missing() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", WEB);
        let wt = tmp.path.join("wt");
        worktree(&main, &wt, "feat/x");
        std::fs::remove_file(wt.join("devlanes.yaml")).expect("drop config");
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                watch: true,
                ..Setup::default()
            },
        )
        .await;
        assert_eq!(lane_at(&h, &main, &wt).await.state, "no-config");
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start main");
        let mut bus = h.engine.subscribe();

        std::fs::write(wt.join("devlanes.yaml"), WEB).expect("write config");
        let index = eventually(Duration::from_secs(10), || async {
            lane_at(&h, &main, &wt).await.index
        })
        .await
        .expect("the new lane gets an index");
        assert_eq!(index, 1);

        while let Ok(event) = bus.try_recv() {
            if event.event == events::LANE_CHANGED {
                assert_ne!(
                    event.data["lane"]["reason"]["code"], "worktree_missing",
                    "{event:?}"
                );
            }
        }
        for lane in lanes(&h, &main).await {
            assert!(lane.reason.is_none(), "{lane:?}");
        }
        assert_eq!(lane_at(&h, &main, &main).await.state, "running");
        let store = devlanes_state::Store::open(tmp.path.join("home/state.db"))
            .await
            .expect("store");
        for project in store.projects().await.expect("projects") {
            for record in store.worktrees(project.id).await.expect("worktrees") {
                assert!(record.present, "{record:?}");
            }
        }
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `lane gc` keeps a lane whose worktree is gone while its units still run,
/// and cleans it once they stopped.
async fn gc_keeps_lanes_that_run() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", WEB);
        let wt = tmp.path.join("wt");
        worktree(&main, &wt, "feat/x");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        h.call(Method::Start, json!({}), cx(&wt))
            .await
            .expect("start wt");
        git(
            &main,
            &["worktree", "remove", "--force", &wt.display().to_string()],
        );
        let confirmed = Context {
            yes: true,
            ..cx(&main)
        };
        let gc = h
            .call(Method::LaneGc, json!({}), confirmed.clone())
            .await
            .expect("gc");
        assert_eq!(gc["cleaned"], json!([]), "{gc}");
        assert_eq!(gc["kept_running"], json!(["demo@1"]), "{gc}");
        let kept = lane_at(&h, &main, &wt).await;
        assert_eq!(kept.state, "running");

        h.call(Method::Stop, json!({}), on_lane(&main, "demo@1"))
            .await
            .expect("stop");
        let gc = h
            .call(Method::LaneGc, json!({}), confirmed)
            .await
            .expect("gc");
        assert_eq!(gc["cleaned"], json!(["demo@1"]), "{gc}");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Editing the config of a running lane so it needs another block leaves the
/// block and the routed ports alone and reports `ports_pending`; the next
/// whole-lane start moves the block.
async fn config_edit_keeps_a_running_lanes_ports() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let small = "project: demo\nlanes: { block_size: 2 }\nservices:\n  web: { run: sleep, ready: started, hosts: [\"web.${{ lane.domain }}\"] }\n";
        let main = repo(&tmp.path, "r", small);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                watch: true,
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let before = lane_at(&h, &main, &main).await;
        let block = before.port_block.expect("block");
        assert_eq!(block.1 - block.0, 1, "{block:?}");
        let port = before.units[0].ports["default"];

        std::fs::write(
            main.join("devlanes.yaml"),
            "project: demo\nlanes: { block_size: 4 }\nservices:\n  web: { run: sleep, ready: started, hosts: [\"web.${{ lane.domain }}\"] }\n  api: { run: sleep, ready: started }\n  docs: { run: sleep, ready: started }\n",
        )
        .expect("edit config");
        let edited = eventually(Duration::from_secs(10), || async {
            Some(lane_at(&h, &main, &main).await).filter(|lane| lane.config_changed)
        })
        .await
        .expect("the edit is noticed");
        assert_eq!(edited.port_block, Some(block), "the block stays");
        assert!(
            edited.warnings.iter().any(|w| w.code == "ports_pending"),
            "{:?}",
            edited.warnings
        );
        assert_eq!(edited.units[0].ports["default"], port);
        let hosts = h
            .call(Method::Hosts, json!({}), cx(&main))
            .await
            .expect("hosts");
        assert_eq!(hosts["items"][0]["port"], port, "{hosts}");

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start again");
        let after = lane_at(&h, &main, &main).await;
        let moved = after.port_block.expect("block");
        assert_eq!(moved.1 - moved.0, 3, "{moved:?}");
        assert!(after.warnings.is_empty(), "{:?}", after.warnings);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A start waiting in the start gate counts as running: a worktree that moved
/// onto the primary branch meanwhile does not take lane 0 from it.
async fn a_start_in_flight_keeps_lane_zero() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", WEB);
        let (a, b) = (tmp.path.join("wt-a"), tmp.path.join("wt-b"));
        worktree(&main, &a, "feat/a");
        worktree(&main, &b, "feat/b");
        let gate = crate::pacing::gate(1, Duration::from_secs(3));
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                gate: Some(gate),
                metrics: crate::pacing::busy_metrics(),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        // `a` holds the only slot, so the next start waits in the gate.
        h.call(Method::Start, json!({}), cx(&a))
            .await
            .expect("start a");
        let holder = {
            let engine = h.engine.clone();
            let context = cx(&main);
            tokio::spawn(async move {
                let (session, _frames) = devlanes_daemon::Session::local(9_000);
                let request = devlanes_proto::Request {
                    id: 9_000,
                    method: Method::Start,
                    params: json!({}),
                    context,
                };
                devlanes_daemon::Handler::handle(&engine, request, session).await
            })
        };
        eventually(Duration::from_secs(5), || async {
            let lane = lane_at(&h, &main, &main).await;
            lane.units
                .iter()
                .any(|unit| unit.state == "queued")
                .then_some(())
        })
        .await
        .expect("main's start waits in the gate");
        // Only now does `b` take the primary branch, so main's own start began
        // without a move of its own.
        git(&main, &["checkout", "-q", "-b", "other"]);
        git(&b, &["checkout", "-q", "main"]);
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");

        let _ = h
            .call(Method::Start, json!({"timeout_ms": 300}), cx(&b))
            .await;
        assert_eq!(lane_at(&h, &main, &main).await.index, Some(0));
        let moved = lane_at(&h, &main, &b).await;
        assert_eq!(moved.index, Some(2));
        assert_eq!(moved.moves_to, Some(0));
        let _ = holder.await;
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Two lanes routing one hostname: the first lane keeps it while neither runs,
/// the running lane takes it, and both lanes, `hosts` and `doctor` warn.
async fn host_conflicts_prefer_running_lanes() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", WEB);
        let wt = tmp.path.join("wt");
        worktree(&main, &wt, "feat/x");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        let owner = |hosts: &Value| {
            hosts["items"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["host"] == "shared.localhost"))
                .map(|item| item["lane"].clone())
        };
        let hosts = h
            .call(Method::Hosts, json!({}), cx(&main))
            .await
            .expect("hosts");
        assert_eq!(owner(&hosts), Some(json!("demo@0")), "{hosts}");

        h.call(Method::Start, json!({}), cx(&wt))
            .await
            .expect("start wt");
        let hosts = h
            .call(Method::Hosts, json!({}), cx(&main))
            .await
            .expect("hosts");
        assert_eq!(owner(&hosts), Some(json!("demo@1")), "{hosts}");
        let warning = &hosts["warnings"][0];
        assert_eq!(warning["code"], "host_conflict", "{hosts}");
        assert_eq!(warning["data"]["lane"], "demo@1");
        assert_eq!(warning["data"]["other"], "demo@0");
        for path in [&main, &wt] {
            let lane = lane_at(&h, &main, path).await;
            assert!(
                lane.warnings.iter().any(|w| w.code == "host_conflict"),
                "{lane:?}"
            );
        }
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        assert_eq!(doctor["route_warnings"][0]["code"], "host_conflict");
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// Keeps the latest snapshot.
#[derive(Default)]
struct Recorder(Mutex<Vec<RouteEntry>>);

impl RouteSink for Recorder {
    /// Replaces the recorded snapshot.
    fn publish(&self, routes: Vec<RouteEntry>) {
        *self.0.lock().expect("recorder") = routes;
    }
}

#[tokio::test(flavor = "multi_thread")]
/// Units of one start become ready concurrently; the last `lane.changed` and the
/// last route snapshot both show the final state.
async fn concurrent_transitions_publish_the_final_state_last() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let mut config = String::from("project: demo\nservices:\n");
        for i in 0..12 {
            config.push_str(&format!(
                "  s{i}: {{ run: sleep, ready: started, hosts: [\"s{i}.${{{{ lane.domain }}}}\"] }}\n"
            ));
        }
        let main = repo(&tmp.path, "r", &config);
        let recorder = Arc::new(Recorder::default());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                routes: Some(recorder.clone()),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        let mut bus = h.engine.subscribe();
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let current = lane_at(&h, &main, &main).await;
        let mut last = None;
        while let Ok(event) = bus.try_recv() {
            if event.event == events::LANE_CHANGED {
                last = Some(event.data["lane"].clone());
            }
        }
        let last: LaneSummary = serde_json::from_value(last.expect("lane.changed")).expect("lane");
        let states = |lane: &LaneSummary| -> Vec<(String, String)> {
            lane.units
                .iter()
                .map(|unit| (unit.name.clone(), unit.state.clone()))
                .collect()
        };
        assert_eq!(states(&last), states(&current));
        assert!(current.units.iter().all(|unit| unit.state == "ready"));
        let published = recorder.0.lock().expect("recorder").clone();
        assert_eq!(published.len(), 12);
        assert!(
            published
                .iter()
                .all(|route| route.status == RouteStatus::Ready),
            "{published:?}"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}
