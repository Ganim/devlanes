//! Which folders are lanes: worktrees become lanes when devlanes runs in them,
//! a folder can hold several lanes, and per-unit facts clients render
//! (display name, readiness check, terminal).

use std::path::Path;

use devlanes_proto::{Context, LaneSummary, Method, Page, StartResult};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, harness, repo, temp, worktree};

const CONFIG: &str = r#"project: demo
services:
  web:
    name: Web shop
    run: serve
    hosts: ["web.${{ lane.domain }}"]
  worker:
    run: sleep
    pty: false
"#;

/// Every lane visible from `cwd`.
async fn status(h: &crate::Harness, context: Context) -> Vec<LaneSummary> {
    let value = h
        .call(Method::Status, json!({}), context)
        .await
        .expect("status");
    serde_json::from_value::<Page<LaneSummary>>(value)
        .expect("page")
        .items
}

/// `cx(cwd)` aimed at `lane`.
fn on_lane(cwd: &Path, lane: &str) -> Context {
    Context {
        lane: Some(lane.into()),
        ..cx(cwd)
    }
}

/// Starts with `context`.
async fn start(h: &crate::Harness, context: Context, params: Value) -> StartResult {
    let value = h.call(Method::Start, params, context).await.expect("start");
    serde_json::from_value(value).expect("start result")
}

#[tokio::test(flavor = "multi_thread")]
/// `lane new --name` adds a second lane for the cwd's folder with its own index
/// and domain; `--lane <name>` reaches it, the folder's commands keep using its
/// first lane, names are unique, and the extra lane survives a daemon restart.
async fn a_folder_can_hold_several_lanes() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        let created = h
            .call(Method::LaneNew, json!({"name": "review"}), cx(&main))
            .await
            .expect("lane new");
        assert_eq!(created["name"], "review");
        assert_eq!(created["lane"]["reference"], "demo@1", "{created}");
        assert_eq!(created["lane"]["domain"], "demo-1.localhost");
        // Clients label it by its name: its branch is the folder's first lane's.
        assert_eq!(created["lane"]["name"], "review");

        let first = start(&h, cx(&main), json!({"units": ["web"]})).await;
        assert_eq!(
            first.lane, "demo@0",
            "the folder's first lane stays its default"
        );
        let second = start(&h, on_lane(&main, "review"), json!({"units": ["web"]})).await;
        assert_eq!(second.lane, "demo@1");
        assert_eq!(second.started, ["demo@1:web"]);
        let lanes = status(&h, cx(&main)).await;
        let ports: Vec<u16> = lanes
            .iter()
            .map(|lane| {
                lane.units
                    .iter()
                    .find(|u| u.name == "web")
                    .expect("web")
                    .ports["default"]
            })
            .collect();
        assert_eq!(lanes.len(), 2);
        assert_ne!(ports[0], ports[1], "each lane has its own ports");
        assert!(lanes.iter().all(|lane| lane.path == main));
        let names: Vec<Option<&str>> = lanes.iter().map(|lane| lane.name.as_deref()).collect();
        assert_eq!(
            names,
            [None, Some("review")],
            "the worktree's own lane has none"
        );

        let taken = h
            .call(Method::LaneNew, json!({"name": "review"}), cx(&main))
            .await
            .expect_err("taken");
        assert_eq!(taken.code, "name_taken");
        let unnamed = h
            .call(Method::LaneNew, json!({}), cx(&main))
            .await
            .expect("default name");
        assert_eq!(unnamed["name"], "r-2");
        assert_eq!(unnamed["lane"]["reference"], "demo@2");

        h.call(Method::Stop, json!({}), on_lane(&main, "review"))
            .await
            .expect("stop review");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop main");
        h.close().await;
        drop(h);

        let h = harness(&home, Setup::default()).await;
        let lanes = status(&h, cx(&main)).await;
        let references: Vec<&str> = lanes.iter().map(|l| l.reference.as_str()).collect();
        assert_eq!(references, ["demo@0", "demo@1", "demo@2"]);
        let again = start(&h, on_lane(&main, "review"), json!({"units": ["web"]})).await;
        assert_eq!(again.lane, "demo@1");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Two lanes of one folder get different data folders under the devlanes home; each
/// exists once its lane does, and `lane clean` deletes only the cleaned lane's.
async fn lanes_of_one_folder_have_their_own_data_folders() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: demo\nservices:\n  worker:\n    run: \"echo ${{ lane.data_path }}; sleep\"\n    pty: false\n";
        let main = repo(&tmp.path, "r", config);
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        h.call(Method::LaneNew, json!({"name": "review"}), cx(&main))
            .await
            .expect("lane new");
        start(&h, cx(&main), json!({})).await;
        start(&h, on_lane(&main, "review"), json!({})).await;
        let paths: Vec<std::path::PathBuf> = h
            .runner
            .journal
            .entries()
            .iter()
            .filter_map(|script| script.strip_prefix("echo "))
            .filter_map(|rest| rest.split(';').next())
            .map(std::path::PathBuf::from)
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert_ne!(paths[0], paths[1], "one folder each");
        for path in &paths {
            assert!(path.starts_with(home.join("data/demo")), "{path:?}");
            assert!(path.is_dir(), "{path:?} exists");
        }

        h.call(Method::Stop, json!({}), on_lane(&main, "review"))
            .await
            .expect("stop review");
        h.call(
            Method::LaneClean,
            json!({}),
            Context {
                yes: true,
                ..on_lane(&main, "review")
            },
        )
        .await
        .expect("clean review");
        assert!(!paths[1].exists(), "the cleaned lane's folder is gone");
        assert!(paths[0].is_dir(), "the other lane keeps its folder");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop main");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Cleaning every lane of a project forgets it: a restarted daemon does not bring
