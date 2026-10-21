# devlanes-state

The daemon's Diesel store for project and lane records, port allocations, and run history, using bundled SQLite. Its async API sends work to a dedicated database thread and uses `devlanes-core` for allocation rules.

## Usage

Open the store from daemon code; the database's parent directory must already exist:

```rust
use std::path::Path;
use devlanes_state::{Error, PruneReport, RetentionPolicy, Store};

async fn maintain(db: &Path) -> Result<PruneReport, Error> {
    let store = Store::open(db).await?;
    let report = store.prune(RetentionPolicy::default()).await?;
    store.close().await?;
    Ok(report)
}
```

## API

- `Store`: cloneable async handle for registry, lifecycle, history, and remote-host queries. `Store::close` drains queued work and joins the database thread.
- `Error`: typed failures. `Conflict`, `ActiveRuns` and `Corrupt` are the ones a caller can act on; Diesel errors stay available as the `source`.
- `AllocationRequest` and `Store::allocate`: transactional block allocation with stable project-wide offsets.
- `UnitRunInput` and `Store::active_unit_runs`: persisted process identities and boot IDs for reconciliation. `Store::finish_other_boot_runs` ends runs from an earlier boot without probing their PIDs.
- `Store::record_unit_run_members`: descendants that left a unit's session, so a later daemon can stop them with the run.
- `Lane::profiles` and `Store::set_lane_desired_profiles`: ordered profile selections; `Lane::profile` contains the first entry for compatibility.
- `RetentionPolicy` and `PruneReport`: age limits, a database size budget, and measured cleanup results.
- `Store::prune_at`: retention with an explicit Unix-millisecond clock.

## Config example

```yaml
# ~/.devlanes/config.yaml
state:
  # Budget for state.db plus its WAL and shared-memory files.
  max_size: 512MiB
history:
  # Finished unit and task runs older than this are pruned.
  retention: 30d
  # Remote hosts that have not connected for this long are forgotten.
  remote_hosts_retention: 90d
```

The daemon builds its `RetentionPolicy` from these settings, `metrics.retention` and `logs.retention.days`.

## Behaviour

Only the daemon opens the database. Client environments and secret-provider values stay in memory; log bodies are stored separately from their database metadata.

Opening runs pending embedded Diesel migrations and configures WAL, foreign keys, and a five-second busy timeout. Old rusqlite development databases using `user_version` are refused with `STATE_LEGACY_SCHEMA`: run `devlanes daemon reset-state`. Newer Diesel schemas require an upgrade.

Call retention periodically from the daemon with configuration-derived limits. Defaults are 30 days for finished runs and metrics, 7 days for log segment rows, 90 days since a remote host's last connection, and a 512 MiB database budget. These defaults are exported as `DEFAULT_*` constants. Log files have separate retention; this store removes their metadata only.

Startup peaks (`record_startup_peak`, `startup_peaks`) keep the newest ten per metrics series and expire with metrics retention; the daemon's start gate uses them as each unit's memory need.

Under size pressure, pruning removes oldest metric rollups before other history. Registry rows, unfinished runs, and successful-once task markers remain. Closed segment rows can expire while their process keeps running. Size-budget deletion runs in batches of at most `PRUNE_BATCH_ROWS` rows, each its own transaction and its own job on the database thread, so other calls run in between. A pass stops after `PRUNE_MAX_BATCHES` and sets `PruneReport::more` when the budget is still exceeded. Incremental vacuum returns freed pages to disk; `PruneReport` includes sidecar bytes and reports an unmet budget or a checkpoint blocked by readers.

`reset`, `clean_lane`, `delete_lane` and `delete_project` refuse with `Error::ActiveRuns` while runs in their scope are unfinished, because those rows are the only record of processes that may still be running. `clean_lane` forgets setup records, once-only task markers and task history in the same transaction. Re-registering a cleaned lane resets its `created_at`.

A panic inside a store operation is caught on the database thread: the caller gets `Error::Panicked`, an open transaction is rolled back, and the store keeps serving. Cancelling an async call does not undo work already queued on the database thread.

## Schema maintenance

`src/schema.rs` is checked in and private to this crate. It was written against the migrations; with Diesel CLI available, regenerate it from `crates/state` using a scratch database:

```sh
DATABASE_URL=../../target/state-schema.db diesel migration run
DATABASE_URL=../../target/state-schema.db diesel print-schema
```

Because of `diesel.toml`, `diesel migration run` (and `redo`) rewrites `src/schema.rs` in place, dropping its hand-written comments and table order. To only compare, run `print-schema` alone and diff its output. Keep migration changes and the schema file together; the `schema_rs_matches_the_migrations` test fails when they drift.

## License

Apache-2.0
