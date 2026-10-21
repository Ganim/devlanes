//! Real SQLite round-trips and allocation transaction regressions.

mod lifecycle;

use devlanes_core::{
    Actor, ActorKind, ExitOutcome, ProcessIdentity, ProjectId,
    ports::{PortName, PortPool},
};
use devlanes_state::{
    AllocationRequest, BlockOwner, DesiredState, Error, Lane, LaneInput, Project, ProjectInput,
    SCHEMA_VERSION, Store, UnitRunInput, WorktreeInput,
};
use std::{collections::BTreeSet, path::PathBuf};

/// Registers a project with deterministic paths inside the test's logical namespace.
async fn project(store: &Store, name: &str) -> Project {
    store
        .upsert_project(ProjectInput {
            name: name.into(),
            common_dir: Some(format!("/test/{name}/.git").into()),
            root_path: format!("/test/{name}").into(),
            primary_branch: "main".into(),
        })
        .await
        .expect("project")
}

/// Registers a worktree and lane without invoking Git or starting processes.
async fn lane(store: &Store, project: ProjectId, index: u32) -> Lane {
    let worktree = store
        .upsert_worktree(WorktreeInput {
            project_id: project,
            identity: format!("worktree-{index}"),
            path: format!("/test/lane-{index}").into(),
            branch: Some(format!("branch-{index}")),
            present: true,
        })
        .await
        .expect("worktree");
    store
        .upsert_lane(LaneInput {
            project_id: project,
            worktree_id: worktree.id,
            index,
            name: format!("lane{index}"),
            desired_state: DesiredState::Stopped,
            profile: None,
        })
        .await
        .expect("lane")
}

/// Requests primary ports while retaining default automatic sizing.
fn request(owner: BlockOwner, count: usize) -> AllocationRequest {
    AllocationRequest {
        owner,
        pool: PortPool::default(),
        fixed_size: None,
        fixed_start: None,
        names: (0..count)
            .map(|n| PortName::new(format!("service-{n:03}"), "default"))
            .collect(),
        reserved: BTreeSet::new(),
    }
}

#[tokio::test]
/// Moving a worktree preserves lane/port identities and resume intent after reopen.
async fn registry_moves_resume_and_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
    let project = project(&store, "demo").await;
    let lane = lane(&store, project.id, 2).await;
    store
        .set_lane_desired(lane.id, DesiredState::Running, Some("api".into()))
        .await
        .expect("desired");
    let moved = store
        .upsert_worktree(WorktreeInput {
            project_id: project.id,
            identity: "worktree-2".into(),
            path: "/test/moved".into(),
            branch: Some("feature/moved".into()),
            present: true,
        })
        .await
        .expect("move");
    assert_eq!(moved.id, lane.worktree_id);
    let allocation = store
        .allocate(request(BlockOwner::Lane(lane.id), 3))
        .await
        .expect("allocation");
    store.mark_project_started(project.id).await.expect("start");
    drop(store);
    let store = Store::open(path).await.expect("reopen");
    let restored = store.lane(lane.id).await.expect("query").expect("lane");
    assert_eq!(restored.index, 2);
    assert_eq!(restored.profile.as_deref(), Some("api"));
    assert_eq!(restored.desired_state, DesiredState::Running);
    assert_eq!(
        store
            .allocation(BlockOwner::Lane(lane.id))
            .await
            .expect("query")
            .expect("allocation"),
        allocation
    );
    assert_eq!(
        store
            .worktree(moved.id)
            .await
            .expect("query")
            .expect("worktree")
            .path,
        PathBuf::from("/test/moved")
    );
    assert!(
        store
            .project(project.id)
            .await
            .expect("query")
            .expect("project")
            .last_start
            .is_some()
    );
    store
        .mark_worktree_missing(moved.id)
        .await
        .expect("missing");
    assert!(!store.worktrees(project.id).await.expect("worktrees")[0].present);
    store.clean_lane(lane.id).await.expect("clean");
    assert!(store.port_blocks().await.expect("blocks").is_empty());
    assert!(
        store
            .lane(lane.id)
            .await
            .expect("query")
            .expect("lane")
            .cleaned_at
            .is_some()
    );
    store.delete_project(project.id).await.expect("delete");
    assert!(store.projects().await.expect("projects").is_empty());
}

#[tokio::test]
/// Existing rows survive upgrades; future schemas are refused before any migration.
async fn migrations_upgrade_v1_and_refuse_future_without_mutation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v1.db");
    {
        let connection = rusqlite::Connection::open(&path).expect("db");
        connection
            .execute_batch(include_str!("../../migrations/001_registry.sql"))
            .expect("v1");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("version");
        connection.execute("INSERT INTO projects(name, identity, root_path, primary_branch) VALUES ('old', x'01', x'2F74657374', 'main')", []).expect("existing data");
    }
    let store = Store::open(&path).await.expect("upgrade");
    assert_eq!(store.projects().await.expect("projects")[0].name, "old");
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
    drop(store);
    let newer = dir.path().join("future.db");
    {
        let connection = rusqlite::Connection::open(&newer).expect("db");
        connection
            .pragma_update(None, "user_version", 999)
            .expect("version");
    }
    assert!(matches!(
        Store::open(&newer).await,
        Err(Error::NewerSchema { found: 999, .. })
    ));
    let connection = rusqlite::Connection::open(&newer).expect("db");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("version"),
        999
    );
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )
        .expect("tables");
    assert_eq!(count, 0);
}

