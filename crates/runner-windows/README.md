# devlanes-runner-windows

Runs daemon-owned units on Windows: each unit's leader starts in its own Job Object, with output over pipes. The daemon registers it in the composition root; Win32 calls go through `devlanes-sys`. The `command` module, which resolves a unit's program and builds its command line, is plain Rust compiled on every OS, and `devlanes with` uses it too.

## Usage

```rust
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use devlanes_runner_windows::command;

let env = BTreeMap::from([
    ("Path".to_owned(), r"C:\Program Files\nodejs".to_owned()),
    ("SystemRoot".to_owned(), r"C:\Windows".to_owned()),
]);
let args = [OsString::from("run"), OsString::from("dev")];
let built = command::build("npm".as_ref(), &args, &env, Path::new(r"C:\src\acme"))?;
// npm resolves to npm.cmd, so this is C:\Windows\System32\cmd.exe with
// cmd.exe /e:ON /v:OFF /d /c ""C:\Program Files\nodejs\npm.cmd" run dev"
println!("{} {:?}", built.application.display(), built.command_line);
```

`application` and `command_line` are what `CreateProcessW` takes. To start the same command with std, use `Command::new(&built.application)` and pass `built.arguments()` to `raw_arg`.

The runner itself is Windows-only:

```rust
use devlanes_core::{ExitSweep, LaunchSpec, StopPolicy};
use devlanes_runner_windows::{ConsoleHelper, WindowsRunner};

let unit = WindowsRunner::new(ConsoleHelper::current_exe()?)
    .with_exit_sweep(ExitSweep::KeepDaemonized)
    .spawn_unit(spec)
    .await?;
let output = unit.output(); // the first subscriber sees output from launch
let report = unit.stop_with_report(StopPolicy::default()).await?;
```

## API

- `WindowsRunner`: spawns units; `new` with the console helper, `with_stderr`, `with_exit_sweep`, `with_track_interval`, `with_output_capacity`.
- `WindowsUnit`: `identity`, `processes`, `listening_ports`, `stop_with_report`, `wait_report`, `exit_report`, plus `devlanes_core::Unit`.
- `ExitReport`: the exit code plus processes swept, escaped, or left running.
- `ConsoleHelper`, `console_ctrl_main`: the console-control helper and its entry point for the binary that hosts it.
- `command::build`: resolves the program and builds the command line; fails with `CommandError`.
- `command::Command`: `application`, `command_line`, and `arguments()` for std's `raw_arg`.
- `command::CommandError`: `NotFound` (with the directories searched), `ArgumentNotSafeForBatch`, and invalid input.

## Units

A unit's leader starts inside a new job object that refuses breakaway (see `Job::create` in `devlanes-sys`) and does not kill on close, so units outlive a daemon crash. Descendants are tracked from the job's members and from snapshots by parent; terminating ends the job, then every tracked process by PID and start time.

`pty: true` units run in a pseudo console (ConPTY), which is closed after the final sweep and ends what is still attached. It dies with the daemon, so these units stop about five seconds after it; pipe units outlive it.

## Stopping

A stop asks first: `SIGINT` sends Ctrl-C (a key in a pseudo console), other signals Ctrl-Break, through a helper without a console (`devlanes __console-ctrl`) that attaches to the unit's console; visible windows get `WM_CLOSE`. What runs at the timeout is terminated; `SIGKILL` terminates at once. `cmd.exe` shims left at "Terminate batch job (Y/N)?" in a pseudo console are ended without escalation.

After a task (`ExitSweep::KeepDaemonized`) exits on its own, processes still on its console end and the rest are daemons it started (`left_running`). The console is told by its console host, through an undocumented `NtQueryInformationProcess` class that `devlanes-sys` tests pin. If the leader exited before its console was seen, everything is kept.

## Program resolution

A name without a separator or drive is looked up in the `PATH` from the unit's environment (`Path` counts), directory by directory, trying the name as given when it has an extension and then each `PATHEXT` extension that `CreateProcessW` can start (`.COM`, `.EXE`, `.BAT`, `.CMD`). Other names are paths from the unit's working directory. The daemon's own directory and the system directories are not searched, unlike std.

Arguments are quoted for `CommandLineToArgvW` exactly like std does. `.bat` and `.cmd` files run through `%SystemRoot%\System32\cmd.exe` with std's batch escaping, which refuses arguments containing line breaks. `cmd.exe /d /s /c <text>`, the shell devlanes falls back to without Git for Windows, gets `<text>` in one pair of quotes with no escaping.

## License

Apache-2.0
