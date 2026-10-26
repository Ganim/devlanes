//! Ordered profile persistence and migration from single/encoded legacy values.

use super::{lane, project, schema};
use devlanes_state::{DesiredState, LaneInput, Store};
use diesel::{migration::MigrationSource, prelude::*};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

#[tokio::test]
/// Migration 006 preserves names, array order, and the legacy decoder's fallbacks.
async fn migration_converts_legacy_values_and_down_preserves_selections() {
    const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("profiles.db");
    let mut c = SqliteConnection::establish(path.to_str().expect("path")).expect("connection");
    c.applied_migrations().expect("ledger");
    let migrations = <_ as MigrationSource<diesel::sqlite::Sqlite>>::migrations(&MIGRATIONS)
        .expect("migrations");
    c.run_migrations(&migrations[..5]).expect("v5");
    let project_id: i64 = diesel::insert_into(schema::projects::table)
        .values((
            schema::projects::name.eq("acme"),
            schema::projects::identity.eq(b"acme".to_vec()),
            schema::projects::root_path.eq(b"/test/acme".to_vec()),
            schema::projects::primary_branch.eq("main"),
        ))
        .returning(schema::projects::id)
        .get_result(&mut c)
        .expect("project");
    let cases: Vec<(Option<&str>, Vec<&str>)> = vec![
        (None, vec![]),
        (Some("dev"), vec!["dev"]),
        (
            Some(r#"["api","worker","api"]"#),
            vec!["api", "worker", "api"],
        ),
        (Some("[]"), vec![]),
        (Some(r#"["qa\"blue","日本語"]"#), vec!["qa\"blue", "日本語"]),
        (Some("[broken"), vec!["[broken"]),
        (Some("123"), vec!["123"]),
        (Some(r#"["api",1]"#), vec![r#"["api",1]"#]),
        (Some(""), vec![""]),
    ];
    let mut ids = Vec::new();
    for (index, (profile, _)) in cases.iter().enumerate() {
        let worktree_id: i64 = diesel::insert_into(schema::worktrees::table)
            .values((
                schema::worktrees::project_id.eq(project_id),
                schema::worktrees::identity.eq(index.to_string()),
                schema::worktrees::path.eq(format!("/test/acme-{index}").into_bytes()),
                schema::worktrees::present.eq(1),
                schema::worktrees::first_seen.eq(1_i64),
                schema::worktrees::last_seen.eq(1_i64),
            ))
            .returning(schema::worktrees::id)
            .get_result(&mut c)
            .expect("worktree");
        let id: i64 = diesel::insert_into(schema::lanes::table)
            .values((
                schema::lanes::project_id.eq(project_id),
                schema::lanes::worktree_id.eq(worktree_id),
                schema::lanes::lane_index.eq(index as i64),
                schema::lanes::name.eq(format!("lane{index}")),
                schema::lanes::desired_state.eq("running"),
                schema::lanes::profile.eq(*profile),
                schema::lanes::created_at.eq(1_i64),
            ))
            .returning(schema::lanes::id)
            .get_result(&mut c)
            .expect("legacy lane");
        ids.push(id);
    }
    let store = Store::open(&path).await.expect("upgrade");
    assert_eq!(
        store.schema_version().await.expect("version"),
        devlanes_state::SCHEMA_VERSION
    );
    for (id, (_, expected)) in ids.iter().zip(&cases) {
        let row = store
            .lane(devlanes_core::LaneId(*id))
            .await
            .expect("query")
            .expect("lane");
        assert_eq!(&row.profiles, expected);
        assert_eq!(row.profile.as_deref(), expected.first().copied());
        assert_eq!(row.desired_state, DesiredState::Running);
        assert_eq!(row.created_at, 1);
    }
    // Keep the actor idle while checking the reversible migration on this connection.
    // Later migrations go first; 006 is the one under test.
    for _ in 6..devlanes_state::SCHEMA_VERSION {
        c.revert_last_migration(MIGRATIONS)
            .expect("down a later migration");
    }
    c.revert_last_migration(MIGRATIONS).expect("down 006");
    for (id, (_, expected)) in ids.iter().zip(&cases) {
        let legacy: Option<String> = schema::lanes::table
            .find(*id)
            .select(schema::lanes::profile)
            .first(&mut c)
            .expect("legacy profile");
        let expected = match expected.as_slice() {
            [] => None,
            [single] => Some((*single).to_owned()),
            names => Some(serde_json::to_string(names).expect("json")),
        };
        assert_eq!(legacy, expected);
    }
    c.run_pending_migrations(MIGRATIONS).expect("redo 006");
}

#[tokio::test]
/// Ordered selections survive upsert, desired-state updates, and database reopen.
async fn profiles_round_trip_and_compatibility_setter() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let project = project(&store, "acme").await;
    let original = lane(&store, project.id, 0).await;
    let selected = vec!["worker".to_owned(), "api".to_owned()];
    let input = LaneInput {
        project_id: project.id,
        worktree_id: original.worktree_id,
        index: 0,
        name: original.name.clone(),
        desired_state: DesiredState::Running,
        profile: Some("ignored-compatibility-value".into()),
        profiles: selected.clone(),
    };
    let row = store.upsert_lane(input).await.expect("select profiles");
    assert_eq!(row.id, original.id);
    assert_eq!(row.profiles, selected);
    assert_eq!(row.profile.as_deref(), Some("worker"));
    store
        .set_lane_desired_profiles(row.id, DesiredState::Stopped, selected.clone())
        .await
        .expect("stop preserves profiles");
    drop(store);
    let store = Store::open(&path).await.expect("reopen");
    let row = store.lane(row.id).await.expect("query").expect("lane");
    assert_eq!(row.profiles, selected);
    assert_eq!(row.desired_state, DesiredState::Stopped);
    assert_eq!(store.lanes(Some(project.id)).await.expect("list")[0], row);
    store
        .set_lane_desired(
            row.id,
            DesiredState::Running,
            Some(r#"["api","web"]"#.into()),
        )
        .await
        .expect("old encoded setter");
    let stored = store.lane(row.id).await.expect("query").expect("lane");
    assert_eq!(stored.profiles, ["api", "web"]);
    assert_eq!(stored.profile.as_deref(), Some("api"));
    store
        .set_lane_desired(row.id, DesiredState::Running, Some("dev".into()))
        .await
        .expect("single profile setter");
    assert_eq!(
        store
            .lane(row.id)
            .await
            .expect("query")
            .expect("lane")
            .profiles,
        ["dev"]
    );
    store
        .set_lane_desired_profiles(row.id, DesiredState::Stopped, Vec::new())
        .await
        .expect("clear");
    let cleared = store.lane(row.id).await.expect("query").expect("lane");
    assert!(cleared.profiles.is_empty());
    assert_eq!(cleared.profile, None);
    let fresh_worktree = store
        .upsert_worktree(devlanes_state::WorktreeInput {
            project_id: project.id,
            identity: "fresh".into(),
            path: "/test/fresh".into(),
            branch: None,
            present: true,
        })
        .await
        .expect("worktree");
    let fresh = store
        .upsert_lane(LaneInput {
            project_id: project.id,
            worktree_id: fresh_worktree.id,
            index: 1,
            name: "fresh".into(),
            desired_state: DesiredState::Stopped,
            profile: None,
            profiles: selected.clone(),
        })
        .await
        .expect("insert profiles");
    assert_eq!(fresh.profiles, selected);
    assert_eq!(fresh.profile.as_deref(), Some("worker"));
}

#[tokio::test]
/// Saved services survive a reopen and a re-registration; the profiles setter
/// and a clean leave them alone and empty them.
async fn services_round_trip_and_clean_empties_them() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("state.db");
    let store = Store::open(&path).await.expect("store");
    let project = project(&store, "acme").await;
    let row = lane(&store, project.id, 0).await;
    assert!(row.services.is_empty());
    let services = vec!["web".to_owned(), "api".to_owned()];
    store
        .set_lane_selection(
            row.id,
            DesiredState::Running,
            vec!["front".into()],
            services.clone(),
        )
        .await
        .expect("save selection");
    store
        .set_lane_desired_profiles(row.id, DesiredState::Running, vec!["back".into()])
        .await
        .expect("profiles only");
    drop(store);
    let store = Store::open(&path).await.expect("reopen");
    let saved = store.lane(row.id).await.expect("query").expect("lane");
    assert_eq!(saved.services, services);
    assert_eq!(saved.profiles, ["back"]);
    let registered = store
        .upsert_lane(LaneInput {
            project_id: project.id,
            worktree_id: saved.worktree_id,
            index: 0,
            name: saved.name.clone(),
            desired_state: saved.desired_state,
            profile: None,
            profiles: saved.profiles.clone(),
        })
        .await
        .expect("re-register");
    assert_eq!(registered.services, services);
    store.clean_lane(row.id).await.expect("clean");
    let cleaned = store.lane(row.id).await.expect("query").expect("lane");
    assert!(cleaned.services.is_empty());
}
