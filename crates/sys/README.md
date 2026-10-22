# devlanes-sys

Process-table queries for macOS, Linux and Windows, plus the operating-system calls the other crates may not make themselves: safe Win32 wrappers (`windows`), the `NoWindow` spawn helper, logoff and shutdown notice (`windows::session`) and the logon entry in the registry (`windows::registry`). The runners use it to track and stop unit processes, and the daemon to reconcile processes left by a crashed daemon and to report listening ports when a ready check times out.

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
- `shared_processes`: a snapshot no older than a given age, with concurrent callers sharing one scan, for runners that track many units.
- `DescendantTracker`: builds up a unit's process tree from repeated snapshots and keeps processes that later call `setsid`. `adopt` adds members known by other means, such as Windows job membership.
- `session_members`: the leader and members of a recorded session, for reconciliation. Empty once the leader is gone, since its session ID may then belong to another session.
- `session_members_known`: the same, but a recorded member still in the session also proves it is ours, because a PID in use as a session ID is never reassigned. For a supervisor that reaped the leader itself.
- `boot_id`: an opaque token for the current boot, so records from before a reboot are never matched against live PIDs.
- `listening_tcp`: TCP listeners of a set of PIDs, for diagnostics only.
- `open_file`, `holders`: find processes that still hold a PTY slave or pipe open.
- `raise_nofile_limit`, `child_nofile_limit`: raise the daemon's open-file limit and keep the original for children. Both report no limit on Windows.
- `file_identity`, `path_identity`: device and inode on Unix, volume serial and 128-bit file id on Windows, for noticing that a path now names a different file.
- `rename_retrying`, `remove_file_retrying`, `retrying`: on Windows, retry for up to about 0.6 s while another process holds the file without delete sharing (sharing or lock errors), and once after access denied (a deletion still pending); a single attempt on Unix. Blocking.
- `wsl`, `windows_drives`: whether this Linux process runs in a WSL distro (distro name, WSL version, interop, the `System32` mount), detected once from the WSL binfmt entry, `WSL_DISTRO_NAME`, `WSL_INTEROP` or `/run/WSL`, and which mounts are Windows drives. A container on Docker Desktop's WSL kernel is not WSL. Both answer `None` or empty on macOS and Windows.
- `NoWindow`: `.no_window()` on a std or tokio `Command` for every program devlanes runs for itself (git, docker, mise, curl, ...). On Windows, in a process without a console (the daemon), it sets `CREATE_NO_WINDOW` so no console window flashes; it does nothing on Unix or when there is a console to share.
- `windows` (Windows only): safe wrappers for the endpoint, daemon and runner crates. `token` (user SIDs, strict SID parsing, user-account checks, elevation), `security` (user-only security descriptors, private-directory checks and tightening, the admin-only check, directory and files created with their descriptor for programs that run as SYSTEM, opening a plain single-name file without following links), `pipe` (user-only pipe instances, single-instance ones whose clients cannot add instances, or ones with a given descriptor, clients opened with only the rights they need, checks of the account and process on the other end and of the pipe's owner), `service` (the privileges a service keeps), `replace` (`ReplaceFileW`, which keeps the replaced file's access list), `dirs` (System32 and Program Files from the API), `process` (spawn flags, waiting on a PID, the job this process runs in, Ctrl-C delivery, the stdout swap and stdio inheritance for the daemon), `proc` (the process table backend and verified terminate), `job` (job objects), `spawn` (starting a unit's leader inside its job with only its stdio inherited, or attached to a pseudo console), `conpty` (pseudo consoles), `console` (console modes and UTF-16 console I/O for attach and prompts, whether someone can type into the console or it sits behind mintty, whether a classic console window shows the console), `clipboard` (plain text on the clipboard, for the TUI's copies), `shell` (opening http and https URLs in the default browser, running a program elevated through the UAC prompt, with its PID), `window` (`WM_CLOSE` to a process's windows, listing visible windows), `session` (a hidden window that reports logoff and shutdown to a process without a console), `registry` (string values under `HKEY_CURRENT_USER`, for the logon entry, and under `HKEY_LOCAL_MACHINE` in `registry::machine`, for the hosts service), `usage` (per-process CPU time and memory, system CPU times, memory load and the low-memory signal, for `devlanes-metrics-windows`).
- `macos` (macOS only): safe rusage, mach time, host CPU and sysctl readings for `devlanes-metrics-macos`.

## Behaviour

Start times are clock ticks since boot on Linux, microseconds since the epoch on macOS and process creation times (100 ns units since 1601) on Windows. Only compare them on the machine that recorded them. Identity checks ignore the session ID, because a process can move to a new session after it was recorded.

On macOS, `listening_tcp` runs `/usr/sbin/lsof`, and zombies are read through `sysctl` because libproc no longer reports them. Linux reads everything from procfs. Windows reads the process list from a Toolhelp snapshot and listeners from the IP helper TCP tables.

`unsafe` code is confined to the macOS modules (libproc, mach and sysctl calls) and the Windows module (Win32 calls). Signalling, sessions and descriptor holders are Unix-only and not compiled on Windows.

## License

Apache-2.0
