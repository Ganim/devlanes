# devlanes-runner-windows

Runs daemon-owned units on Windows and implements the `Runner` and `Unit` traits from `devlanes-core`, as `devlanes-runner-pty` does on Unix. The composition root registers it for the daemon; every Win32 call goes through `devlanes-sys-windows`.

## Usage

```rust
use devlanes_core::{LaunchSpec, StopPolicy, TerminalSize, Unit};
use devlanes_runner_windows::WindowsRunner;

let unit = WindowsRunner::default()
    .with_shell(devlanes_shell_windows::windows_shell().await)
    .spawn_unit(LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "npm run dev".into()],
        cwd: r"C:\src\acme".into(),
        env: [("PATH".into(), r"C:\Program Files\nodejs;C:\Windows\System32".into())].into(),
        pty: false,
        size: TerminalSize::default(),
    })
    .await?;

let mut output = unit.output();
let report = unit.stop_with_report(StopPolicy::default()).await?;
println!("{:?}, swept: {:?}", report.outcome, report.swept);
```

The `devlanes` binary dispatches `devlanes __console-ctrl <pid> <c|break>` to `console_ctrl_main` before anything else; graceful stops start it.

## API

- `WindowsRunner`: `spawn_unit` returns the concrete `WindowsUnit`, and `Runner::spawn` returns it as `Arc<dyn Unit>`. `with_shell`, `with_stderr`, `with_console_helper`, `with_track_interval` and `with_output_capacity` adjust the defaults.
- `WindowsUnit`: implements `Unit`. Also provides `identity`, `processes`, `listening_ports`, `size`, `set_power`, `terminate`, `exit_report`, `wait_report` and `stop_with_report`.
- `ExitReport`: exit code, stop details and the processes swept after the leader exited.
- `SpawnError`: why a unit did not start, with a stable `code()` such as `shell_missing` or `unsafe_batch_argument`. Converted to `devlanes_core::Error::Io` with a `devlanes_core::SpawnRefusal` source that carries the code.
- `spawn_foreground` and `shell_exit_code`: run a command on this process's console with the same resolution (the test command of `devlanes with`) and map its exit code the way a shell reports it.
- `console_ctrl_main` and `ConsoleHelper`: the helper that sends Ctrl-C or Ctrl-Break to a unit's console.

## Processes

Each unit starts inside its own job object, so the job's members are its process tree and no process snapshots are taken. The job refuses breakaway, ends a crashing member at once instead of waiting on an error dialog, and does not end its members when the daemon exits, so units outlive a daemon crash and the next daemon finds them as orphans.

The leader gets a hidden console of its own, `NUL` as stdin and pipes for stdout and stderr. The environment is exactly `LaunchSpec::env`: entries such as `=C:` are dropped, and of two names that differ only in case the one sorting last wins.

## Pseudo consoles

A `pty: true` leader runs in a pseudo console (ConPTY) of `LaunchSpec::size`, or `TerminalSize::RENDERED_HEADLESS` (300x40) when that is the default. Input is written to the console and resizes go to it. The inbox console host sends its rendering of the screen, not the bytes the programs wrote, so the unit reports `renders_screen()`. Its own mode switches (`ESC[?9001h`, `ESC[?1004h` and their resets) and window size reports (`ESC[8;<rows>;<columns>t`) are taken out of the output, and the repaint that follows a resize is preceded by `ESC[8;<rows>;<columns>t`, so the daemon's screen model changes size exactly there. A resize asked for while another is being repainted waits for that repaint.

When the unit ends or is stopped, its job is terminated and waited for before the console is closed, which ends the output. The console closes with the daemon too, so `pty: true` units end about 5 seconds after a daemon crash and are not found as orphans.

A bare program name is looked up on the unit's `PATH` with `PATHEXT` (`.COM`, `.EXE`, `.BAT` and `.CMD` only). Batch files run through `<System32>\cmd.exe /e:ON /v:OFF /d /c` with std's escaping, and an argument `cmd.exe` cannot take safely (a line break) is refused. A program relative to a drive's current directory (`C:tool`) and a working directory of 260 characters or more are refused too.

`/bin/sh`, which `run:` strings resolve to, runs as Git for Windows' `usr\bin\sh.exe` with `MSYS_NO_PATHCONV=1` and `MSYS2_ARG_CONV_EXCL=*`; without it such units fail with `shell_missing`. Git's `usr\bin` is appended to the end of the unit's `PATH`, so scripts find `sleep`, `grep` and `sed` while programs found earlier on `PATH` still win.

## Stopping

| `stop.signal` | Windows action |
|---|---|
| `SIGINT` | Ctrl-C (in a pseudo console, the `0x03` key), or Ctrl-Break for `/bin/sh` units, whose programs ignore Ctrl-C |
| `SIGTERM`, `SIGHUP`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2` | Ctrl-Break |
| `SIGKILL` | terminate the job |

A graceful stop also posts `WM_CLOSE` to the members' visible windows, then sends one console event through the helper, which the supervisor does not wait for. In a pseudo console a `.cmd` shim asks "Terminate batch job (Y/N)?" after the event; once only `cmd.exe` is left, the runner answers `Y` once, which is not an escalation. Once only the console host is left, or at the timeout, the job is terminated; the report says whether the timeout was reached. A process ended by a console event exits with `STATUS_CONTROL_C_EXIT` (`0xC000013A`), which is reported as its code.

## Idle units

`set_power(Power::Frozen)` suspends every job member except the console host with `NtSuspendProcess`, reading the job again until no new member appears, and keeps each process handle open until the thaw, so every process is resumed once and a reused PID is never touched. `Power::Throttled` is Task Manager's efficiency mode: `IDLE_PRIORITY_CLASS` and EcoQoS on every member, also on members the supervisor's refresh finds later. `Power::Normal` resumes and restores each member's recorded priority class and power throttling (`NORMAL_PRIORITY_CLASS` and Windows' default for members that started while throttled). A process that cannot be resumed or restored keeps its handle, and the next `Normal` tries again. A stop puts a frozen unit back to `Normal` before sending Ctrl-C, Ctrl-Break or `WM_CLOSE`, which suspended threads would never handle. Working sets are not trimmed.

## Limitations

When the leader exits on its own, everything left in its job is ended, for tasks too: a task cannot start a daemon that outlives it, and `ExitReport::left_running` is always empty. Tools that insist on breaking away from the job fail to start their children.

## License

Apache-2.0
