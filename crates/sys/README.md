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
- `session_members`: the leader and members of a recorded session, for reconciliation. Empty once the leader is gone, since its session ID may then belong to another session.
- `session_members_known`: the same, but a recorded member still in the session also proves it is ours, because a PID in use as a session ID is never reassigned. For a supervisor that reaped the leader itself.
- `boot_id`: an opaque token for the current boot, so records from before a reboot are never matched against live PIDs.
- `listening_tcp`: TCP listeners of a set of PIDs, for diagnostics only.
- `open_file`, `holders`: find processes that still hold a PTY slave or pipe open.
- `raise_nofile_limit`, `child_nofile_limit`: raise the daemon's open-file limit and keep the original for children. Both report no limit on Windows.
- `file_identity`, `path_identity`: device and inode on Unix, volume serial and 128-bit file id on Windows, for noticing that a path now names a different file.
- `windows` (Windows only): safe wrappers for the endpoint, daemon and runner crates. `token` (user SIDs, elevation), `security` (user-only security descriptors, private-directory checks and tightening), `pipe` (user-only pipe instances, checks of the account on the other end), `process` (spawn flags, waiting on a PID, the job this process runs in, Ctrl-C delivery, the stdout swap and stdio inheritance for the daemon).
- `macos` (macOS only): safe rusage, mach time, host CPU and sysctl readings for `devlanes-metrics-macos`.

## Behaviour

Start times are clock ticks since boot on Linux and microseconds since the epoch on macOS. Only compare them on the machine that recorded them. Identity checks ignore the session ID, because a process can move to a new session after it was recorded.

On macOS, `listening_tcp` runs `/usr/sbin/lsof`, and zombies are read through `sysctl` because libproc no longer reports them. Linux reads everything from procfs.

`unsafe` code is confined to the macOS modules (libproc, mach and sysctl calls) and the Windows module (Win32 calls). On Windows the process queries are not implemented yet and return `Error::Unsupported`; signalling, sessions and descriptor holders are Unix-only.

## License

Apache-2.0
