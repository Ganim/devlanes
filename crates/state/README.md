# devlanes-state

The daemon's Diesel store for project and lane records, port allocations, and run history, using bundled SQLite. Its async API sends work to a dedicated database thread and uses `devlanes-core` for allocation rules.

## Usage

Open the store from daemon code; the database's parent directory must already exist:

```rust
use std::path::Path;
use devlanes_state::{Error, PruneReport, RetentionPolicy, Store};

async fn maintain(db: &Path) -> Result<PruneReport, Error> {
    let store = Store::open(db).await?;
    store.prune(RetentionPolicy::default()).await
}
```

## API

- `Store`: cloneable async handle for registry, lifecycle, history, and remote-host queries.
- `AllocationRequest` and `Store::allocate`: transactional block allocation with stable project-wide offsets.
- `UnitRunInput` and `Store::active_unit_runs`: persisted process identities for reconciliation.
- `RetentionPolicy` and `PruneReport`: age limits, a database size budget, and measured cleanup results.
- `Store::prune_at`: retention with an explicit Unix-millisecond clock.

## Behaviour

Only the daemon opens the database. Client environments and secret-provider values stay in memory; log bodies are stored separately from their database metadata.

Opening runs pending embedded Diesel migrations and configures WAL, foreign keys, and a five-second busy timeout. Old rusqlite development databases using `user_version` are refused with `STATE_LEGACY_SCHEMA`: run `devlanes daemon reset-state`. Newer Diesel schemas require an upgrade.

The daemon schedules retention calls and supplies configuration values. Defaults are 30 days for finished runs and metrics, 7 days for log segment rows, 90 days since a remote host's last connection, and a 512 MiB database budget. These defaults are exported as `DEFAULT_*` constants. Log files have separate retention; this store removes their metadata only.

Under size pressure, pruning removes oldest metric rollups before other history. Registry rows, unfinished runs, and successful-once task markers remain. Closed segment rows can expire while their process keeps running. Incremental vacuum returns freed pages to disk; `PruneReport` includes sidecar bytes and reports an unmet budget or a checkpoint blocked by readers.

Cancelling an async call does not undo work already queued on the database thread.

## Schema maintenance

`src/schema.rs` is checked in and private to this crate. It was written against the migrations; with Diesel CLI available, regenerate it from `crates/state` using a scratch database:

```sh
DATABASE_URL=../../target/state-schema.db diesel migration run
DATABASE_URL=../../target/state-schema.db diesel print-schema
```

`diesel.toml` makes migration commands update `src/schema.rs`; `print-schema` prints the same schema to stdout. Keep migration changes and the schema file together.

## License

Apache-2.0
