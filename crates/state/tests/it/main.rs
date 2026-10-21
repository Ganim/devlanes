//! Real SQLite round-trips and allocation transaction regressions.

mod catalog;
mod identity;
mod lifecycle;
mod profiles;
mod retention;
#[path = "../../src/schema.rs"]
mod schema;

use diesel::{connection::SimpleConnection, prelude::*};
use diesel_migrations::{MigrationHarness, embed_migrations};

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
            profiles: Vec::new(),
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
        use diesel::migration::MigrationSource;
        let mut connection = SqliteConnection::establish(path.to_str().expect("path")).expect("db");
        const MIGRATIONS: diesel_migrations::EmbeddedMigrations = embed_migrations!("migrations");
        let migrations = <_ as MigrationSource<diesel::sqlite::Sqlite>>::migrations(&MIGRATIONS)
            .expect("migrations");
        connection.applied_migrations().expect("initialize ledger");
        connection.run_migrations(&migrations[..1]).expect("v1");
        use schema::projects as p;
        diesel::insert_into(p::table)
            .values((
                p::name.eq("old"),
                p::identity.eq(vec![1u8]),
                p::root_path.eq(b"/test".to_vec()),
                p::primary_branch.eq("main"),
            ))
            .execute(&mut connection)
            .expect("existing data");
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
        let mut connection =
            SqliteConnection::establish(newer.to_str().expect("path")).expect("db");
        connection
            .batch_execute("PRAGMA user_version = 999")
            .expect("legacy version");
    }
    assert!(matches!(
        Store::open(&newer).await,
        Err(Error::LegacySchema { version: 999 })
    ));
    let mut connection = SqliteConnection::establish(newer.to_str().expect("path")).expect("db");
    #[derive(diesel::QueryableByName)]
    struct Version {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        user_version: i32,
    }
    assert_eq!(
        diesel::sql_query("PRAGMA user_version")
            .get_result::<Version>(&mut connection)
            .expect("version")
            .user_version,
        999
    );
    assert!(
        schema::projects::table
            .count()
            .get_result::<i64>(&mut connection)
            .is_err(),
        "refusal must not create application tables"
    );
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
            boot_id: Some("boot-a".into()),
        })
        .await
        .expect("run");
    assert_eq!(run.process, process);
    assert_eq!(run.actor, actor);
    assert_eq!(run.boot_id.as_deref(), Some("boot-a"));
    assert_eq!(
        store.active_unit_runs().await.expect("active").as_slice(),
        std::slice::from_ref(&run)
    );
    let second = store
        .record_unit_run(UnitRunInput {
            lane_id: lane.id,
            unit: "api".into(),
            process: ProcessIdentity { pid: 43, ..process },
            actor: actor.clone(),
            log_path: None,
            boot_id: Some("boot-a".into()),
        })
        .await;
    assert!(
        matches!(&second, Err(Error::Conflict { constraint }) if constraint.contains("unit_runs")),
        "{second:?}"
    );
    assert!(matches!(
        store.clean_lane(lane.id).await,
        Err(Error::ActiveRuns { units: 1, tasks: 0 })
    ));
    assert!(matches!(
        store.delete_project(project.id).await,
        Err(Error::ActiveRuns { units: 1, tasks: 0 })
    ));
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
                profile: None,
                profiles: Vec::new(),
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
    let mut connection =
        SqliteConnection::establish(path.to_str().expect("path")).expect("database");
    #[derive(diesel::QueryableByName)]
    struct Journal {
        #[diesel(sql_type = diesel::sql_types::Text)]
        journal_mode: String,
    }
    assert_eq!(
        diesel::sql_query("PRAGMA journal_mode")
            .get_result::<Journal>(&mut connection)
            .expect("journal")
            .journal_mode,
        "wal"
    );
    use schema::port_blocks as p;
    assert!(
        diesel::insert_into(p::table)
            .values((
                p::project_id.eq(a.id.0),
                p::lane_id.eq(lane.id.0),
                p::start.eq(20000),
                p::size.eq(10)
            ))
            .execute(&mut connection)
            .is_ok()
    );
    assert!(
        diesel::insert_into(p::table)
            .values((p::project_id.eq(b.id.0), p::start.eq(20005), p::size.eq(10)))
            .execute(&mut connection)
            .is_err()
    );
}

