//! Setup records, task history, rollups, remote hosts, retention and reset.

use devlanes_core::{Actor, ActorKind, ExitOutcome, ProcessIdentity};
use devlanes_state::{
    Error, RemoteHost, RollupRow, SCHEMA_VERSION, Store, TaskRunInput, UnitRunInput,
};

use crate::{lane, project, schema};

#[tokio::test]
/// Migration 009 keeps the boxes of earlier daemons, none of them wanted, and
/// its down script keeps the rows.
async fn migration_adds_unwanted_remote_hosts_and_down_keeps_them() {
    use diesel::{migration::MigrationSource, prelude::*};
    use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
    const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("hosts.db");
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    c.applied_migrations().expect("ledger");
    let migrations = <_ as MigrationSource<diesel::sqlite::Sqlite>>::migrations(&MIGRATIONS)
        .expect("migrations");
    c.run_migrations(&migrations[..8]).expect("v8");
    diesel::insert_into(schema::remote_hosts::table)
        .values((
            schema::remote_hosts::name.eq("box"),
            schema::remote_hosts::transport.eq("ssh"),
            schema::remote_hosts::address.eq("box.example.invalid"),
            schema::remote_hosts::last_connected.eq(Some(5_i64)),
        ))
        .execute(&mut c)
        .expect("legacy host");
    let store = Store::open(&path).await.expect("upgrade");
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
    let hosts = store.remote_hosts().await.expect("hosts");
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].last_connected, Some(5));
    assert!(!hosts[0].wanted, "an earlier daemon's box is not restored");
    assert!(
        store
            .set_remote_host_wanted("box".into(), true)
            .await
            .expect("want")
    );
    store.close().await.expect("close");
    // Later migrations go first; 009 is the one under test.
    while c.applied_migrations().expect("ledger").len() > 9 {
        c.revert_last_migration(MIGRATIONS)
            .expect("down a later migration");
    }
    c.revert_last_migration(MIGRATIONS).expect("down 009");
    let names: Vec<String> = schema::remote_hosts::table
        .select(schema::remote_hosts::name)
        .load(&mut c)
        .expect("hosts after down");
    assert_eq!(names, ["box"]);
    c.run_pending_migrations(MIGRATIONS).expect("redo 009");
}

