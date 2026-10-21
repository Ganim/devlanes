# devlanes-sys

Process-table queries for macOS and Linux. `devlanes-runner-pty` uses it to track and signal unit processes, and the daemon uses it to reconcile processes left by a crashed daemon and to report listening ports when a ready check times out.

## Usage

```rust
use devlanes_sys::{Signal, SignalOutcome};

fn stop_child(pid: u32) -> Result<(), devlanes_sys::Error> {
    // Record the identity right after spawning.
    let identity = devlanes_sys::identity(pid)?.expect("child not yet reaped");
    let listeners = devlanes_sys::listening_tcp(&[pid])?;
    println!("{} listening sockets", listeners.len());

    // Later, possibly after a daemon restart: signal it only if the PID was not recycled.
    let outcome = devlanes_sys::signal_verified(&identity, Signal::TERM)?;
    assert_ne!(outcome, SignalOutcome::Mismatch);
    Ok(())
}
```

All functions block. Call them from `spawn_blocking` in async code.

## API

- `processes`, `process`, `identity`, `start_time`: read the process table. `process` still returns unreaped zombies.
- `matches`, `signal_verified`: compare PID and start time before acting.
- `DescendantTracker`: builds up a unit's process tree from repeated snapshots and keeps processes that later call `setsid`.
- `session_members`: the leader and members of a recorded session, for reconciliation.
- `boot_id`: an opaque token for the current boot, so records from before a reboot are never matched against live PIDs.
- `listening_tcp`: TCP listeners of a set of PIDs, for diagnostics only.
- `open_file`, `holders`: find processes that still hold a PTY slave or pipe open.
- `raise_nofile_limit`, `child_nofile_limit`: raise the daemon's open-file limit and keep the original for children.
- `macos` (macOS only): safe rusage, mach time, host CPU and sysctl readings for `devlanes-metrics-macos`.

## Behaviour

Start times are clock ticks since boot on Linux and microseconds since the epoch on macOS. Only compare them on the machine that recorded them. Identity checks ignore the session ID, because a process can move to a new session after it was recorded.

On macOS, `listening_tcp` runs `/usr/sbin/lsof`, and zombies are read through `sysctl` because libproc no longer reports them. Linux reads everything from procfs.

`unsafe` code is confined to the macOS modules (libproc, mach and sysctl calls).

## License

Apache-2.0
