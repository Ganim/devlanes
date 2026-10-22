# devlanes-transport-ssh

The SSH `Transport` for `devlanes remote connect`. It runs the system `ssh` and starts `devlanes tunnel --stdio` on the box, so `~/.ssh/config`, agents, ProxyJump and certificates apply as usual. On Unix one ControlMaster per box carries every session; on Windows each session is its own `ssh.exe` process. The binary's composition root registers it; `devlanes-transport` runs the protocol on top.

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

`devlanes askpass <prompt>` calls `run_askpass(&prompt)`, prints the answer and exits 0, or exits 1 to cancel. On Windows ssh runs `devlanes.exe <prompt>` directly with `DEVLANES_ASKPASS=1` set, and the binary dispatches on that marker, plus a relay name that `askpass::relay_process_id` accepts, before parsing arguments.

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

Win32-OpenSSH has no ControlMaster, so a session runs `sh -c '<start>; <probe>; <end>; exec devlanes tunnel --stdio'` on the box in one `ssh.exe` process: the probe lines sit between two random marker lines and mux frames follow the second, so a connect authenticates once and login output cannot be mistaken for the probe. Installing and `probe` are separate ssh runs. The bare program name `ssh` resolves to `System32\OpenSSH\ssh.exe` (found through `GetSystemDirectoryW`), then `PATH`; a missing client is `SshError::ProgramMissing`. Each `ssh.exe` runs in a job object that ends its whole tree, a `ProxyCommand` included, when the session ends or the daemon dies. The one exception: when no job can be created or the job refuses `ssh.exe`, ssh starts without one, and `Transport::warnings` reports it (shown by `remote ls`) until a later run gets a job.

Prompts travel over a named pipe that only the current user can open. The askpass side opens only names of the form `\\.\pipe\devlanes-askpass-<pid>-<32 hex digits>` and refuses a pipe served by any process other than `<pid>`, so a UNC name never reaches the network and another process cannot answer in the daemon's place. ssh runs without a console window when the daemon has none. Only exit status 255 with nothing on stdout is read as ssh's own failure, and rejected credentials only from ssh's `Permission denied (` wording; ssh passes the box's stderr on, so other exits before the tunnel starts are temporary errors.

## License

Apache-2.0