#[tokio::test]
/// Replacement writes clear nullable fields and history records preserve every value.
async fn all_lifecycle_write_models_round_trip_and_replace() {
    use devlanes_state::{LogSegmentInput, RunSummaryRow};
    let store = Store::open(":memory:").await.expect("store");
    let p = project(&store, "acme").await;
    let lane = lane(&store, p.id, 0).await;
    let run = store
        .record_unit_run(UnitRunInput {
            lane_id: lane.id,
            unit: "web".into(),
            process: ProcessIdentity {
                pid: 10,
                sid: 10,
                start_time: u64::MAX,
            },
            actor: human(),
            log_path: Some("web.log".into()),
            boot_id: None,
        })
        .await
        .expect("run");
    let mut segment = LogSegmentInput {
        unit_run_id: run.id,
        first_seq: 4,
        last_seq: 50,
        min_ts_ms: 100,
        max_ts_ms: 200,
        path: "segment.log".into(),
        compressed: true,
        bytes: 1234,
    };
    store
        .record_log_segment(segment.clone())
        .await
        .expect("segment");
    assert_eq!(
        store.log_segments(run.id).await.expect("segments"),
        [segment.clone()]
    );
    segment.last_seq = 70;
    segment.compressed = false;
    store
        .record_log_segment(segment.clone())
        .await
        .expect("replace segment");
    assert_eq!(
        store.log_segments(run.id).await.expect("segments"),
        [segment]
    );
    let mut summary = RunSummaryRow {
        unit_run_id: run.id,
        series: "web".into(),
        started_ms: 10,
        ended_ms: 500,
        samples: 600,
        mem_p50: 10,
        mem_p95: 20,
        mem_peak: 30,
        cpu_p50: 1.0,
        cpu_p95: 2.0,
        cpu_peak: 3.0,
        rss_peak: 40,
    };
    store
        .record_run_summary(summary.clone())
        .await
        .expect("summary");
    assert_eq!(
        store
            .run_summaries("web".into(), 10)
            .await
            .expect("summaries"),
        [summary.clone()]
    );
    summary.mem_peak = 99;
    store
        .record_run_summary(summary.clone())
        .await
        .expect("replace summary");
    assert_eq!(
        store
            .run_summaries("web".into(), 10)
            .await
            .expect("summaries"),
        [summary]
    );
    let mut row = RollupRow {
        series: "web".into(),
        minute_ms: 60000,
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
        .expect("metrics");
    row.samples = u32::MAX;
    row.mem_max = u64::MAX;
    store
        .insert_rollups(vec![row.clone()])
        .await
        .expect("replace metrics");
    row.mem_max = i64::MAX as u64;
    assert_eq!(
        store.rollups("web".into(), 60000).await.expect("metrics"),
        [row]
    );
    assert!(
        store
            .rollups("web".into(), 60001)
            .await
            .expect("cutoff")
            .is_empty()
    );
    store
        .insert_rollups(Vec::new())
        .await
        .expect("empty insert");
    store
        .record_setup(lane.id, "database".into(), Some("old".into()))
        .await
        .expect("setup");
    store
        .record_setup(lane.id, "database".into(), None)
        .await
        .expect("replace setup");
    assert_eq!(
        store.setup_records(lane.id).await.expect("setup")[0].container_started_at,
        None
    );
    assert_eq!(
        store.clear_setup(lane.id, None).await.expect("clear all"),
        1
    );
    let mut host = RemoteHost {
        name: "box".into(),
        transport: "ssh".into(),
        address: "example.invalid".into(),
        daemon_version: Some("0.1.0".into()),
        last_connected: Some(123),
        wanted: true,
    };
    store.upsert_remote_host(host.clone()).await.expect("host");
    host.daemon_version = None;
    host.last_connected = None;
    store
        .upsert_remote_host(host.clone())
        .await
        .expect("replace host");
    assert_eq!(store.remote_hosts().await.expect("hosts"), [host.clone()]);
    assert!(
        store
            .set_remote_host_wanted("box".into(), false)
            .await
            .expect("disconnect")
    );
    host.wanted = false;
    assert_eq!(store.remote_hosts().await.expect("hosts"), [host]);
    assert!(
        !store
            .set_remote_host_wanted("unknown".into(), true)
            .await
            .expect("unknown host"),
        "no row for an unknown host"
    );
    assert!(matches!(
        store.reset().await,
        Err(Error::ActiveRuns { units: 1, .. })
    ));
    assert_eq!(
        store.active_unit_runs().await.expect("units"),
        std::slice::from_ref(&run),
        "a refused reset keeps the identity of a possibly running process"
    );
    store
        .finish_unit_run(
            run.id,
            ExitOutcome {
                code: None,
                signal: Some(15),
            },
        )
        .await
        .expect("finish");
    store.reset().await.expect("reset");
    assert!(store.active_unit_runs().await.expect("units").is_empty());
    assert!(store.unit_runs(lane.id, 10).await.expect("runs").is_empty());
    assert!(
        store
            .log_segments(run.id)
            .await
            .expect("segments")
            .is_empty()
    );
    assert!(
        store
            .run_summaries("web".into(), 10)
            .await
            .expect("summaries")
            .is_empty()
    );
    assert!(
        store
            .rollups("web".into(), 0)
            .await
            .expect("metrics")
            .is_empty()
    );
}

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

    // A clean refused for an unfinished task changes nothing.
    let pending = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "migrate".into(),
            trigger: "manual".into(),
            actor: human(),
        })
        .await
        .expect("start");
    assert!(matches!(
        store.clean_lane(lane.id).await,
        Err(Error::ActiveRuns { units: 0, tasks: 1 })
    ));
    assert_eq!(
        store.setup_records(lane.id).await.expect("records").len(),
        1
    );
    assert!(
        store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("query")
    );
    // A restarted daemon cannot finish a task it did not start.
    assert_eq!(store.abandon_task_runs().await.expect("abandon"), 1);
    let abandoned = store.task_runs(lane.id, 1).await.expect("runs");
    assert_eq!(abandoned[0].id, pending.id);
    assert!(abandoned[0].ended_at.is_some() && abandoned[0].exit_code.is_none());
    assert_eq!(store.abandon_task_runs().await.expect("abandon"), 0);

    store.clean_lane(lane.id).await.expect("clean");
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
    assert!(store.task_runs(lane.id, 10).await.expect("runs").is_empty());
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
            boot_id: None,
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
            wanted: true,
        })
        .await
        .expect("host");
    assert_eq!(store.remote_hosts().await.expect("hosts").len(), 1);

    let expire_all = devlanes_state::RetentionPolicy {
        run_history: std::time::Duration::ZERO,
        metrics: std::time::Duration::ZERO,
        ..Default::default()
    };
    let pruned = store
        .prune_at(expire_all.clone(), 120_000)
        .await
        .expect("prune");
    assert_eq!(
        (pruned.unit_runs, pruned.metric_rollups),
        (0, 1),
        "unfinished runs stay"
    );
    assert!(matches!(
        store.reset().await,
        Err(Error::ActiveRuns { units: 1, tasks: 0 })
    ));
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
    let finished = store.unit_runs(lane.id, 1).await.expect("runs")[0].clone();
    assert_eq!(
        store
            .prune_at(expire_all, finished.ended_at.expect("ended") + 1)
            .await
            .expect("prune")
            .unit_runs,
        1
    );

    store.reset().await.expect("reset");
    assert!(store.projects().await.expect("projects").is_empty());
    assert!(store.remote_hosts().await.expect("hosts").is_empty());
    assert!(
        !store
            .prune(devlanes_state::RetentionPolicy::default())
            .await
            .expect("periodic prune")
            .budget_exceeded
    );
}