#[tokio::test]
/// Requests across projects and independent actors share one non-overlapping pool.
async fn concurrent_allocations_are_machine_wide_and_serialized() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let other_handle = Store::open(&path).await.expect("second actor");
    let project_a = project(&store, "a").await;
    let project_b = project(&store, "b").await;
    let mut jobs = tokio::task::JoinSet::new();
    for index in 0..24 {
        let lane = lane(
            &store,
            if index % 2 == 0 {
                project_a.id
            } else {
                project_b.id
            },
            index,
        )
        .await;
        let store = if index % 2 == 0 {
            store.clone()
        } else {
            other_handle.clone()
        };
        jobs.spawn(async move { store.allocate(request(BlockOwner::Lane(lane.id), 4)).await });
    }
    let mut allocations = Vec::new();
    while let Some(result) = jobs.join_next().await {
        let allocation = result.expect("task").expect("allocate");
        assert!(
            allocations
                .iter()
                .all(|block| !allocation.block.overlaps(*block))
        );
        allocations.push(allocation.block);
    }
    assert_eq!(allocations.len(), 24);
    let shared = store
        .allocate(request(BlockOwner::Project(project_a.id), 2))
        .await
        .expect("shared");
    assert!(
        allocations
            .iter()
            .all(|block| !shared.block.overlaps(*block))
    );
    assert_eq!(store.port_blocks().await.expect("blocks").len(), 25);
}

#[tokio::test]
/// Resizing keeps offsets; failed changes roll back and GC preserves retained names.
async fn stable_offsets_resize_reservations_gc_and_failed_transaction() {
    let store = Store::open(":memory:").await.expect("store");
    let project = project(&store, "demo").await;
    let a = lane(&store, project.id, 0).await;
    let b = lane(&store, project.id, 1).await;
    let mut initial = request(BlockOwner::Lane(a.id), 2);
    initial.reserved.insert(0);
    let first = store.allocate(initial.clone()).await.expect("first");
    assert!(first.offsets.values().all(|offset| *offset > 0));
    let second = store
        .allocate(request(BlockOwner::Lane(b.id), 2))
        .await
        .expect("second");
    assert_eq!(first.offsets, second.offsets);
    let mut larger = request(BlockOwner::Lane(a.id), 20);
    larger.reserved.insert(0);
    let resized = store.allocate(larger).await.expect("resize");
    assert_eq!(resized.previous, Some(first.block));
    assert!(!resized.block.overlaps(first.block));
    assert!(!resized.block.overlaps(second.block));
    for (name, offset) in &first.offsets {
        assert_eq!(resized.offsets[name], *offset);
    }
    let b_resized = store
        .allocate(request(BlockOwner::Lane(b.id), 20))
        .await
        .expect("resize second");
    assert_eq!(resized.offsets, b_resized.offsets);
    let mut conflict = request(BlockOwner::Lane(a.id), 20);
    conflict
        .reserved
        .insert(*first.offsets.values().next().expect("offset"));
    assert!(store.allocate(conflict).await.is_err());
    assert_eq!(
        store
            .allocation(BlockOwner::Lane(a.id))
            .await
            .expect("query")
            .expect("allocation")
            .block,
        resized.block
    );
    let mut fixed = request(BlockOwner::Lane(a.id), 20);
    fixed.fixed_size = Some(10);
    assert!(store.allocate(fixed).await.is_err());
    let retained = store
        .allocate(initial.clone())
        .await
        .expect("removed names retained");
    assert_eq!(retained.offsets.len(), 20);
    assert_eq!(
        store
            .gc_offsets(project.id, initial.names.clone(), BTreeSet::new())
            .await
            .expect("gc"),
        19
    );
    let after = store
        .allocation(BlockOwner::Lane(a.id))
        .await
        .expect("query")
        .expect("allocation");
    assert_eq!(after.offsets, first.offsets);
    assert!(after.reserved.is_empty());
}

