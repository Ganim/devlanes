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
/// Renaming `project:` in the primary worktree renames the lane references in
/// memory and in state.db and emits `lane.changed`; the old reference answers
/// `not_found` naming the new one.
async fn renaming_the_project_renames_its_lanes() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", WEB);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        assert_eq!(lane_at(&h, &main, &main).await.reference, "demo@0");
        let mut bus = h.engine.subscribe();
        std::fs::write(
            main.join("devlanes.yaml"),
            WEB.replace("project: demo", "project: revb"),
        )
        .expect("rename");
        // `wait` reloads an edited config before it looks the lane up.
        let _ = h
            .call(
                Method::Wait,
                json!({"target": "web", "timeout_ms": 10}),
                cx(&main),
            )
            .await;
        assert_eq!(lane_at(&h, &main, &main).await.reference, "revb@0");
        let renamed = std::iter::from_fn(|| bus.try_recv().ok()).any(|event| {
            event.event == events::LANE_CHANGED && event.data["lane"]["reference"] == "revb@0"
        });
        assert!(renamed, "lane.changed carries the new reference");
        let old = h
            .call(Method::Status, json!({}), on_lane(&main, "demo@0"))
            .await
            .expect_err("the old name is gone");
        assert_eq!((old.code.as_str(), old.exit_code), ("not_found", 3));
        assert_eq!(old.data["renamed_to"], "revb");
        assert_eq!(old.data["suggestion"], "revb@0");
        h.close().await;
        let store = devlanes_state::Store::open(tmp.path.join("home/state.db"))
            .await
            .expect("store");
        let names: Vec<String> = store
            .projects()
            .await
            .expect("projects")
            .into_iter()
            .map(|project| project.name)
            .collect();
        assert_eq!(names, ["revb"]);
    })
    .await
    .expect("timeout");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
/// A project in a folder with spaces and mixed case is one project with one
/// lane per worktree, whatever case and separators the cwd is spelled with.
async fn windows_cwd_spelling_finds_the_same_lanes() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "Acme Src", WEB);
        let feature = tmp.path.join("Acme Src-Feature");
        worktree(&main, &feature, "feature");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let respell = |path: &Path| {
            std::path::PathBuf::from(path.display().to_string().to_uppercase().replace('\\', "/"))
        };
        let references = |lanes: Vec<LaneSummary>| {
            let mut references: Vec<String> =
                lanes.into_iter().map(|lane| lane.reference).collect();
            references.sort();
            references
        };
        // devlanes running in a worktree gives it a lane, here from a respelled cwd.
        lanes(&h, &respell(&feature.join("sub"))).await;
        let exact = references(lanes(&h, &main).await);
        assert_eq!(exact, ["demo@0", "demo@1"]);
        assert_eq!(references(lanes(&h, &respell(&main)).await), exact);
        assert_eq!(
            references(lanes(&h, &respell(&feature.join("sub"))).await),
            exact
        );
        assert_eq!(lane_at(&h, &main, &feature).await.reference, "demo@1");
        h.close().await;
        let store = devlanes_state::Store::open(tmp.path.join("home/state.db"))
            .await
            .expect("store");
        assert_eq!(store.projects().await.expect("projects").len(), 1);
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Writing a config into a worktree devlanes ran in without one, while another
/// lane runs, assigns the new lane and marks no worktree missing, in memory or
/// in state.db.
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
        // The primary worktree registers the project; then devlanes runs in wt.
        lanes(&h, &main).await;
        assert_eq!(lane_at(&h, &wt, &wt).await.state, "no-config");
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
/// asking for no confirmation when nothing is to clean; once they stopped it
/// asks with the lanes it would clean, and cleans them confirmed.
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
        // Nothing to clean while it runs: no confirmation needed.
        let gc = h
            .call(Method::LaneGc, json!({}), cx(&main))
            .await
            .expect("gc");
        assert_eq!(gc["cleaned"], json!([]), "{gc}");
        assert_eq!(gc["kept_running"], json!(["demo@1"]), "{gc}");
        let kept = lane_at(&h, &main, &wt).await;
        assert_eq!(kept.state, "running");

        h.call(Method::Stop, json!({}), on_lane(&main, "demo@1"))
            .await
            .expect("stop");
        let asked = h
            .call(Method::LaneGc, json!({}), cx(&main))
            .await
            .expect_err("a lane to clean needs --yes");
        assert_eq!(asked.code, "confirmation_required");
        assert_eq!(asked.data["lanes"], json!(["demo@1"]), "{asked:?}");
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
/// A removed worktree's running lane stops even when the config poller read its
/// config as missing before the worktree watcher saw the removal.
async fn removed_worktree_stops_after_its_config_vanished() {
    removed_worktree_stops(false).await;
}

#[tokio::test(flavor = "multi_thread")]
/// The same when the daemon first saw the project from the removed worktree,
/// so the worktree watcher can no longer list from there.
async fn removed_worktree_stops_when_the_project_was_found_there() {
    removed_worktree_stops(true).await;
}

