# devlanes-runner-pty

Runs daemon-owned units in a PTY or with pipes and implements the `Runner` and `Unit` traits from `devlanes-core`. The daemon registers it in the composition root and uses `devlanes-sys` to track and signal unit processes.

## Usage

```rust
use devlanes_core::{LaunchSpec, StopPolicy, TerminalSize, Unit};
use devlanes_runner_pty::PtyRunner;

let unit = PtyRunner::default()
    .spawn_unit(LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "echo hi; sleep 30".into()],
        cwd: "/".into(),
        env: [("PATH".into(), "/usr/bin:/bin".into())].into(),
        pty: true,
        size: TerminalSize::default(),
    })
    .await?;

let mut output = unit.output();
unit.resize(TerminalSize { columns: 100, rows: 50 }).await?;
let report = unit.stop_with_report(StopPolicy::default()).await?;
println!("{:?}, escaped: {:?}", report.outcome, report.escaped);
```

The runner needs a tokio runtime with the IO, time and signal drivers enabled.

## API

- `PtyRunner`: `spawn_unit` returns the concrete `PtyUnit`, and `Runner::spawn` returns it as `Arc<dyn Unit>`. `with_stderr`, `with_track_interval` and `with_output_capacity` adjust the defaults.
- `PtyUnit`: implements `Unit`. Also provides `identity`, `descendants`, `processes`, `listening_ports`, `size`, `wait_report` and `stop_with_report`.
- `ExitReport`: exit outcome, stop details, processes swept after the leader exited, and processes that survived SIGKILL.
- `StderrMode`: separate or merged stderr for pipe units.

## Behaviour to know

Each unit leads its own session, with the PTY as its controlling terminal. Pipe units also call `setsid` and get `/dev/null` as stdin. The environment is exactly `LaunchSpec::env`, with nothing inherited from the daemon.

Leaders are reaped by PID on SIGCHLD, never with `waitpid(-1)`, so `tokio::process` helpers keep their exit statuses.

The first `output()` call sees all output since launch. Later calls only see output from the moment they subscribe.

Stop signals the session's process group and every tracked descendant, including ones that called `setsid`, then sends SIGKILL after the timeout. Processes still running after the leader exits are killed and listed in `ExitReport::swept`. Dropping a handle does not stop the unit.

Children get the open-file limit the daemon started with, even after the daemon raised its own.

`unsafe` code is confined to the pre-exec hook in `spawn.rs` and the PTY master's registration with the tokio reactor in `unit.rs`.

## License

Apache-2.0