#[tokio::test]
/// Failed fixed or dynamic requests leave both ownership and occupancy unchanged.
async fn explicit_conflicts_and_pool_exhaustion_do_not_leak_records() {
    let store = Store::open(":memory:").await.expect("store");
    let project = project(&store, "demo").await;
    let a = lane(&store, project.id, 0).await;
    let b = lane(&store, project.id, 1).await;
    let mut first = request(BlockOwner::Lane(a.id), 1);
    first.pool = PortPool {
        start: 30_000,
        end: 30_009,
    };
    store.allocate(first.clone()).await.expect("first");
    first.owner = BlockOwner::Lane(b.id);
    assert!(matches!(
        store.allocate(first.clone()).await,
        Err(Error::Allocation(_))
    ));
    first.fixed_start = Some(30_005);
    assert!(matches!(
        store.allocate(first).await,
        Err(Error::Allocation(_))
    ));
    assert_eq!(store.port_blocks().await.expect("blocks").len(), 1);
    assert!(
        store
            .allocation(BlockOwner::Lane(b.id))
            .await
            .expect("query")
            .is_none()
    );
}

#[tokio::test]
/// A restricted hole in the first fit must not mask an otherwise usable pool.
async fn allocation_searches_past_restricted_port_holes() {
    let store = Store::open(":memory:").await.expect("store");
    let project = project(&store, "demo").await;
    let lane = lane(&store, project.id, 0).await;
    let mut request = request(BlockOwner::Lane(lane.id), 10);
    request.pool = PortPool {
        start: 5999,
        end: 6010,
    };
    request.fixed_size = Some(10);
    let allocation = store
        .allocate(request)
        .await
        .expect("block after restricted 6000");
    assert_eq!(allocation.block.start, 6001);
    assert_eq!(allocation.offsets.len(), 10);
}

#[tokio::test]
/// Run history preserves unsigned identity tokens and the first terminal outcome.
async fn unit_identity_actor_and_exit_roundtrip_without_environment_columns() {
    let store = Store::open(":memory:").await.expect("store");
    let project = project(&store, "demo").await;
    let lane = lane(&store, project.id, 0).await;
    let process = ProcessIdentity {
        pid: 42,
        sid: 42,
        start_time: u64::MAX,
    };
    let actor = Actor {
        kind: ActorKind::Agent,
        agent_id: Some("test-agent".into()),
        session_id: Some("session".into()),
    };
    let run = store
        .record_unit_run(UnitRunInput {
            lane_id: lane.id,
            unit: "api".into(),
            process,
            actor: actor.clone(),
            log_path: Some("/test/api.log".into()),
        })
        .await
        .expect("run");
    assert_eq!(run.process, process);
    assert_eq!(run.actor, actor);
    assert_eq!(
        store.active_unit_runs().await.expect("active").as_slice(),
        std::slice::from_ref(&run)
    );
    assert!(store.clean_lane(lane.id).await.is_err());
    let outcome = ExitOutcome {
        code: None,
        signal: Some(15),
    };
    store
        .finish_unit_run(run.id, outcome)
        .await
        .expect("finish");
    store
        .finish_unit_run(
            run.id,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        )
        .await
        .expect("idempotent finish");
    assert!(store.active_unit_runs().await.expect("active").is_empty());
    let history = store.unit_runs(lane.id, 10).await.expect("history");
    assert_eq!(history[0].outcome, Some(outcome));
    assert!(history[0].ended_at.is_some());
    store.clean_lane(lane.id).await.expect("clean");
    assert_eq!(
        store.unit_runs(lane.id, 10).await.expect("history").len(),
        1
    );
}

#[tokio::test]
/// Store constraints reject cross-project ownership and retain non-UTF-8 paths.
async fn relational_constraints_and_lossless_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let a = project(&store, "a").await;
    let b = project(&store, "b").await;
    let lane = lane(&store, a.id, 0).await;
    assert!(
        store
            .upsert_lane(LaneInput {
                project_id: b.id,
                worktree_id: lane.worktree_id,
                index: 1,
                name: "wrong".into(),
                desired_state: DesiredState::Stopped,
                profile: None
            })
            .await
            .is_err()
    );
    let non_utf8 = PathBuf::from(OsString::from_vec(b"/test/non-utf8-\xff".to_vec()));
    let worktree = store
        .upsert_worktree(WorktreeInput {
            project_id: a.id,
            identity: "non-utf8".into(),
            path: non_utf8.clone(),
            branch: None,
            present: true,
        })
        .await
        .expect("worktree");
    assert_eq!(worktree.path, non_utf8);
    let connection = rusqlite::Connection::open(path).expect("database");
    assert_eq!(
        connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .expect("journal mode"),
        "wal"
    );
    let columns: Vec<String> = connection
        .prepare("SELECT name FROM pragma_table_info('unit_runs')")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("columns");
    assert!(
        columns
            .iter()
            .all(|name| !name.contains("env") && !name.contains("secret"))
    );
    assert!(
        connection
            .execute(
                "INSERT INTO port_blocks(project_id, lane_id, start, size) VALUES (?1,?2,20000,10)",
                rusqlite::params![a.id.0, lane.id.0]
            )
            .is_ok()
    );
    assert!(
        connection
            .execute(
                "INSERT INTO port_blocks(project_id, start, size) VALUES (?1,20005,10)",
                [b.id.0]
            )
            .is_err()
    );
}
