//! Setup records, task history, rollups, remote hosts, retention and reset.

use devlanes_core::{Actor, ActorKind, ExitOutcome, ProcessIdentity};
use devlanes_state::{RemoteHost, RollupRow, SCHEMA_VERSION, Store, TaskRunInput, UnitRunInput};

use crate::{lane, project};

/// A human actor.
fn human() -> Actor {
    Actor {
        kind: ActorKind::Human,
        agent_id: None,
        session_id: None,
    }
}

#[tokio::test]
/// Setup entries keep their container timestamp; `lane.create` succeeds once.
async fn setup_and_task_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("state.db"))
        .await
        .expect("store");
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
    let project = project(&store, "demo").await;
    store
        .set_project_config_dir(project.id, "apps/web".into())
        .await
        .expect("config dir");
    assert_eq!(
        store.project_config_dirs().await.expect("dirs"),
        [(project.id, "apps/web".into())]
    );
    let lane = lane(&store, project.id, 1).await;
    store
        .record_setup(lane.id, "vhost".into(), Some("2026-10-21T10:00:00Z".into()))
        .await
        .expect("setup");
    store
        .record_setup(lane.id, "database".into(), None)
        .await
        .expect("setup");
    let records = store.setup_records(lane.id).await.expect("records");
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1].container_started_at.as_deref(),
        Some("2026-10-21T10:00:00Z")
    );
    assert_eq!(
        store
            .clear_setup(lane.id, Some("database".into()))
            .await
            .expect("clear"),
        1
    );

    assert!(
        !store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("query")
    );
    let failed = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "seed".into(),
            trigger: "lane.create".into(),
            actor: human(),
        })
        .await
        .expect("start");
    store
        .finish_task_run(failed.id, Some(1))
        .await
        .expect("finish");
    assert!(
        !store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("query")
    );
    let ok = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "seed".into(),
            trigger: "lane.create".into(),
            actor: human(),
        })
        .await
        .expect("start");
    store.finish_task_run(ok.id, Some(0)).await.expect("finish");
    store
        .finish_task_run(ok.id, Some(9))
        .await
        .expect("idempotent");
    assert!(
        store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("query")
    );
    let runs = store.task_runs(lane.id, 10).await.expect("runs");
    assert_eq!(runs[0].exit_code, Some(0));

    store.forget_lane_lifecycle(lane.id).await.expect("forget");
    assert!(
        store
            .setup_records(lane.id)
            .await
            .expect("records")
            .is_empty()
    );
    assert!(
        !store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("query")
    );
}

#[tokio::test]
/// Rollups, summaries and remote hosts round-trip; retention keeps unfinished runs.
async fn history_retention_and_reset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("state.db"))
        .await
        .expect("store");
    let project = project(&store, "demo").await;
    let lane = lane(&store, project.id, 0).await;
    let run = store
        .record_unit_run(UnitRunInput {
            lane_id: lane.id,
            unit: "web".into(),
            process: ProcessIdentity {
                pid: 10,
                sid: 10,
                start_time: 1,
            },
            actor: human(),
            log_path: None,
        })
        .await
        .expect("run");
    let row = RollupRow {
        series: "unit:1:1:web".into(),
        minute_ms: 60_000,
        samples: 60,
        cpu_min: 1.0,
        cpu_avg: 2.0,
        cpu_max: 3.0,
        mem_min: 10,
        mem_avg: 20,
        mem_max: 30,
        rss_max: 40,
    };
    store
        .insert_rollups(vec![row.clone()])
        .await
        .expect("rollups");
    assert_eq!(
        store.rollups(row.series.clone(), 0).await.expect("query"),
        [row]
    );
    store
        .upsert_remote_host(RemoteHost {
            name: "box".into(),
            transport: "ssh".into(),
            address: "box.example".into(),
            daemon_version: Some("0.1.0".into()),
            last_connected: Some(1),
        })
        .await
        .expect("host");
    assert_eq!(store.remote_hosts().await.expect("hosts").len(), 1);

    let pruned = store.prune_history(i64::MAX, 120_000).await.expect("prune");
    assert_eq!(
        (pruned.unit_runs, pruned.rollups),
        (0, 1),
        "unfinished runs stay"
    );
    store
        .finish_unit_run(
            run.id,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        )
        .await
        .expect("finish");
    assert_eq!(
        store
            .prune_history(i64::MAX, 0)
            .await
            .expect("prune")
            .unit_runs,
        1
    );

    store.reset().await.expect("reset");
    assert!(store.projects().await.expect("projects").is_empty());
    assert!(store.remote_hosts().await.expect("hosts").is_empty());
}
