//! Boot-aware run identities, escaped descendants, and rows that fail to decode.

use super::{lane, project, schema};
use devlanes_core::{Actor, ActorKind, ExitOutcome, LaneId, ProcessIdentity};
use devlanes_state::{DesiredState, Error, LaneInput, Store, UnitRun, UnitRunInput};
use diesel::prelude::*;

/// Records an unfinished run of `unit` under `boot`.
async fn run(store: &Store, lane: LaneId, unit: &str, pid: u32, boot: Option<&str>) -> UnitRun {
    store
        .record_unit_run(UnitRunInput {
            lane_id: lane,
            unit: unit.into(),
            process: ProcessIdentity {
                pid,
                sid: pid,
                start_time: 7,
            },
            actor: Actor {
                kind: ActorKind::Human,
                agent_id: None,
                session_id: None,
            },
            log_path: None,
            boot_id: boot.map(str::to_owned),
        })
        .await
        .expect("run")
}

#[tokio::test]
/// A run recorded under another boot is ended without a probe and never listed
/// as active; same-boot and legacy rows stay for the identity check.
async fn runs_from_another_boot_are_never_returned_alive() {
    let store = Store::open(":memory:").await.expect("store");
    let p = project(&store, "acme").await;
    let lane = lane(&store, p.id, 0).await;
    let old = run(&store, lane.id, "old", 10, Some("boot-1")).await;
    let current = run(&store, lane.id, "current", 11, Some("boot-2")).await;
    let legacy = run(&store, lane.id, "legacy", 12, None).await;
    assert_eq!(
        store
            .finish_other_boot_runs("boot-2".into())
            .await
            .expect("finish"),
        1
    );
    let active: Vec<_> = store
        .active_unit_runs()
        .await
        .expect("active")
        .into_iter()
        .map(|run| run.id)
        .collect();
    assert_eq!(active, [current.id, legacy.id]);
    let ended = store
        .unit_runs(lane.id, 10)
        .await
        .expect("history")
        .into_iter()
        .find(|run| run.id == old.id)
        .expect("old run");
    assert!(ended.ended_at.is_some());
    assert_eq!(
        ended.outcome,
        Some(ExitOutcome {
            code: None,
            signal: None
        })
    );
    assert_eq!(
        store
            .finish_other_boot_runs("boot-2".into())
            .await
            .expect("again"),
        0
    );
    // The unit of the ended run can start again in this boot.
    run(&store, lane.id, "old", 13, Some("boot-2")).await;
}

#[tokio::test]
/// Escaped descendants are stored once per identity, only for unfinished runs,
/// and go with their run.
async fn escaped_members_round_trip_and_follow_their_run() {
    let store = Store::open(":memory:").await.expect("store");
    let p = project(&store, "acme").await;
    let lane = lane(&store, p.id, 0).await;
    let leader = run(&store, lane.id, "web", 100, Some("boot")).await;
    let daemon = ProcessIdentity {
        pid: 200,
        sid: 200,
        start_time: u64::MAX,
    };
    let child = ProcessIdentity {
        pid: 201,
        sid: 200,
        start_time: 9,
    };
    assert_eq!(
        store
            .record_unit_run_members(leader.id, vec![daemon, child])
            .await
            .expect("members"),
        2
    );
    // A recycled PID with a new start time is a different process.
    let recycled = ProcessIdentity {
        start_time: 10,
        ..child
    };
    assert_eq!(
        store
            .record_unit_run_members(leader.id, vec![daemon, child, recycled])
            .await
            .expect("repeat"),
        1
    );
    let mut members = store.unit_run_members(leader.id).await.expect("list");
    members.sort_by_key(|member| (member.pid, member.start_time));
    assert_eq!(members, [daemon, child, recycled]);
    assert!(matches!(
        store
            .record_unit_run_members(leader.id, vec![ProcessIdentity { pid: 0, ..child }])
            .await,
        Err(Error::Invalid(_))
    ));
    store
        .finish_unit_run(
            leader.id,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        )
        .await
        .expect("finish");
    assert!(matches!(
        store
            .record_unit_run_members(leader.id, vec![ProcessIdentity { pid: 300, ..child }])
            .await,
        Err(Error::NotFound(_))
    ));
    store.reset().await.expect("reset");
    assert!(
        store
            .unit_run_members(leader.id)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
/// One undecodable lane row is skipped by `lanes` and reported by `lane`.
async fn a_corrupt_lane_row_does_not_hide_the_others() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let p = project(&store, "acme").await;
    let good = lane(&store, p.id, 0).await;
    let bad = lane(&store, p.id, 1).await;
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("db");
    diesel::update(schema::lanes::table.find(bad.id.0))
        .set(schema::lanes::profile.eq("not-the-first-profile"))
        .execute(&mut c)
        .expect("corrupt");
    assert_eq!(store.lanes(Some(p.id)).await.expect("lanes"), [good]);
    assert!(matches!(store.lane(bad.id).await, Err(Error::Corrupt(_))));
}

#[tokio::test]
/// Re-registering a cleaned lane revives it with a new creation time; updating
/// an active lane keeps the original one.
async fn reviving_a_cleaned_lane_resets_created_at() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let p = project(&store, "acme").await;
    let original = lane(&store, p.id, 0).await;
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("db");
    diesel::update(schema::lanes::table.find(original.id.0))
        .set(schema::lanes::created_at.eq(1_i64))
        .execute(&mut c)
        .expect("old clock");
    let input = LaneInput {
        project_id: p.id,
        worktree_id: original.worktree_id,
        index: 0,
        name: original.name.clone(),
        desired_state: DesiredState::Stopped,
        profile: None,
        profiles: Vec::new(),
    };
    let updated = store.upsert_lane(input.clone()).await.expect("update");
    assert_eq!(updated.created_at, 1);
    store.clean_lane(original.id).await.expect("clean");
    let revived = store.upsert_lane(input).await.expect("revive");
    assert_eq!(revived.id, original.id);
    assert_eq!(revived.cleaned_at, None);
    assert!(revived.created_at > 1, "{revived:?}");
}