/// it back, and the next command in its folder registers it again.
async fn a_project_whose_lanes_were_all_cleaned_is_forgotten() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        start(&h, cx(&main), json!({"units": ["worker"]})).await;
        h.call(
            Method::LaneClean,
            json!({}),
            Context {
                yes: true,
                ..cx(&main)
            },
        )
        .await
        .expect("clean");
        assert!(
            !home.join("data/demo").exists(),
            "the project's data folder goes with its last lane"
        );
        // Outside any project, status lists every project the daemon knows.
        assert!(status(&h, cx(&tmp.path)).await.is_empty());
        h.close().await;
        drop(h);

        let h = harness(&home, Setup::default()).await;
        assert!(
            status(&h, cx(&tmp.path)).await.is_empty(),
            "a restart does not bring it back"
        );
        let lanes = status(&h, cx(&main)).await;
        assert_eq!(lanes.len(), 1, "a command in the folder does");
        assert_eq!(lanes[0].reference, "demo@0");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A worktree that only exists is no lane; running devlanes in it makes one, and
/// a reference by its directory name reaches a worktree that has none.
async fn worktrees_become_lanes_when_devlanes_runs_in_them() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let wt = tmp.path.join("wt-feature");
        worktree(&main, &wt, "feat/x");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let lanes = status(&h, cx(&main)).await;
        assert_eq!(lanes.len(), 1, "{lanes:?}");
        assert_eq!(lanes[0].index, Some(0));

        let stopped = h
            .call(Method::Stop, json!({}), on_lane(&main, "demo@wt-feature"))
            .await
            .expect("a worktree without a lane can be named");
        assert_eq!(stopped["items"], json!([]));
        assert_eq!(
            status(&h, cx(&main)).await.len(),
            1,
            "naming it made no lane"
        );

        let result = start(&h, cx(&wt), json!({"units": ["worker"]})).await;
        assert_eq!(result.lane, "demo@1");
        assert_eq!(status(&h, cx(&main)).await.len(), 2);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Summaries carry the display name, the readiness check and whether the unit
/// has a terminal; resizing a pipe unit is a silent no-op; starting units that
/// run already changes nothing.
async fn summaries_and_pipe_units() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let idle = status(&h, cx(&main)).await;
        let web = idle[0].units.iter().find(|u| u.name == "web").expect("web");
        assert_eq!(web.display_name.as_deref(), Some("Web shop"));
        assert_eq!(
            web.public_url.as_deref(),
            Some("https://web.demo.localhost")
        );
        let orders: Vec<(&str, Option<u32>)> = idle[0]
            .units
            .iter()
            .map(|u| (u.name.as_str(), u.order))
            .collect();
        assert_eq!(orders, [("web", Some(0)), ("worker", Some(1))]);

        let result = start(&h, cx(&main), json!({})).await;
        assert!(result.ready, "{result:?}");
        let units = &status(&h, cx(&main)).await[0].units;
        let web = units.iter().find(|u| u.name == "web").expect("web");
        let worker = units.iter().find(|u| u.name == "worker").expect("worker");
        assert_eq!(web.label(), "Web shop");
        assert_eq!(worker.label(), "worker");
        assert_eq!(web.ready_check.as_deref(), Some("tcp"));
        assert_eq!((web.state.as_str(), web.shown_state()), ("ready", "ready"));
        assert_eq!(worker.ready_check.as_deref(), Some("started"));
        assert_eq!(
            (worker.state.as_str(), worker.shown_state()),
            ("ready", "running"),
            "alive is not a passed check"
        );
        assert!(web.pty && !worker.pty);
        assert_eq!(web.urls.get(1), web.public_url.as_ref(), "{:?}", web.urls);
        assert_eq!((web.order, worker.order), (Some(0), Some(1)));
        assert_eq!(worker.public_url, None);

        let (session, _frames) = h.session();
        let attached = h
            .call_on(
                Method::Attach,
                json!({"unit": "worker", "interactive": true, "columns": 90, "rows": 20}),
                cx(&main),
                &session,
            )
            .await
            .expect("attach");
        let resized = h
            .call_on(
                Method::Resize,
                json!({"stream_id": attached["stream_id"], "columns": 100, "rows": 30}),
                cx(&main),
                &session,
            )
            .await
            .expect("a pipe resize is no error");
        assert_eq!(resized["applied"], true);
        let worker_pid = worker.pid.expect("pid");
        assert!(
            !h.runner
                .resizes
                .entries()
                .iter()
                .any(|entry| entry.starts_with(&format!("{worker_pid} "))),
            "a pipe unit's runner is never asked to resize"
        );
        session.close();

        let spawned = h.runner.journal.entries().len();
        let again = start(&h, cx(&main), json!({})).await;
        assert!(again.started.is_empty() && !again.whole_lane, "{again:?}");
        assert_eq!(again.already_running.len(), 2);
        let one = start(&h, cx(&main), json!({"units": ["web"]})).await;
        assert_eq!(one.already_running, ["demo@0:web"]);
        assert!(one.started.is_empty());
        assert_eq!(
            h.runner.journal.entries().len(),
            spawned,
            "nothing respawned"
        );
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}
