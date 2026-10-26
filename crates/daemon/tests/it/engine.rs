//! Engine behaviour through the `Handler` API.

use std::{path::Path, time::Duration};

use bytes::Bytes;
use devlanes_daemon::Handler;
use devlanes_ipc::Frame;
use devlanes_proto::{Context, LaneSummary, Method, Page, StartResult, events};
use serde_json::{Value, json};

use crate::{
    LIMIT, Setup, agent, cx, drain_events, eventually, git, harness, repo, temp, worktree,
};

const BASIC: &str = r#"project: demo
services:
  web:
    run: serve
    hosts: ["web.${{ lane.domain }}"]
  worker:
    run: "printenv RUN; sleep"
"#;

#[tokio::test(flavor = "multi_thread")]
/// In a repository without `devlanes.yaml`, `start`, `config check` and `lane
/// new` answer `no_config` with the cwd and the repository root the search
/// stopped at, even when a directory above the repository has a config.
async fn fresh_repository_reports_no_config() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        std::fs::write(tmp.path.join("devlanes.yaml"), "project: outside\n").expect("outer");
        let root = tmp.path.join("fresh");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        git(&root, &["init", "-q", "-b", "main"]);
        let cwd = root.join("src");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        for (method, params) in [
            (Method::Start, json!({})),
            (Method::ConfigCheck, json!({})),
            (Method::LaneNew, json!({"branch": "feature"})),
        ] {
            let error = h
                .call(method, params, cx(&cwd))
                .await
                .expect_err("no config");
            assert_eq!(
                (error.code.as_str(), error.exit_code),
                ("no_config", 3),
                "{method:?}: {error:?}"
            );
            assert_eq!(error.data["path"], json!(cwd));
            assert_eq!(error.data["searched_to"], json!(root));
        }
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `env` for an unknown unit lists the lane's units as candidates.
async fn env_of_an_unknown_unit_lists_candidates() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lanes");
        let error = h
            .call(Method::Env, json!({"unit": "wbe"}), cx(&main))
            .await
            .expect_err("unknown unit");
        assert_eq!((error.code.as_str(), error.exit_code), ("not_found", 3));
        assert_eq!(
            error.data["candidates"],
            json!(["web", "worker"]),
            "{error:?}"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// Lanes visible from `cwd` (no re-list).
async fn status(h: &crate::Harness, context: Context, params: Value) -> Vec<LaneSummary> {
    let value = h
        .call(Method::Status, params, context)
        .await
        .expect("status");
    serde_json::from_value::<Page<LaneSummary>>(value)
        .expect("page")
        .items
}

/// Lanes after a re-list.
async fn lanes(h: &crate::Harness, cwd: &Path) -> Vec<LaneSummary> {
    let value = h
        .call(Method::LaneList, json!({}), cx(cwd))
        .await
        .expect("lane list");
    serde_json::from_value::<Page<LaneSummary>>(value)
        .expect("page")
        .items
}

/// Lane whose worktree path is `path`.
fn lane_at<'a>(lanes: &'a [LaneSummary], path: &Path) -> &'a LaneSummary {
    lanes
        .iter()
        .find(|lane| lane.path == path)
        .unwrap_or_else(|| panic!("no lane at {}", path.display()))
}

/// Starts the cwd's lane.
async fn start(h: &crate::Harness, context: Context, params: Value) -> StartResult {
    let value = h.call(Method::Start, params, context).await.expect("start");
    serde_json::from_value(value).expect("start result")
}

