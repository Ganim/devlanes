# devlanes-transport-ssh

The SSH `Transport` for `devlanes remote connect`. It runs the system `ssh` (a ControlMaster per box on Unix, one `ssh.exe` per session on Windows) and starts `devlanes tunnel --stdio` there, so `~/.ssh/config`, agents, ProxyJump and certificates apply as usual. The binary's composition root registers it; `devlanes-transport` runs the protocol on top.

## Usage

```rust
use std::sync::Arc;
use devlanes_transport::{Session, SessionOptions};
use devlanes_transport_ssh::{AskpassConfig, PromptRelay, SshOptions, SshTransport};

async fn connect(home: &std::path::Path, relay: Arc<dyn PromptRelay>) -> Result<Session, Box<dyn std::error::Error>> {
    let mut options = SshOptions::new(home);
    options.askpass = Some(AskpassConfig::current_exe(relay)?);
    let transport = SshTransport::new("box.example.invalid", options);
    Ok(Session::connect(&transport, SessionOptions::new("devlanes 0.1.0")).await?)
}
```

`devlanes askpass <prompt>` calls `run_askpass(&prompt)`, prints the answer and exits 0, or exits 1 to cancel. On Windows ssh runs `devlanes.exe <prompt>` directly, and the binary answers as askpass before parsing arguments when `DEVLANES_ASKPASS_SOCKET` holds a name that `askpass::is_relay_name` accepts.

## API

- `SshTransport`: `Transport` impl plus `probe` and `close`.
- `SshOptions::new(devlanes_home)`: control files under `<home>/run/ssh`, extra `-o` options, askpass.
- `PromptRelay`: shows ssh prompts to the attached client.
- `SshError::is_permanent`: separates rejected credentials or a missing binary from network failures and a stopped box daemon.

## Behaviour

The crate launches `ssh -M -f -N` itself and hands the socket to `openssh`, because `openssh`'s builder forces `BatchMode=yes` and cannot set `SSH_ASKPASS`. Prompts reach the daemon through a 0700 wrapper script and a private Unix socket.

Only a session channel is used, so boxes with `AllowTcpForwarding no` work.

The probe looks for `devlanes` on the box's `PATH`, then in `$HOME/.devlanes/bin`, and tags its lines, so whatever the box's login shell prints around them is ignored. Before handing the tunnel to the protocol, the transport skips up to 64 KiB of such output until the tunnel's preamble line (`devlanes_transport::read_preamble`); output without one fails as `SshError::Preamble` with that output in the message. A preamble of another protocol revision is permanent; silence or more than 64 KiB of output is permanent until a tunnel has opened once. A box without devlanes fails with `SshError::NotInstalled`, which is permanent until a tunnel has opened once and temporary afterwards (a binary briefly missing during an upgrade); installing devlanes there is up to the user. A tunnel that exits with `devlanes_transport::EXIT_DAEMON_STOPPED` (the box daemon is stopped and `remote.start_daemon` is off there) becomes the temporary `SshError::BoxDaemonStopped`, so the link keeps retrying.

The control socket path must stay under 100 bytes for macOS, so pass a short `control_dir` if `DEVLANES_HOME` is deep.

## Windows

Win32-OpenSSH has no ControlMaster. Each session is one `ssh.exe` (`System32\OpenSSH` first, else the first on `PATH`) inside a kill-on-close job: the box prints the probe and a random sentinel line, then execs `devlanes tunnel --stdio`, whose preamble follows, so probe and tunnel cost one authentication. Exit status 255 counts as ssh's own failure only when the box printed nothing. Prompts travel over a named pipe `\\.\pipe\devlanes-askpass-<pid>-<random>` that only the current user can open; the askpass process opens only names of that shape and checks that the pipe is owned by its user. Nothing is written to `control_dir`.

## License

Apache-2.0
