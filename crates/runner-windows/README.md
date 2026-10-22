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
use devlanes_runner_windows::WindowsRunner;

let unit = WindowsRunner::default()
    .with_exit_sweep(ExitSweep::KeepDaemonized)
    .spawn_unit(spec)
    .await?;
let output = unit.output(); // the first subscriber sees output from launch
let report = unit.stop_with_report(StopPolicy::default()).await?;
```

## API

- `WindowsRunner`: spawns units; `with_stderr`, `with_exit_sweep`, `with_track_interval`, `with_output_capacity`.
- `WindowsUnit`: `identity`, `processes`, `listening_ports`, `stop_with_report`, `wait_report`, `exit_report`, plus `devlanes_core::Unit`.
- `ExitReport`: the exit code plus processes swept, escaped, or left running.
- `command::build`: resolves the program and builds the command line; fails with `CommandError`.
- `command::Command`: `application`, `command_line`, and `arguments()` for std's `raw_arg`.
- `command::CommandError`: `NotFound` (with the directories searched), `ArgumentNotSafeForBatch`, and invalid input.

## Units

A unit's leader starts inside a new job object (breakaway allowed, no limits, no kill on close, so units outlive a daemon crash). Its descendants are tracked from the job's members and from process snapshots by parent, which also catches processes that broke away. A stop terminates the job and then every tracked process outside it, each checked by PID and start time first. After a natural exit, leftovers are ended too.

`pty: true` units run in a pseudo console (ConPTY): their output is one terminal stream, and input and resizes go to the console. Closing it ends the processes still attached, so the console is closed only after the final sweep. A pseudo console dies with the process that owns it, so these units stop about five seconds after the daemon does; pipe units (`pty: false`) outlive it.

Not yet:
- A stop terminates without first sending Ctrl-C or Ctrl-Break.
- Tasks (`ExitSweep::KeepDaemonized`) keep only processes that left their job (detached with breakaway); daemons a task starts inside its job, such as `start redis-server`, are still ended when it exits.

## Program resolution

A name without a separator or drive is looked up in the `PATH` from the unit's environment (`Path` counts), directory by directory, trying the name as given when it has an extension and then each `PATHEXT` extension that `CreateProcessW` can start (`.COM`, `.EXE`, `.BAT`, `.CMD`). Other names are paths from the unit's working directory. The daemon's own directory and the system directories are not searched, unlike std.

Arguments are quoted for `CommandLineToArgvW` exactly like std does. `.bat` and `.cmd` files run through `%SystemRoot%\System32\cmd.exe` with std's batch escaping, which refuses arguments containing line breaks. `cmd.exe /d /s /c <text>`, the shell devlanes falls back to without Git for Windows, gets `<text>` in one pair of quotes with no escaping.

## License

Apache-2.0