#[tokio::test(flavor = "multi_thread")]
/// Lane 0 follows the primary branch; other worktrees get a lane once devlanes
/// runs in them and keep its index; a worktree moving onto the primary branch
/// takes lane 0 at its next start; an idle lane whose folder is gone is cleaned.
async fn lane_indices_and_primary_moves() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let (a, b, c) = (
            tmp.path.join("wt-a"),
            tmp.path.join("wt-b"),
            tmp.path.join("wt-c"),
        );
        worktree(&main, &a, "feat/a");
        worktree(&main, &b, "feat/b");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let list = lanes(&h, &main).await;
        assert_eq!(lane_at(&list, &main).index, Some(0));
        assert!(
            list.iter().all(|lane| lane.path != a && lane.path != b),
            "existing is not enough: {list:?}"
        );
        lanes(&h, &a).await;
        let list = lanes(&h, &b).await;
        assert_eq!(lane_at(&list, &a).index, Some(1));
        assert_eq!(lane_at(&list, &b).index, Some(2));
        assert_eq!(lane_at(&list, &b).reference, "demo@2");
        assert_eq!(
            lane_at(&list, &b).domain.as_deref(),
            Some("demo-2.localhost")
        );

        git(&main, &["checkout", "-q", "-b", "other"]);
        git(&b, &["checkout", "-q", "main"]);
        let list = lanes(&h, &main).await;
        assert_eq!(lane_at(&list, &b).moves_to, Some(0));
        assert_eq!(lane_at(&list, &main).moves_to, Some(3));

        let result = start(&h, cx(&b), json!({"units": ["worker"]})).await;
        assert_eq!(result.lane, "demo@0");
        let list = lanes(&h, &main).await;
        assert_eq!(lane_at(&list, &b).index, Some(0));
        assert_eq!(lane_at(&list, &main).index, Some(3));

        git(
            &main,
            &["worktree", "remove", "--force", &a.display().to_string()],
        );
        worktree(&main, &c, "feat/c");
        let cleaned = eventually(Duration::from_secs(10), || async {
            let list = lanes(&h, &main).await;
            list.iter().all(|lane| lane.path != a).then_some(())
        })
        .await;
        assert!(
            cleaned.is_some(),
            "an idle lane whose folder is gone is cleaned"
        );
        let list = lanes(&h, &c).await;
        assert_eq!(lane_at(&list, &c).index, Some(1), "lowest free index");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A worktree devlanes ran in, moved and removed, shows in `status` without a
/// re-list request; once removed and idle, its lane is cleaned.
async fn worktrees_are_picked_up_live() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                watch: true,
                ..Setup::default()
            },
        )
        .await;
        assert_eq!(status(&h, cx(&main), json!({})).await.len(), 1);
        let x = tmp.path.join("wt-x");
        worktree(&main, &x, "feat/x");
        // The watcher sees the worktree, but only a command run in it makes a lane.
        let index = eventually(Duration::from_secs(10), || async {
            let all = status(&h, cx(&x), json!({})).await;
            all.iter().find(|l| l.path == x).and_then(|l| l.index)
        })
        .await
        .expect("the worktree devlanes ran in shows up");
        let y = tmp.path.join("wt-y");
        git(
            &main,
            &[
                "worktree",
                "move",
                &x.display().to_string(),
                &y.display().to_string(),
            ],
        );
        let moved = eventually(Duration::from_secs(10), || async {
            let all = status(&h, cx(&main), json!({})).await;
            all.iter().find(|l| l.path == y).and_then(|l| l.index)
        })
        .await
        .expect("moved worktree shows up");
        assert_eq!(moved, index, "a move keeps the lane");
        git(&main, &["worktree", "remove", &y.display().to_string()]);
        let cleaned = eventually(Duration::from_secs(10), || async {
            let all = status(&h, cx(&main), json!({})).await;
            all.iter().all(|l| l.path != y).then_some(())
        })
        .await;
        assert!(
            cleaned.is_some(),
            "the idle lane of a removed worktree is cleaned"
        );
        let start = h
            .call(
                Method::Start,
                json!({}),
                Context {
                    lane: Some(format!("demo@{index}")),
                    ..cx(&main)
                },
            )
            .await
            .expect_err("the lane is gone");
        assert_eq!((start.code.as_str(), start.exit_code), ("not_found", 3));
        h.close().await;
    })
    .await
    .expect("timeout");
}

const PIPELINE: &str = r#"project: pipe
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
  setup:
    database: "echo setup-db"
tasks:
  build: { run: "echo task-build", when: lane.create }
  migrate: { run: "echo task-migrate", when: lane.start }
  seed: { run: "echo task-seed", when: lane.create, after: services }
  manual: { run: "echo manual" }
services:
  api:
    run: "printenv DB_PORT; sleep"
    env: { DB_PORT: "${{ infra.db.port }}" }
"#;

