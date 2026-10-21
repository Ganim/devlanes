//! Deterministic age/size retention and preservation of live registry state.

use super::{lane, project, request, schema};
use devlanes_core::{Actor, ActorKind, ExitOutcome, LaneId, ProcessIdentity};
use devlanes_state::*;
use diesel::{connection::SimpleConnection, prelude::*};
use std::time::Duration;

const DAY: i64 = 86_400_000;
const NOW: i64 = 100 * DAY;

/// Supplies attribution without reading the test runner's environment.
fn human() -> Actor {
    Actor {
        kind: ActorKind::Human,
        agent_id: None,
        session_id: None,
    }
}

/// Creates a run through the public API so identity and foreign-key validation apply.
async fn run(store: &Store, lane: LaneId, name: &str, finished: bool) -> UnitRun {
    let row = store
        .record_unit_run(UnitRunInput {
            lane_id: lane,
            unit: name.into(),
            process: ProcessIdentity {
                pid: 42,
                sid: 42,
                start_time: 1,
            },
            actor: human(),
            log_path: None,
        })
        .await
        .expect("run");
    if finished {
        store
            .finish_unit_run(
                row.id,
                ExitOutcome {
                    code: Some(0),
                    signal: None,
                },
            )
            .await
            .expect("finish");
    }
    row
}

/// A metric minute with a caller-controlled clock and payload size.
fn metric(series: String, minute: u64) -> RollupRow {
    RollupRow {
        series,
        minute_ms: minute,
        samples: 1,
        cpu_min: 1.0,
        cpu_avg: 2.0,
        cpu_max: 3.0,
        mem_min: 10,
        mem_avg: 20,
        mem_max: 30,
        rss_max: 40,
    }
}

/// Disables age expiration so pressure tests isolate the byte budget.
fn budget(bytes: u64) -> RetentionPolicy {
    RetentionPolicy {
        run_history: Duration::MAX,
        metrics: Duration::MAX,
        logs: Duration::MAX,
        remote_hosts: Duration::MAX,
        max_size_bytes: bytes,
    }
}

#[tokio::test]
/// Each group follows its cutoff; unfinished runs survive expired closed metadata.
async fn age_retention_preserves_live_rows_and_once_markers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let p = project(&store, "acme").await;
    let lane = lane(&store, p.id, 0).await;
    let allocation = store
        .allocate(request(BlockOwner::Lane(lane.id), 2))
        .await
        .expect("allocation");
    store
        .record_setup(lane.id, "database".into(), None)
        .await
        .expect("setup");
    let old = run(&store, lane.id, "old", true).await;
    let boundary = run(&store, lane.id, "boundary", true).await;
    let active = run(&store, lane.id, "active", false).await;
    let task = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "seed".into(),
            trigger: "lane.create".into(),
            actor: human(),
        })
        .await
        .expect("task");
    store
        .finish_task_run(task.id, Some(0))
        .await
        .expect("task finish");
    let active_task = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "busy".into(),
            trigger: "manual".into(),
            actor: human(),
        })
        .await
        .expect("active task");
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    diesel::update(schema::unit_runs::table.find(old.id.0))
        .set(schema::unit_runs::ended_at.eq(DAY))
        .execute(&mut c)
        .expect("old clock");
    diesel::update(schema::unit_runs::table.find(boundary.id.0))
        .set(schema::unit_runs::ended_at.eq(70 * DAY))
        .execute(&mut c)
        .expect("boundary clock");
    diesel::update(schema::task_runs::table.find(task.id.0))
        .set(schema::task_runs::ended_at.eq(DAY))
        .execute(&mut c)
        .expect("task clock");
    for run in [&old, &active] {
        store
            .record_log_segment(LogSegmentInput {
                unit_run_id: run.id,
                first_seq: 0,
                last_seq: 10,
                min_ts_ms: DAY as u64,
                max_ts_ms: DAY as u64,
                path: format!("{}.log", run.id.0).into(),
                compressed: false,
                bytes: 100,
            })
            .await
            .expect("segment");
        store
            .record_run_summary(RunSummaryRow {
                unit_run_id: run.id,
                series: "web".into(),
                started_ms: 0,
                ended_ms: DAY as u64,
                samples: 1,
                mem_p50: 10,
                mem_p95: 20,
                mem_peak: 30,
                cpu_p50: 1.0,
                cpu_p95: 2.0,
                cpu_peak: 3.0,
                rss_peak: 40,
            })
            .await
            .expect("summary");
    }
    store
        .insert_rollups(vec![
            metric("web".into(), DAY as u64),
            metric("web".into(), (70 * DAY) as u64),
        ])
        .await
        .expect("metrics");
    for (name, clock) in [
        ("expired", Some(9 * DAY)),
        ("boundary", Some(10 * DAY)),
        ("unknown", None),
    ] {
        store
            .upsert_remote_host(RemoteHost {
                name: name.into(),
                transport: "ssh".into(),
                address: "example.invalid".into(),
                daemon_version: None,
                last_connected: clock,
            })
            .await
            .expect("remote");
    }
    let report = store
        .prune_at(RetentionPolicy::default(), NOW)
        .await
        .expect("prune");
    assert_eq!(
        (
            report.unit_runs,
            report.task_runs,
            report.metric_rollups,
            report.run_summaries,
            report.log_segments,
            report.remote_hosts
        ),
        (1, 1, 1, 2, 2, 1)
    );
    assert!(!report.budget_exceeded);
    assert_eq!(
        store.active_unit_runs().await.expect("active")[0].id,
        active.id
    );
    assert_eq!(
        store.task_runs(lane.id, 10).await.expect("tasks")[0].id,
        active_task.id
    );
    assert!(
        store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("once marker")
    );
    assert_eq!(store.setup_records(lane.id).await.expect("setup").len(), 1);
    assert_eq!(
        store
            .allocation(BlockOwner::Lane(lane.id))
            .await
            .expect("allocation"),
        Some(allocation)
    );
    assert!(
        store
            .log_segments(active.id)
            .await
            .expect("expired closed segments")
            .is_empty()
    );
    assert!(
        store
            .run_summaries("web".into(), 10)
            .await
            .expect("expired summaries")
            .is_empty()
    );
    assert_eq!(store.lanes(None).await.expect("lanes").len(), 1);
    let second = store
        .prune_at(RetentionPolicy::default(), NOW)
        .await
        .expect("idempotent");
    assert_eq!(
        (
            second.unit_runs,
            second.task_runs,
            second.metric_rollups,
            second.run_summaries,
            second.log_segments,
            second.remote_hosts
        ),
        (0, 0, 0, 0, 0, 0)
    );
}