/// Starts a linked worktree's lane, deletes its config until the poller sees
/// it missing, removes the worktree and waits for the lane to stop. The
/// project is first seen from the primary worktree, or from the linked one.
async fn removed_worktree_stops(found_in_linked: bool) {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", WEB);
        let wt = tmp.path.join("wt");
        worktree(&main, &wt, "feat/x");
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                watch: true,
                ..Setup::default()
            },
        )
        .await;
        if !found_in_linked {
            h.call(Method::LaneList, json!({}), cx(&main))
                .await
                .expect("lane list");
        }
        h.call(Method::Start, json!({}), cx(&wt))
            .await
            .expect("start wt");
        std::fs::remove_file(wt.join("devlanes.yaml")).expect("drop config");
        eventually(Duration::from_secs(10), || async {
            let lane = lane_at(&h, &main, &wt).await;
            lane.config_changed.then_some(lane)
        })
        .await
        .expect("the poller reads the config as missing");

        git(
            &main,
            &["worktree", "remove", "--force", &wt.display().to_string()],
        );
        let stopped = eventually(Duration::from_secs(10), || async {
            let lane = lane_at(&h, &main, &wt).await;
            (lane.state != "running").then_some(lane)
        })
        .await
        .expect("the removed worktree's lane stops");
        assert!(
            stopped.units.iter().all(|unit| unit.state == "stopped"),
            "{stopped:?}"
        );
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
        // `config_changed` comes with the reload, `ports_pending` with the
        // refresh after it; a status in between sees only the first.
        let edited = eventually(Duration::from_secs(10), || async {
            Some(lane_at(&h, &main, &main).await).filter(|lane| {
                lane.config_changed && lane.warnings.iter().any(|w| w.code == "ports_pending")
            })
        })
        .await
        .expect("the edit is noticed and its ports wait for the next start");
        assert_eq!(edited.port_block, Some(block), "the block stays");
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
/// A service added to a running lane's config takes a free offset of the kept
/// block and starts; when the fixed block has no room, `start` answers
/// `ports_pending` instead of a missing-port error.
async fn a_service_added_to_a_running_lane_starts_in_its_block() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let web = "  web: { run: sleep, ready: started }\n";
        let main = repo(&tmp.path, "r", &format!("project: demo\nlanes: {{ block_size: 2 }}\nservices:\n{web}"));
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let before = lane_at(&h, &main, &main).await;
        let port = before.units[0].ports["default"];

        std::fs::write(
            main.join("devlanes.yaml"),
            format!("project: demo\nlanes: {{ block_size: 2 }}\nservices:\n{web}  api: {{ run: sleep, ready: started }}\n"),
        )
        .expect("add api");
        let started = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("api starts");
        assert_eq!(started["started"], json!(["demo@0:api"]), "{started}");
        let after = lane_at(&h, &main, &main).await;
        assert_eq!(after.port_block, before.port_block, "the block stays");
        let web_now = after.units.iter().find(|u| u.name == "web").expect("web");
        assert_eq!(web_now.ports["default"], port);

        // More services than the kept block has offsets: it would have to move.
        // (A fixed `block_size` that is too small fails validation instead.)
        let block = after.port_block.expect("block");
        let mut many = format!("project: demo\nservices:\n{web}  api: {{ run: sleep, ready: started }}\n");
        for i in 0..=(block.1 - block.0) {
            many.push_str(&format!("  s{i}: {{ run: sleep, ready: started }}\n"));
        }
        std::fs::write(main.join("devlanes.yaml"), many).expect("add services");
        let refused = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect_err("no room for the new services");
        assert_eq!(refused.code, "ports_pending", "{refused:?}");
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

/// One web service in a two-port block.
const SMALL: &str = "project: demo\nlanes: { block_size: 2 }\nservices:\n  web: { run: sleep, ready: started, hosts: [\"web.${{ lane.domain }}\"] }\n";

/// [`SMALL`] grown to three services in a four-port block.
const BIG: &str = "project: demo\nlanes: { block_size: 4 }\nservices:\n  web: { run: sleep, ready: started, hosts: [\"web.${{ lane.domain }}\"] }\n  api: { run: sleep, ready: started }\n  docs: { run: sleep, ready: started }\n";

/// A start of `main` caught between resolving and spawning, during which its
/// config grew to need a bigger block and the worktrees were re-listed.
struct HeldStart {
    _tmp: crate::Temp,
    h: crate::Harness,
    main: std::path::PathBuf,
    /// The block the start resolved against.
    block: (u16, u16),
    start: tokio::task::JoinHandle<Result<Value, devlanes_proto::RpcError>>,
}