#[tokio::test(flavor = "multi_thread")]
/// infra -> setup -> tasks -> services -> after-services, with create tasks and
/// setup entries only once, start tasks every start, and phase events throughout.
async fn start_pipeline_order_and_idempotence() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", PIPELINE);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let mut bus = h.engine.subscribe();
        let result = start(&h, cx(&main), json!({})).await;
        assert!(result.ready, "{result:?}");
        assert!(result.whole_lane);
        assert_eq!(result.started, ["pipe@0:api"]);
        assert_eq!(
            h.runner.journal.entries(),
            [
                "echo setup-db",
                "echo task-build",
                "echo task-migrate",
                "printenv DB_PORT; sleep",
                "echo task-seed"
            ]
        );
        assert_eq!(h.infra.calls.entries(), ["up 0"]);
        let mut phases = Vec::new();
        while let Ok(event) = bus.try_recv() {
            // Detail on a running phase is covered in `stream`.
            if event.event == events::PHASE && event.data["state"] != "progress" {
                phases.push(format!(
                    "{}:{}",
                    event.data["phase"].as_str().unwrap_or(""),
                    event.data["state"].as_str().unwrap_or("")
                ));
            }
        }
        assert_eq!(
            phases,
            [
                "infra:started",
                "infra:done",
                "setup:started",
                "setup:done",
                "tasks:started",
                "tasks:done",
                "services:started",
                "services:done",
                "after-services:started",
                "after-services:done",
            ]
        );
        let port = eventually(Duration::from_secs(5), || async {
            let logs = h
                .call(
                    Method::Logs,
                    json!({"units": ["api"], "tail": 5}),
                    cx(&main),
                )
                .await
                .ok()?;
            logs["items"][0]["text"].as_str().map(str::to_owned)
        })
        .await
        .expect("api logged");
        assert!(
            port.parse::<u16>().is_ok(),
            "infra port reaches the service env: {port}"
        );
        let lanes = status(&h, cx(&main), json!({})).await;
        let role = |name: &str| {
            lanes[0]
                .units
                .iter()
                .find(|u| u.name == name)
                .and_then(|u| u.role.clone())
        };
        assert_eq!(role("setup.database").as_deref(), Some("setup"));
        assert_eq!(role("migrate").as_deref(), Some("task"));
        assert_eq!(role("api").as_deref(), Some("service"));

        let again = start(&h, cx(&main), json!({})).await;
        assert_eq!(again.already_running, ["pipe@0:api"]);
        assert!(again.started.is_empty() && !again.whole_lane);
        assert_eq!(
            h.runner.journal.entries().len(),
            5,
            "a running lane is untouched"
        );

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        let third = start(&h, cx(&main), json!({})).await;
        assert!(third.ready);
        assert_eq!(
            &h.runner.journal.entries()[5..],
            ["echo task-migrate", "printenv DB_PORT; sleep"]
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

const ENTRIES: &str = r#"project: entries
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
  setup:
    database: "echo setup-db"
  teardown:
    database: "echo teardown-db"
services:
  api: { run: "sleep" }
"#;

#[tokio::test(flavor = "multi_thread")]
/// `run infra.setup.<name>` runs one entry and records it as done, so the next
/// start skips it; `run infra.teardown.<name>` clears that record.
async fn run_infra_entries_keep_the_setup_record() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", ENTRIES);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let ran = h
            .call(
                Method::Run,
                json!({"task": "infra.setup.database"}),
                cx(&main),
            )
            .await
            .expect("run setup entry");
        assert_eq!(ran["unit"], "setup.database");
        assert_eq!(ran["exit_code"], 0);
        assert_eq!(h.runner.journal.entries(), ["echo setup-db"]);

        assert!(start(&h, cx(&main), json!({})).await.ready);
        assert_eq!(
            h.runner.journal.entries(),
            ["echo setup-db", "sleep"],
            "the start found the entry done"
        );
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");

        // In the foreground the answer comes after the record is cleared.
        let torn = h
            .call(
                Method::Run,
                json!({"task": "infra.teardown.database"}),
                cx(&main),
            )
            .await
            .expect("run teardown entry");
        assert_eq!(torn["unit"], "teardown.database");
        assert_eq!(torn["exit_code"], 0);

        assert!(start(&h, cx(&main), json!({})).await.ready);
        assert_eq!(
            h.runner.journal.entries(),
            [
                "echo setup-db",
                "sleep",
                "echo teardown-db",
                "echo setup-db",
                "sleep"
            ],
            "teardown cleared the record"
        );

        // Detached, the answer names the unit the client attaches to.
        let detached = h
            .call(
                Method::Run,
                json!({"task": "infra.setup.database", "detach": true}),
                cx(&main),
            )
            .await
            .expect("run setup entry detached");
        assert_eq!(detached["unit"], "setup.database");
        assert_eq!(detached["detached"], true);
        let again = eventually(Duration::from_secs(5), || async {
            (h.runner.journal.entries().len() == 6).then_some(())
        })
        .await;
        assert!(again.is_some(), "{:?}", h.runner.journal.entries());
        h.close().await;
    })
    .await
    .expect("timeout");
}

const BACKED: &str = r#"project: backed
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
  setup:
    database: { run: "echo setup-db", container: db }
services:
  api: { run: "sleep" }
  web: { run: "sleep" }
"#;