#[tokio::test]
/// Parent rows remain available while logs have a longer configured lifetime.
async fn independent_child_retention_and_explicit_clock() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let p = project(&store, "acme").await;
    let lane = lane(&store, p.id, 0).await;
    let run = run(&store, lane.id, "web", true).await;
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    diesel::update(schema::unit_runs::table.find(run.id.0))
        .set(schema::unit_runs::ended_at.eq(DAY))
        .execute(&mut c)
        .expect("clock");
    store
        .record_log_segment(LogSegmentInput {
            unit_run_id: run.id,
            first_seq: 0,
            last_seq: 1,
            min_ts_ms: DAY as u64,
            max_ts_ms: DAY as u64,
            path: "web.log".into(),
            compressed: false,
            bytes: 20,
        })
        .await
        .expect("segment");
    let policy = RetentionPolicy {
        logs: Duration::from_secs(200 * 86_400),
        ..Default::default()
    };
    assert_eq!(
        store
            .prune_at(policy.clone(), NOW)
            .await
            .expect("prune")
            .unit_runs,
        0
    );
    let removed = store
        .prune_at(policy, 202 * DAY)
        .await
        .expect("later prune");
    assert_eq!((removed.unit_runs, removed.log_segments), (1, 1));
    assert!(
        store
            .prune_at(RetentionPolicy::default(), -1)
            .await
            .is_err()
    );
    assert!(store.prune_at(budget(0), NOW).await.is_err());
}

