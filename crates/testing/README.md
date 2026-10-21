# devlanes-testing

Test helpers and a native fixture server for devlanes crate tests. The library creates temporary homes and Git repositories without depending on core; `fixture-server` supplies HTTP and process-lifecycle behavior for integration tests.

## Usage

```rust
use devlanes_testing::{Error, TestHome, TestRepo};
use std::process::Command;

fn main() -> Result<(), Error> {
    let home = TestHome::new()?;
    let repo = TestRepo::new()?;
    repo.git(&["worktree", "add", "-b", "feature", "linked"])?;

    let mut command = Command::new("devlanes");
    command.current_dir(repo.path());
    home.configure(&mut command);
    Ok(())
}
```

Run the server from the repository root:

```sh
cargo run -p devlanes-testing --bin fixture-server -- \
  --port 4999 --log-line ready --exit-after 10s
```

HTTP paths return the request path, headers and PID as JSON. `/ws` echoes text/binary frames; `/sse` emits numbered events every 100 ms. The server prints `LISTENING <address>` after binding.

- `--port <port>` overrides `PORT` (default 3000); zero selects an ephemeral port.
- `--bind-other-port <port>` replaces the configured port; `--bind-ipv6-only` binds `[::1]` only.
- `--log-line <text>` prints a startup line and is repeatable; `--print-env` prints the entire environment as an `ENV` JSON line.
- `--ignore-sigterm` consumes SIGTERM; SIGINT still exits.
- `--spawn-grandchild` creates a child and grandchild; `--setsid-grandchild` creates that tree with the grandchild in its own session.
- `--exit-after <duration>` accepts values such as `500ms` or `10s` and is inherited by descendants.

## Behaviour

Helpers perform blocking work; `TestRepo` requires Git on PATH and disables global Git configuration (through an empty file from `empty_git_config`) and hooks. `TestHome::configure` sets home variables on a command without changing the caller's environment. Temporary directories are removed on drop, so stop owned processes first.

On Unix, homes live under `/tmp` with mode `0700`. On Windows they live in the user's temp directory without mode bits, and `configure` also sets `USERPROFILE`. `fixture-server` has no SIGTERM there, so `--ignore-sigterm` has no effect, and `--setsid-grandchild` exits with an error before spawning anything.

Server descendants deliberately survive parent termination. Tests must clean them up and supply an exit deadline as a fallback. Clear the subprocess environment before testing `--print-env` to avoid recording credentials.

## License

Apache-2.0