#[tokio::test]
/// Registry updates retain identity and explicit deletion releases ownership.
async fn registry_updates_deletes_and_allocation_release() {
    let store = Store::open(":memory:").await.expect("store");
    let p = project(&store, "acme").await;
    let updated = store
        .upsert_project(ProjectInput {
            name: "renamed".into(),
            common_dir: p.common_dir.clone(),
            root_path: "/test/moved".into(),
            primary_branch: "trunk".into(),
        })
        .await
        .expect("update");
    assert_eq!(updated.id, p.id);
    assert_eq!(updated.name, "renamed");
    let first = lane(&store, p.id, 0).await;
    let moved = store
        .upsert_lane(LaneInput {
            project_id: p.id,
            worktree_id: first.worktree_id,
            index: 3,
            name: "feature".into(),
            desired_state: DesiredState::Running,
            profile: Some("api".into()),
            profiles: Vec::new(),
        })
        .await
        .expect("update lane");
    assert_eq!(moved.id, first.id);
    assert_eq!(moved.created_at, first.created_at);
    assert_eq!(store.lanes(Some(p.id)).await.expect("lanes"), [moved]);
    store
        .set_lane_desired(first.id, DesiredState::Stopped, None)
        .await
        .expect("clear profile");
    assert_eq!(
        store
            .lane(first.id)
            .await
            .expect("lane")
            .expect("exists")
            .profile,
        None
    );
    for owner in [BlockOwner::Lane(first.id), BlockOwner::Project(p.id)] {
        store.allocate(request(owner, 1)).await.expect("allocate");
        store.release_block(owner).await.expect("release");
        store
            .release_block(owner)
            .await
            .expect("idempotent release");
        assert!(store.allocation(owner).await.expect("query").is_none());
    }
    store.delete_lane(first.id).await.expect("delete");
    assert!(store.lane(first.id).await.expect("query").is_none());
    assert!(matches!(
        store.delete_lane(first.id).await,
        Err(Error::NotFound("lane"))
    ));
    assert!(
        store
            .upsert_worktree(WorktreeInput {
                project_id: ProjectId(-1),
                identity: "foreign".into(),
                path: "/test".into(),
                branch: None,
                present: true
            })
            .await
            .is_err()
    );
    assert!(
        store
            .finish_unit_run(
                devlanes_core::UnitRunId(-1),
                ExitOutcome {
                    code: Some(0),
                    signal: None
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
/// Diesel upgrades each supported prefix and reverses migrations without losing constraints.
async fn every_diesel_migration_prefix_upgrades_and_down_scripts_work() {
    use diesel::migration::MigrationSource;
    const MIGRATIONS: diesel_migrations::EmbeddedMigrations = embed_migrations!("migrations");
    let dir = tempfile::tempdir().expect("dir");
    for prefix in 0..=SCHEMA_VERSION as usize {
        let path = dir.path().join(format!("prefix-{prefix}.db"));
        let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("db");
        c.batch_execute("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000")
            .expect("connection settings");
        c.applied_migrations().expect("ledger");
        let migrations = <_ as MigrationSource<diesel::sqlite::Sqlite>>::migrations(&MIGRATIONS)
            .expect("migrations");
        c.run_migrations(&migrations[..prefix]).expect("prefix");
        let store = Store::open(&path).await.expect("upgrade");
        assert_eq!(
            store.schema_version().await.expect("schema"),
            SCHEMA_VERSION
        );
        project(&store, "acme").await;
        drop(store);
        c.revert_all_migrations(MIGRATIONS)
            .expect("down migrations");
        assert!(c.applied_migrations().expect("ledger").is_empty());
        c.run_pending_migrations(MIGRATIONS).expect("redo");
        assert_eq!(
            c.applied_migrations().expect("ledger").len(),
            SCHEMA_VERSION as usize
        );
    }
}

#[tokio::test]
/// A newer Diesel ledger is rejected instead of silently applying an older schema.
async fn newer_diesel_schema_is_refused() {
    diesel::table! { __diesel_schema_migrations (version) { version -> Text, run_on -> Timestamp, } }
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("future.db");
    let store = Store::open(&path).await.expect("store");
    drop(store);
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    c.batch_execute("PRAGMA busy_timeout = 5000")
        .expect("busy timeout");
    diesel::insert_into(__diesel_schema_migrations::table)
        .values(__diesel_schema_migrations::version.eq("00000000000999"))
        .execute(&mut c)
        .expect("future migration");
    assert!(matches!(
        Store::open(&path).await,
        Err(Error::NewerSchema {
            found: 999,
            supported: SCHEMA_VERSION
        })
    ));
}

#[tokio::test]
/// Initialization recovers if the first migration committed before its ledger row.
async fn initial_migration_recovers_after_interrupted_ledger_write() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("interrupted.db");
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    c.applied_migrations().expect("ledger");
    c.batch_execute(include_str!(
        "../../migrations/00000000000001_registry/up.sql"
    ))
    .expect("migration without ledger commit");
    let store = Store::open(&path).await.expect("resume migrations");
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
    assert!(store.projects().await.expect("projects").is_empty());
}

#[tokio::test]
/// Filenames cannot inject URI options; Linux additionally exercises non-UTF-8 bytes.
async fn database_paths_preserve_bytes_and_uri_punctuation() {
    use std::ffi::OsString;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().expect("dir");
    #[cfg(target_os = "linux")]
    let name = OsString::from_vec(b"state-\xff?mode=memory.db".to_vec());
    #[cfg(not(target_os = "linux"))]
    let name = OsString::from("state-é?mode=memory.db");
    let path = dir.path().join(name);
    let store = Store::open(&path).await.expect("literal database filename");
    let p = project(&store, "acme").await;
    assert!(path.exists());
    let report = store
        .prune(devlanes_state::RetentionPolicy::default())
        .await
        .expect("retention");
    assert!(!report.budget_exceeded);
    drop(store);
    let store = Store::open(&path).await.expect("reopen");
    assert_eq!(store.project(p.id).await.expect("project"), Some(p));
}