#[tokio::test(flavor = "multi_thread")]
/// An entry backed by a container runs again only when that container
/// restarted: a `run` of the entry records the container's start time like a
/// start does, so a later start into the running lane leaves it alone.
async fn setup_entry_reruns_only_after_its_container_restarted() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BACKED);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let started_at = |at: &str| {
            *h.infra.started_at.lock().expect("lock") = Some(at.to_owned());
        };
        started_at("2026-01-01T00:00:00Z");
        assert!(start(&h, cx(&main), json!({"units": ["api"]})).await.ready);
        assert_eq!(h.runner.journal.entries(), ["echo setup-db", "sleep"]);

        h.call(
            Method::Run,
            json!({"task": "infra.setup.database"}),
            cx(&main),
        )
        .await
        .expect("run setup entry");
        let web = start(&h, cx(&main), json!({"units": ["web"]})).await;
        assert!(web.ready && !web.whole_lane, "{web:?}");
        assert_eq!(
            h.runner.journal.entries(),
            ["echo setup-db", "sleep", "echo setup-db", "sleep"],
            "the run recorded the container's start time"
        );

        h.call(Method::Stop, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("stop web");
        started_at("2026-01-01T01:00:00Z");
        assert!(start(&h, cx(&main), json!({"units": ["web"]})).await.ready);
        assert_eq!(
            &h.runner.journal.entries()[4..],
            ["echo setup-db", "sleep"],
            "the restarted container lost the entry's effect"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A start into a running lane plans only the services it starts and the
/// setup entries; a whole-lane start plans every unit.
async fn start_into_running_lane_plans_only_its_services() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = r#"project: planned
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
  setup:
    database: "echo setup-db"
tasks:
  migrate: { run: "echo migrate", when: lane.start }
services:
  api: { run: "sleep" }
  web: { run: "printenv API_PORT; sleep", env: { API_PORT: "${{ services.api.port }}" } }
  docs: { run: "sleep" }
"#;
        let main = repo(&tmp.path, "r", config);
        let journal = crate::PlanJournal::default();
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                contributors: vec![std::sync::Arc::new(journal.clone())],
                ..Setup::default()
            },
        )
        .await;
        let planned = || {
            let mut planned = journal.planned.entries();
            planned.sort();
            planned.dedup();
            journal.planned.0.lock().expect("lock").clear();
            planned
        };
        assert!(start(&h, cx(&main), json!({"units": ["api"]})).await.ready);
        assert_eq!(
            planned(),
            [
                "InfraCommand database",
                "Service api",
                "Service docs",
                "Service web",
                "Task migrate"
            ]
        );

        let web = start(&h, cx(&main), json!({"units": ["web"]})).await;
        assert!(web.ready && !web.whole_lane, "{web:?}");
        assert_eq!(planned(), ["InfraCommand database", "Service web"]);
        // Expressions still see the services that were not planned.
        let lanes = status(&h, cx(&main), json!({})).await;
        let api_port = lanes[0]
            .units
            .iter()
            .find(|u| u.name == "api")
            .and_then(|u| u.ports.get("default").copied())
            .expect("api port");
        let printed = eventually(Duration::from_secs(5), || async {
            let logs = h
                .call(
                    Method::Logs,
                    json!({"units": ["web"], "tail": 5}),
                    cx(&main),
                )
                .await
                .ok()?;
            logs["items"][0]["text"].as_str().map(str::to_owned)
        })
        .await
        .expect("web logged");
        assert_eq!(printed, api_port.to_string());
        assert_eq!(
            h.runner.journal.entries(),
            [
                "echo setup-db",
                "echo migrate",
                "sleep",
                "printenv API_PORT; sleep"
            ],
            "neither setup nor tasks ran again"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// An entry run on a lane this daemon never started still gets the Compose
/// env its `docker compose exec` needs.
async fn run_infra_entry_gets_the_compose_env_without_a_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = ENTRIES.replace("echo setup-db", "printenv COMPOSE_PROJECT_NAME");
        let main = repo(&tmp.path, "r", &config);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        h.call(
            Method::Run,
            json!({"task": "infra.setup.database"}),
            cx(&main),
        )
        .await
        .expect("run setup entry");
        let logs = h
            .call(
                Method::Logs,
                json!({"units": ["setup.database"], "tail": 5}),
                cx(&main),
            )
            .await
            .expect("logs");
        assert_eq!(logs["items"][0]["text"], "fake", "{logs}");
        assert!(
            !h.infra
                .calls
                .entries()
                .iter()
                .any(|call| call.starts_with("up")),
            "nothing started"
        );
        let lanes = status(&h, cx(&main), json!({})).await;
        assert!(
            lanes[0].containers.is_empty(),
            "status does not show infra that never came up: {:?}",
            lanes[0].containers
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A failing entry run on its own reports its exit code, `ignore_errors` or
/// not; `ignore_errors` still records it as done for later starts.
async fn run_infra_entry_reports_a_failure() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: entries\ninfra:\n  containers:\n    db: { image: \"postgres:16\", ports: { pg: 5432 }, per_lane: true }\n  setup:\n    broken: \"exit 3\"\n    tolerated: { run: \"exit 4\", ignore_errors: true }\nservices:\n  api: { run: \"sleep\" }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        for (entry, code) in [("broken", 3), ("tolerated", 4)] {
            let ran = h
                .call(
                    Method::Run,
                    json!({"task": format!("infra.setup.{entry}")}),
                    cx(&main),
                )
                .await
                .expect("run answers");
            assert_eq!(ran["exit_code"], code, "{entry}: {ran}");
            assert_eq!(ran["state"], "failed", "{entry}: {ran}");
        }
        // Only the failure without `ignore_errors` runs again at the start.
        let result = start(&h, cx(&main), json!({})).await;
        let entries = h.runner.journal.entries();
        assert_eq!(
            entries.iter().filter(|e| *e == "exit 3").count(),
            2,
            "{result:?} {entries:?}"
        );
        assert_eq!(entries.iter().filter(|e| *e == "exit 4").count(), 1);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// An entry the config does not define is `not_found`, listing the entries.
async fn run_infra_entry_not_found_lists_entries() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", ENTRIES);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let error = h
            .call(Method::Run, json!({"task": "infra.setup.nope"}), cx(&main))
            .await
            .expect_err("unknown entry");
        assert_eq!((error.code.as_str(), error.exit_code), ("not_found", 3));
        assert_eq!(
            error.data["candidates"],
            json!(["infra.setup.database", "infra.teardown.database"])
        );
        assert!(h.runner.journal.entries().is_empty());
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A failed service skips `after: services` tasks and reports its log tail.
async fn after_services_skip_on_failure() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = r#"project: skip
tasks:
  seed: { run: "echo seed", when: lane.create, after: services }
services:
  bad: { run: fail, ready: { log: never } }
  good: { run: sleep }
"#;
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let result = start(&h, cx(&main), json!({})).await;
        assert!(!result.ready);
        assert_eq!(result.skipped_tasks, ["seed"]);
        let failure = result
            .failures
            .iter()
            .find(|f| f.unit == "bad")
            .expect("bad failed");
        assert_eq!(failure.exit_code, Some(1));
        assert!(
            failure.log_tail.iter().any(|l| l.contains("boom")),
            "{failure:?}"
        );
        assert!(!h.runner.journal.entries().iter().any(|e| e == "echo seed"));
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// StartResult says what this request started, so `devlanes with` stops only that.
async fn start_result_ownership_stop_and_restart() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let first = start(&h, cx(&main), json!({"units": ["worker"]})).await;
        assert_eq!(first.started, ["demo@0:worker"]);
        assert!(first.whole_lane && first.ready);
        let second = start(&h, cx(&main), json!({})).await;
        assert_eq!(second.already_running, ["demo@0:worker"]);
        assert_eq!(second.started, ["demo@0:web"]);
        assert!(!second.whole_lane);
        assert!(second.ready, "{second:?}");

        let before = second
            .units
            .iter()
            .find(|u| u.name == "web")
            .and_then(|u| u.pid);
        let restarted: StartResult = serde_json::from_value(
            h.call(Method::Restart, json!({"units": ["web"]}), cx(&main))
                .await
                .expect("restart"),
        )
        .expect("result");
        let after = restarted
            .units
            .iter()
            .find(|u| u.name == "web")
            .and_then(|u| u.pid);
        assert!(restarted.ready, "{restarted:?}");
        assert_ne!(before, after);

        let stopped = h
            .call(Method::Stop, json!({"units": ["demo@0:worker"]}), cx(&main))
            .await
            .expect("stop");
        assert_eq!(stopped["items"][0]["name"], "worker");
        assert_eq!(stopped["items"][0]["state"], "stopped");
        let lane = &status(&h, cx(&main), json!({})).await[0];
        assert_eq!(lane.state, "running");
        let missing = h
            .call(Method::Restart, json!({"units": ["nope"]}), cx(&main))
            .await;
        let error = missing.expect_err("unknown unit");
        assert_eq!((error.code.as_str(), error.exit_code), ("not_found", 3));
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `exec` adopts a command as a unit whose logs and terminal are reachable.
async fn exec_logs_and_attach() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let unit = h
            .call(
                Method::Exec,
                json!({"name": "tool", "command": ["/bin/sh", "-c", "echo hello; sleep"]}),
                cx(&main),
            )
            .await
            .expect("exec");
        assert_eq!(unit["kind"], "exec");
        let logs = eventually(Duration::from_secs(5), || async {
            let page = h
                .call(
                    Method::Logs,
                    json!({"units": ["tool"], "tail": 10}),
                    cx(&main),
                )
                .await
                .ok()?;
            page["items"]
                .as_array()
                .filter(|items| !items.is_empty())
                .cloned()
        })
        .await
        .expect("logs");
        assert_eq!(logs[0]["text"], "hello");

        let (session, mut frames) = h.session();
        let attached = h
            .call_on(
                Method::Attach,
                json!({"unit": "tool", "interactive": true, "columns": 80, "rows": 24}),
                cx(&main),
                &session,
            )
            .await
            .expect("attach");
        let stream = attached["stream_id"].as_u64().expect("stream id");
        let snapshot = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .expect("snapshot")
            .expect("frame");
        let Frame::Bytes { stream_id, data } = snapshot else {
            panic!("expected bytes")
        };
        assert_eq!(stream_id, stream);
        assert!(String::from_utf8_lossy(&data).contains("hello"));
        h.engine
            .input(stream, Bytes::from_static(b"ping"), session.clone())
            .await
            .expect("input");
        let echoed = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .expect("echo")
            .expect("frame");
        let Frame::Bytes { data, .. } = echoed else {
            panic!("expected bytes")
        };
        assert!(String::from_utf8_lossy(&data).contains("ping"));
        let resized = h
            .call_on(
                Method::Resize,
                json!({"stream_id": stream, "columns": 100, "rows": 30}),
                cx(&main),
                &session,
            )
            .await
            .expect("resize");
        assert_eq!(resized["applied"], true);
        let (other, _) = h.session();
        assert!(
            h.engine
                .input(stream, Bytes::from_static(b"x"), other)
                .await
                .is_err(),
            "streams belong to their connection"
        );
        session.close();
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `stop --mine` stops only units started by the caller's session.
async fn stop_mine_filters_by_actor() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let mut mine = cx(&main);
        mine.actor = agent("s1");
        h.call(
            Method::Exec,
            json!({"name": "agent-job", "command": ["sleep"]}),
            mine.clone(),
        )
        .await
        .expect("exec");
        h.call(
            Method::Exec,
            json!({"name": "human-job", "command": ["sleep"]}),
            cx(&main),
        )
        .await
        .expect("exec");
        let outside = Context {
            cwd: tmp.path.clone(),
            ..mine.clone()
        };
        let stopped = h
            .call(Method::Stop, json!({"mine": true}), outside)
            .await
            .expect("stop mine");
        let names: Vec<&str> = stopped["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|u| u["name"].as_str())
            .collect();
        assert_eq!(names, ["agent-job"]);
        let lane = &status(&h, cx(&main), json!({})).await[0];
        let human = lane
            .units
            .iter()
            .find(|u| u.name == "human-job")
            .expect("human job");
        assert_eq!(human.state, "ready");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Lane references resolve by number and branch; misses and ambiguity list candidates.
async fn lane_references_and_candidates() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let alpha = repo(
            &tmp.path,
            "a",
            &BASIC.replace("project: demo", "project: alpha"),
        );
        let beta = repo(
            &tmp.path,
            "b",
            &BASIC.replace("project: demo", "project: beta"),
        );
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        lanes(&h, &alpha).await;
        lanes(&h, &beta).await;
        let at = |lane: &str| Context {
            cwd: tmp.path.clone(),
            lane: Some(lane.into()),
            ..cx(&tmp.path)
        };
        let ambiguous = h
            .call(Method::Status, json!({}), at("0"))
            .await
            .expect_err("ambiguous");
        assert_eq!(
            (ambiguous.code.as_str(), ambiguous.exit_code),
            ("ambiguous", 3)
        );
        assert_eq!(ambiguous.data["candidates"], json!(["alpha@0", "beta@0"]));
        let in_alpha = Context {
            lane: Some("99".into()),
            ..cx(&alpha)
        };
        let missing = h
            .call(Method::Status, json!({}), in_alpha)
            .await
            .expect_err("missing");
        assert_eq!((missing.code.as_str(), missing.exit_code), ("not_found", 3));
        assert_eq!(missing.data["candidates"], json!(["alpha@0"]));
        let by_branch = status(&h, at("beta@main"), json!({})).await;
        assert_eq!(by_branch[0].reference, "beta@0");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Ports survive a daemon restart and adding a service never shifts existing ones.
async fn port_allocation_is_stable() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let home = tmp.path.join("home");
        let port = |lanes: &[LaneSummary], unit: &str| {
            lanes[0]
                .units
                .iter()
                .find(|u| u.name == unit)
                .and_then(|u| u.ports.get("default").copied())
        };
        let first = harness(&home, Setup::default()).await;
        let before = lanes(&first, &main).await;
        let web = port(&before, "web").expect("web port");
        let worker = port(&before, "worker").expect("worker port");
        assert_ne!(web, worker);
        first.close().await;
        drop(first);
        let config = BASIC.replace("services:\n", "services:\n  api:\n    run: sleep\n");
        std::fs::write(main.join("devlanes.yaml"), config).expect("config");
        let second = harness(&home, Setup::default()).await;
        let after = lanes(&second, &main).await;
        assert_eq!(port(&after, "web"), Some(web));
        assert_eq!(port(&after, "worker"), Some(worker));
        assert!(port(&after, "api").is_some_and(|p| p != web && p != worker));
        second.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `env --lane-env` returns DEVLANES_* and a public URL per service; `explain`
/// reports layers.
async fn lane_env_and_explain() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let env = h
            .call(Method::Env, json!({"lane_env": true}), cx(&main))
            .await
            .expect("env");
        assert_eq!(env["env"]["DEVLANES_LANE"], "0");
        assert_eq!(env["env"]["DEVLANES_PROJECT"], "demo");
        assert_eq!(
            env["env"]["DEVLANES_PUBLIC_URL_WEB"],
            "https://web.demo.localhost"
        );
        assert!(
            env["env"]["DEVLANES_PUBLIC_URL_WORKER"]
                .as_str()
                .is_some_and(|u| u.starts_with("http://127.0.0.1:"))
        );
        let explained = h
            .call(
                Method::Env,
                json!({"unit": "web", "explain": true}),
                cx(&main),
            )
            .await
            .expect("explain");
        let items = explained["items"].as_array().expect("items");
        let port = items.iter().find(|i| i["key"] == "PORT").expect("PORT");
        assert_eq!(port["layer"], "devlanes");
        let path = items.iter().find(|i| i["key"] == "PATH").expect("PATH");
        assert_eq!(path["layer"], "client");
        let url = h
            .call(Method::Url, json!({"unit": "web"}), cx(&main))
            .await
            .expect("url");
        assert_eq!(url["url"], "https://web.demo.localhost");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `status` covers the cwd's project; `all: true` covers everything.
async fn status_scope() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let alpha = repo(
            &tmp.path,
            "a",
            &BASIC.replace("project: demo", "project: alpha"),
        );
        let beta = repo(
            &tmp.path,
            "b",
            &BASIC.replace("project: demo", "project: beta"),
        );
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        lanes(&h, &beta).await;
        let local = status(&h, cx(&alpha), json!({})).await;
        assert_eq!(
            local.iter().map(|l| l.project.as_str()).collect::<Vec<_>>(),
            ["alpha"]
        );
        let all = status(&h, cx(&alpha), json!({"all": true})).await;
        assert_eq!(
            all.iter().map(|l| l.project.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `run` needs `yes` for confirm tasks, streams logs, and hands interactive tasks
/// to an attaching terminal; `task.list` has each task's last run (none for a
/// task that never ran) and ended runs carry `finished_at_ms`.
async fn run_confirm_and_interactive() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = format!(
            "{BASIC}tasks:\n  reset: {{ run: \"echo resetting\", confirm: \"Drops it\" }}\n  login: {{ run: sleep, interactive: true }}\n  broken: {{ run: \"exit 3\" }}\n  never: {{ run: \"echo never\" }}\n"
        );
        let main = repo(&tmp.path, "r", &config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let refused = h.call(Method::Run, json!({"task": "reset"}), cx(&main)).await.expect_err("confirm");
        assert_eq!((refused.code.as_str(), refused.exit_code), ("confirmation_required", 2));
        let (session, mut frames) = h.session();
        let yes = Context { yes: true, ..cx(&main) };
        let done = h.call_on(Method::Run, json!({"task": "reset"}), yes, &session).await.expect("run");
        assert_eq!(done["exit_code"], 0);
        assert_eq!(done["interactive"], false);
        let logs: Vec<String> = drain_events(&mut frames)
            .into_iter()
            .filter(|e| e.event == events::LOG)
            .filter_map(|e| e.data["line"]["text"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(logs, ["resetting"]);
        let failed = h.call(Method::Run, json!({"task": "broken"}), cx(&main)).await.expect("run broken");
        assert_eq!(failed["exit_code"], 3);
        let no_terminal = h.call(Method::Run, json!({"task": "login"}), cx(&main)).await.expect_err("terminal");
        assert_eq!(no_terminal.code, "terminal_required");
        let interactive = h.call(Method::Run, json!({"task": "login", "terminal": true}), cx(&main)).await.expect("interactive");
        assert_eq!(interactive["interactive"], true);
        assert_eq!(interactive["unit"], "login");
        let tasks = h.call(Method::TaskList, json!({}), cx(&main)).await.expect("task list");
        let items = tasks["items"].as_array().expect("items");
        assert_eq!(items.len(), 4);
        let last = |name: &str| items.iter().find(|t| t["name"] == name).expect(name)["last_run"].clone();
        assert_eq!(last("reset")["state"], "done");
        assert_eq!(last("reset")["exit_code"], 0);
        assert!(last("reset")["finished_at_ms"].as_i64().is_some());
        assert_eq!((last("broken")["state"].clone(), last("broken")["exit_code"].clone()), (json!("failed"), json!(3)));
        assert_eq!(last("login")["state"], "running");
        assert!(last("login")["finished_at_ms"].is_null());
        assert!(last("never").is_null(), "a task that never ran has no last run");
        let lanes: Page<LaneSummary> =
            serde_json::from_value(h.call(Method::Status, json!({}), cx(&main)).await.expect("status")).expect("page");
        let broken = lanes.items[0].units.iter().find(|u| u.name == "broken").expect("broken");
        assert!(broken.finished_at_ms.is_some_and(|ms| ms >= broken.started_at_ms.unwrap_or(0)));
        assert_eq!(broken.role.as_deref(), Some("task"));
        h.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `logs --all --since start` returns only the current lane start's lines.
async fn logs_since_lane_start() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        start(
            &h,
            cx(&main),
            json!({"units": ["worker"], "env": {"RUN": "one"}}),
        )
        .await;
        eventually(Duration::from_secs(5), || async {
            let page = h
                .call(Method::Logs, json!({"units": ["worker"]}), cx(&main))
                .await
                .ok()?;
            (!page["items"].as_array()?.is_empty()).then_some(())
        })
        .await
        .expect("first run logged");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        tokio::time::sleep(Duration::from_millis(20)).await;
        start(
            &h,
            cx(&main),
            json!({"units": ["worker"], "env": {"RUN": "two"}}),
        )
        .await;
        let lines = eventually(Duration::from_secs(5), || async {
            let value = h
                .call(
                    Method::Logs,
                    json!({"all": true, "since": "start"}),
                    cx(&main),
                )
                .await
                .ok()?;
            let worker = value["units"]
                .as_array()?
                .iter()
                .find(|u| u["unit"] == "worker")?
                .clone();
            let lines: Vec<String> = worker["lines"]
                .as_array()?
                .iter()
                .filter_map(|l| l["text"].as_str().map(str::to_owned))
                .collect();
            (!lines.is_empty()).then_some(lines)
        })
        .await
        .expect("second run logged");
        assert_eq!(lines, ["two"]);
        let all = h
            .call(Method::Logs, json!({"units": ["worker"]}), cx(&main))
            .await
            .expect("logs");
        assert_eq!(
            all["items"].as_array().map(Vec::len),
            Some(2),
            "earlier runs stay queryable"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A worktree devlanes ran in without a config shows `no-config`; one it never
/// ran in is not listed; reset-state needs `yes`.
async fn no_config_and_reset_state() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", BASIC);
        let bare = tmp.path.join("wt-bare");
        worktree(&main, &bare, "feat/bare");
        std::fs::remove_file(bare.join("devlanes.yaml")).expect("remove config");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let list = lanes(&h, &main).await;
        assert!(list.iter().all(|lane| lane.path != bare), "{list:?}");
        let list = lanes(&h, &bare).await;
        assert_eq!(lane_at(&list, &bare).state, "no-config");
        assert_eq!(lane_at(&list, &bare).index, None);
        let refused = h
            .call(Method::DaemonResetState, json!({}), cx(&main))
            .await
            .expect_err("confirm");
        assert_eq!(refused.exit_code, 2);
        let reset = h
            .call(
                Method::DaemonResetState,
                json!({}),
                Context {
                    yes: true,
                    ..cx(&main)
                },
            )
            .await
            .expect("reset");
        assert_eq!(reset["stopped"], 0);
        h.close().await;
    })
    .await
    .expect("timeout");
}