#[tokio::test]
/// Startup peaks come back newest first, are capped per series and age out
/// with metrics retention.
async fn startup_peaks_are_capped_and_pruned() {
    use devlanes_state::{RetentionPolicy, STARTUP_PEAKS_PER_SERIES, StartupPeak};
    let store = Store::open(":memory:").await.expect("store");
    let peak = |series: &str, recorded_ms: u64, peak_bytes: u64| StartupPeak {
        series: series.into(),
        recorded_ms,
        peak_bytes,
        duration_ms: 1_500,
    };
    for index in 0..15_u64 {
        store
            .record_startup_peak(peak("unit:1:1:web", 1_000 + index, index << 20))
            .await
            .expect("record");
    }
    store
        .record_startup_peak(peak("unit:1:1:api", 1_000, 7))
        .await
        .expect("other series");
    let web = store
        .startup_peaks("unit:1:1:web".into(), 100)
        .await
        .expect("peaks");
    assert_eq!(web.len(), STARTUP_PEAKS_PER_SERIES);
    assert_eq!(web[0], peak("unit:1:1:web", 1_014, 14 << 20));
    assert_eq!(web.last().map(|p| p.recorded_ms), Some(1_005));
    assert_eq!(
        store
            .startup_peaks("unit:1:1:api".into(), 5)
            .await
            .expect("api")
            .len(),
        1
    );
    let policy = RetentionPolicy::default();
    let later = i64::try_from(policy.metrics.as_millis()).expect("millis") + 1_010;
    store.prune_at(policy, later).await.expect("prune");
    let left = store
        .startup_peaks("unit:1:1:web".into(), 100)
        .await
        .expect("after prune");
    assert_eq!(
        left.iter().map(|p| p.recorded_ms).collect::<Vec<_>>(),
        [1_014, 1_013, 1_012, 1_011, 1_010]
    );
}
