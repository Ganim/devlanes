# devlanes-transport-ssh

The SSH `Transport` for `devlanes remote connect`. It runs the system `ssh` (a ControlMaster per box on Unix, one `ssh.exe` per session on Windows) and starts `devlanes tunnel --stdio` there, so `~/.ssh/config`, agents, ProxyJump and certificates apply as usual. The binary's composition root registers it; `devlanes-transport` runs the protocol on top.

## Usage

```rust
use std::sync::Arc;
use devlanes_transport::{Session, SessionError, SessionOptions};
use devlanes_transport_ssh::{AskpassConfig, PromptRelay, SshError, SshOptions, SshTransport};

async fn connect(home: &std::path::Path, relay: Arc<dyn PromptRelay>) -> Result<Session, Box<dyn std::error::Error>> {
    let mut options = SshOptions::new(home);
    options.askpass = Some(AskpassConfig::current_exe(relay)?);
    let transport = SshTransport::new("box.example.invalid", options);
    match Session::connect(&transport, SessionOptions::new("devlanes 0.1.0")).await {
        Err(SessionError::Transport(error)) => {
            if let Some(SshError::BootstrapRequired(plan)) = error.downcast_ref::<SshError>() {
                // After the user approves plan.instructions():
                transport.bootstrap(plan).await?;
            }
            Err(error.into())
        }
        result => Ok(result?),
    }
}
```

`devlanes askpass <prompt>` calls `run_askpass(&prompt)`, prints the answer and exits 0, or exits 1 to cancel. On Windows ssh runs `devlanes.exe <prompt>` directly, and the binary answers as askpass before parsing arguments when `DEVLANES_ASKPASS_SOCKET` holds a name that `askpass::is_relay_name` accepts.

## API

- `SshTransport`: `Transport` impl plus `probe`, `bootstrap` and `close`.
- `SshOptions::new(devlanes_home)`: control files under `<home>/run/ssh`, extra `-o` options, askpass, manifest, bootstrap policy.
- `PromptRelay`: shows ssh prompts to the attached client.
- `ReleaseManifest` and `BootstrapPlan`: what to install when the box has no `devlanes`.
- `SshError::is_permanent`: separates rejected credentials or a missing binary from network failures.

## Behaviour

The crate launches `ssh -M -f -N` itself and hands the socket to `openssh`, because `openssh`'s builder forces `BatchMode=yes` and cannot set `SSH_ASKPASS`. Prompts reach the daemon through a 0700 wrapper script and a private Unix socket.

Only a session channel is used, so boxes with `AllowTcpForwarding no` work.

Bootstrap installs to `$HOME/.devlanes/bin` only when no binary exists, verifying the manifest's SHA-256 on the box. It never replaces an installed binary, so a running box daemon is never upgraded; a protocol mismatch is reported with instructions.

The control socket path must stay under 100 bytes for macOS, so pass a short `control_dir` if `DEVLANES_HOME` is deep.

## Windows

Win32-OpenSSH has no ControlMaster. Each session is one `ssh.exe` (`System32\OpenSSH` first, else the first on `PATH`) inside a kill-on-close job: the box prints the probe and a random sentinel line, then execs `devlanes tunnel --stdio`, so probe and tunnel cost one authentication. Exit status 255 counts as ssh's own failure only when the box printed nothing. Prompts travel over a named pipe `\\.\pipe\devlanes-askpass-<pid>-<random>` that only the current user can open; the askpass process opens only names of that shape and checks that the pipe is owned by its user. Nothing is written to `control_dir`.

## License

Apache-2.0
