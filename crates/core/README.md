# devlanes-core

Shared domain types, pure allocation and reference-resolution logic, and integration traits for devlanes. The daemon and clients use these contracts; integration crates implement the traits and the binary registers them.

## Usage

Allocate a port block from an explicit snapshot of occupied ranges:

```rust
use std::collections::BTreeSet;
use devlanes_core::ports::{allocate_block, block_size, AllocationError, PortPool};

fn main() -> Result<(), AllocationError> {
    let size = block_size(7, None)?;
    let block = allocate_block(
        PortPool::default(),
        size,
        &[],
        None,
        &BTreeSet::new(),
    )?;
    assert_eq!(block.start, 20_000);
    assert_eq!(block.size, 20);
    Ok(())
}
```

## API

- `ports`: block sizing, stable named offsets, reservations, and explicit offset GC.
- `refs`: lane and unit reference parsing, context-aware resolution, globs, and tags.
- `paths`: path comparison that ignores case on Windows (`same_path`, `path_starts_with`), and `name_on_disk`, the file name a project, service or task name is stored under (the name itself on macOS and Linux, a reversible `%XX` encoding on Windows).
- `env::assemble`: environment precedence with source-layer attribution.
- `env::fold_name`, `env::fold_names`: variable names as devlanes keeps them; unchanged on Unix, uppercased on Windows, where `Path` and `PATH` are one variable.
- `Runner` and `Unit`: process launch, terminal I/O, stopping, exit waiting, and tracked process identities. `Unit::set_power` throttles or freezes a unit's process tree (`Power`) for idle lanes.
- `contrib::{Contributor, ScriptRunner}`: launch customization and script command rendering.
- `gate::{StartGate, PressureSource}`: startup pacing slots and queue snapshots, and system CPU and memory pressure readings.
- `clock::{Clock, TokioClock, ManualClock}`: monotonic time for schedulers, with a manual clock for tests.

## Behaviour

Allocation functions perform no socket probing or persistence. The caller supplies current occupancy and persists the result atomically; `devlanes-state` handles that for the daemon.

## License

Apache-2.0