/// Starts `main` behind a start-gate slot another lane holds for `slot`, so the
/// start has resolved and waits to spawn; then edits main's config to [`BIG`]
/// and re-lists. Checks that neither moved the block.
async fn held_start(slot: Duration) -> HeldStart {
    let tmp = temp();
    let main = repo(&tmp.path, "r", SMALL);
    let other = tmp.path.join("wt");
    worktree(&main, &other, "feat/other");
    let h = harness(
        &tmp.path.join("home"),
        Setup {
            watch: true,
            gate: Some(crate::pacing::gate(1, slot)),
            metrics: crate::pacing::busy_metrics(),
            ..Setup::default()
        },
    )
    .await;
    h.call(Method::LaneList, json!({}), cx(&main))
        .await
        .expect("lane list");
    h.call(Method::Start, json!({}), cx(&other))
        .await
        .expect("start other");
    let block = lane_at(&h, &main, &main).await.port_block.expect("block");
    assert_eq!(block.1 - block.0, 1, "{block:?}");
    let start = {
        let engine = h.engine.clone();
        let context = cx(&main);
        tokio::spawn(async move {
            let (session, _frames) = devlanes_daemon::Session::local(9_100);
            let request = devlanes_proto::Request {
                id: 9_100,
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

    std::fs::write(main.join("devlanes.yaml"), BIG).expect("edit config");
    eventually(Duration::from_secs(5), || async {
        lane_at(&h, &main, &main).await.config_changed.then_some(())
    })
    .await
    .expect("the edit is recorded mid-start");
    h.call(Method::LaneList, json!({}), cx(&main))
        .await
        .expect("re-list");
    let lane = lane_at(&h, &main, &main).await;
    assert_eq!(lane.port_block, Some(block), "the block stays mid-start");
    assert!(!start.is_finished(), "the edit came mid-start");
    HeldStart {
        _tmp: tmp,
        h,
        main,
        block,
        start,
    }
}

#[tokio::test(flavor = "multi_thread")]
/// A re-list during a start whose config now needs a bigger block leaves the
/// block alone; the spawned ports, routes and status agree, and the refresh after
/// the start reports `ports_pending` because the unit runs.
async fn a_relist_mid_start_keeps_the_block() {
    tokio::time::timeout(LIMIT, async {
        let HeldStart {
            _tmp,
            h,
            main,
            block,
            start,
        } = held_start(Duration::from_secs(3)).await;
        start.await.expect("join").expect("start");
        let lane = eventually(Duration::from_secs(5), || async {
            Some(lane_at(&h, &main, &main).await)
                .filter(|lane| lane.warnings.iter().any(|w| w.code == "ports_pending"))
        })
        .await
        .expect("the deferred refresh ran");
        assert_eq!(lane.port_block, Some(block));
        assert!(lane.config_changed);
        let web = lane
            .units
            .iter()
            .find(|unit| unit.name == "web")
            .expect("web");
        assert_eq!(web.state, "ready");
        let port = web.ports["default"];
        assert!((block.0..=block.1).contains(&port), "{port} in {block:?}");
        let hosts = h
            .call(Method::Hosts, json!({}), cx(&main))
            .await
            .expect("hosts");
        let route = hosts["items"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["lane"] == "demo@0"))
            .cloned()
            .expect("web route");
        assert_eq!(route["port"], port, "{hosts}");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// When the held start ends with nothing running (a stop withdrew it), the
/// deferred refresh moves the block to the size the config asks for.
async fn the_deferred_refresh_moves_an_idle_lanes_block() {
    tokio::time::timeout(LIMIT, async {
        let HeldStart {
            _tmp,
            h,
            main,
            block,
            start,
        } = held_start(Duration::from_secs(30)).await;
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        let _ = start.await.expect("join");
        let lane = eventually(Duration::from_secs(5), || async {
            Some(lane_at(&h, &main, &main).await)
                .filter(|lane| lane.port_block.is_some_and(|b| b.1 - b.0 == 3))
        })
        .await
        .expect("the block grew after the start");
        assert_ne!(lane.port_block, Some(block));
        assert!(lane.warnings.is_empty(), "{:?}", lane.warnings);
        assert!(
            lane.units.iter().all(|unit| unit.state == "idle"),
            "{:?}",
            lane.units
        );
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
        let states = |lane: &LaneSummary| -> Vec<(String, String)> {
            lane.units
                .iter()
                .map(|unit| (unit.name.clone(), unit.state.clone()))
                .collect()
        };
        // Events still in flight settle within moments; wait for the final one
        // rather than a fixed time.
        let mut last = None;
        let settled = eventually(Duration::from_secs(5), || {
            while let Ok(event) = bus.try_recv() {
                if event.event == events::LANE_CHANGED {
                    last = Some(event.data["lane"].clone());
                }
            }
            let seen = last.clone();
            async move {
                let lane: LaneSummary = serde_json::from_value(seen?).ok()?;
                lane.units
                    .iter()
                    .all(|unit| unit.state == "ready")
                    .then_some(lane)
            }
        })
        .await;
        let last = settled.expect("a lane.changed with every unit ready");
        let current = lane_at(&h, &main, &main).await;
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
