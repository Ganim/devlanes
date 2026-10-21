# devlanes-state

The daemon's SQLite store for project and lane records, port allocations, and run history. It exposes typed async queries backed by a dedicated database thread and uses `devlanes-core` for domain types and allocation rules.

## Usage

Open the store from daemon code; the database's parent directory must already exist:

```rust
use std::path::Path;
use devlanes_state::{Error, ProjectInput, Store};

async fn register_project(db: &Path, root: &Path) -> Result<Store, Error> {
    let store = Store::open(db).await?;
    let project = store.upsert_project(ProjectInput {
        name: "demo".into(),
        common_dir: None,
        root_path: root.to_path_buf(),
        primary_branch: "main".into(),
    }).await?;

    assert_eq!(store.project(project.id).await?, Some(project));
    Ok(store)
}
```

## API

- `Store`: cloneable async handle for registry, lifecycle, history, and remote-host queries.
- `AllocationRequest` and `Store::allocate`: transactional block allocation with stable project-wide offsets.
- `UnitRunInput` and `Store::active_unit_runs`: persisted process identities for reconciliation.
- `TaskRunInput` and `SetupRecord`: task history and per-lane setup completion.
- `LogSegmentInput`, `RollupRow`, and `RunSummaryRow`: log metadata and metrics history.

## Persistence

Only the daemon opens the database. Client environments and secret-provider values stay in memory; log bodies are stored separately from their database metadata.

Opening applies embedded forward-only migrations, enables WAL and foreign keys, and refuses schemas newer than the crate supports. Cancelling an async call does not undo work already queued on the database thread.

## License

Apache-2.0
