# devlanes-sys

Process-table queries for macOS and Linux. `devlanes-runner-pty` uses it to track and signal unit processes, and the daemon uses it to reconcile processes left by a crashed daemon and to report listening ports when a ready check times out.

## Usage

```rust
use devlanes_sys::{Signal, SignalOutcome};

// Record the identity right after spawning.
let identity = devlanes_sys::identity(pid)?.expect("child not yet reaped");

// Later, possibly after a daemon restart: signal it only if the PID was not recycled.
let outcome = devlanes_sys::signal_verified(&identity, Signal::TERM)?;
assert_ne!(outcome, SignalOutcome::Mismatch);

let listeners = devlanes_sys::listening_tcp(&[pid])?;
```

All functions block. Call them from `spawn_blocking` in async code.

## API

- `processes`, `process`, `identity`, `start_time`: read the process table. `process` still returns unreaped zombies.
- `matches`, `signal_verified`: compare PID and start time before acting.
- `DescendantTracker`: builds up a unit's process tree from repeated snapshots and keeps processes that later call `setsid`.
- `session_members`: the leader and members of a recorded session, for reconciliation.
- `listening_tcp`: TCP listeners of a set of PIDs, for diagnostics only.
- `raise_nofile_limit`, `child_nofile_limit`: raise the daemon's open-file limit and keep the original for children.

## Platform notes

Start times are clock ticks since boot on Linux and microseconds since the epoch on macOS. Only compare them on the machine that recorded them. Identity checks ignore the session ID, because a process can move to a new session after it was recorded.

On macOS, `listening_tcp` runs `/usr/sbin/lsof`, and zombies are read through `sysctl` because libproc no longer reports them. Linux reads everything from procfs.

`unsafe` code is confined to the macOS module (libproc and sysctl calls).

## License

Apache-2.0