#[tokio::test]
/// Pressure evicts oldest metrics before history and returns freed file pages.
async fn size_budget_reclaims_pages_and_keeps_history_until_metrics_are_gone() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let p = project(&store, "acme").await;
    let lane = lane(&store, p.id, 0).await;
    let completed = run(&store, lane.id, "completed", true).await;
    let active = run(&store, lane.id, "active", false).await;
    let completed_task = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "seed".into(),
            trigger: "lane.create".into(),
            actor: human(),
        })
        .await
        .expect("task");
    store
        .finish_task_run(completed_task.id, Some(0))
        .await
        .expect("completed task");
    let active_task = store
        .start_task_run(TaskRunInput {
            lane_id: lane.id,
            task: "active-task".into(),
            trigger: "manual".into(),
            actor: human(),
        })
        .await
        .expect("active task");
    store
        .record_log_segment(LogSegmentInput {
            unit_run_id: active.id,
            first_seq: 1,
            last_seq: 2,
            min_ts_ms: 1,
            max_ts_ms: 2,
            path: "closed.log".into(),
            compressed: false,
            bytes: 100,
        })
        .await
        .expect("closed segment");
    store
        .record_run_summary(RunSummaryRow {
            unit_run_id: completed.id,
            series: "web".into(),
            started_ms: 1,
            ended_ms: 3,
            samples: 1,
            mem_p50: 1,
            mem_p95: 1,
            mem_peak: 1,
            cpu_p50: 1.0,
            cpu_p95: 1.0,
            cpu_peak: 1.0,
            rss_peak: 1,
        })
        .await
        .expect("summary");
    store
        .upsert_remote_host(RemoteHost {
            name: "box".into(),
            transport: "ssh".into(),
            address: "example.invalid".into(),
            daemon_version: None,
            last_connected: Some(4),
        })
        .await
        .expect("host");
    let allocation = store
        .allocate(request(BlockOwner::Lane(lane.id), 1))
        .await
        .expect("allocate");
    let baseline = store
        .prune_at(budget(u64::MAX), NOW)
        .await
        .expect("baseline")
        .bytes_after;
    store
        .insert_rollups(
            (0..96)
                .map(|i| metric(format!("series-{i:03}-{}", "x".repeat(4096)), i))
                .collect(),
        )
        .await
        .expect("metrics");
    let report = store
        .prune_at(budget(baseline + 64 * 1024), NOW)
        .await
        .expect("prune");
    assert!(
        report.metric_rollups > 0 && report.metric_rollups < 96,
        "{report:?}"
    );
    assert_eq!(report.unit_runs, 0);
    assert!(report.bytes_after < report.bytes_before, "{report:?}");
    assert!(!report.budget_exceeded, "{report:?}");
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    let oldest = schema::metric_rollups::table
        .select(schema::metric_rollups::minute)
        .order(schema::metric_rollups::minute)
        .first::<i64>(&mut c)
        .expect("remaining metrics");
    assert_eq!(oldest as u64, report.metric_rollups);
    #[derive(diesel::QueryableByName)]
    struct Vacuum {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        auto_vacuum: i32,
    }
    assert_eq!(
        diesel::sql_query("PRAGMA auto_vacuum")
            .get_result::<Vacuum>(&mut c)
            .expect("vacuum mode")
            .auto_vacuum,
        2
    );
    let tiny = store.prune_at(budget(1), NOW).await.expect("tiny budget");
    assert!(tiny.budget_exceeded, "live state cannot fit in one byte");
    assert_eq!(tiny.unit_runs, 1);
    assert_eq!(
        (
            tiny.task_runs,
            tiny.log_segments,
            tiny.run_summaries,
            tiny.remote_hosts
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(
        store.task_runs(lane.id, 10).await.expect("tasks")[0].id,
        active_task.id
    );
    assert!(
        store
            .task_succeeded(lane.id, "seed".into())
            .await
            .expect("once marker")
    );
    assert!(
        store
            .unit_runs(lane.id, 10)
            .await
            .expect("history")
            .iter()
            .all(|run| run.id != completed.id)
    );
    assert_eq!(
        store.active_unit_runs().await.expect("active")[0].id,
        active.id
    );
    assert_eq!(
        store
            .allocation(BlockOwner::Lane(lane.id))
            .await
            .expect("allocation"),
        Some(allocation)
    );
    assert_eq!(store.projects().await.expect("projects").len(), 1);
    assert_eq!(store.worktrees(p.id).await.expect("worktrees").len(), 1);
    assert_eq!(store.lanes(None).await.expect("lanes").len(), 1);
}

#[tokio::test]
/// Every old user_version schema is refused with a stable recovery code.
async fn old_schema_refusal_is_explicit_and_non_destructive() {
    let dir = tempfile::tempdir().expect("dir");
    for version in 1..=5 {
        let path = dir.path().join(format!("legacy-{version}.db"));
        let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
        c.batch_execute(&format!("PRAGMA user_version = {version}"))
            .expect("old version");
        let error = match Store::open(&path).await {
            Ok(_) => panic!("legacy schema accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::LegacySchema { version: found } if found == version));
        assert!(error.to_string().contains("STATE_LEGACY_SCHEMA"));
        assert!(error.to_string().contains("devlanes daemon reset-state"));
    }
}
