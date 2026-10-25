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
- `--ignore-sigterm` consumes SIGTERM; SIGINT still exits. On Windows, Ctrl-Break and Ctrl-C both exit, and this flag consumes both.
- `--spawn-grandchild` creates a child and grandchild; `--setsid-grandchild` creates that tree with the grandchild in its own session (Unix only; Windows refuses it).
- `--exit-after <duration>` accepts values such as `500ms` or `10s` and is inherited by descendants.

## Fake tools

`fake-tool` stands in for external programs (`mise`, `direnv`, `mkcert`) where `#!/bin/sh` scripts cannot run. `fake_tool::install(dir, "mise", &behaviour)` copies the binary to `dir/mise` (`mise.exe` on Windows) and writes the `Behaviour` (expected arguments, a file recording each call, a sleep, stdout, stderr, exit code) next to it as `mise.exe.json`. Tests never build it: build this crate first (`cargo build -p devlanes-testing`, or any workspace test build), and the helper finds the binary next to the test binaries.

```rust
use devlanes_testing::fake_tool::{Behaviour, install};

let bin = tempfile::tempdir()?;
let mise = install(
    bin.path(),
    "mise",
    &Behaviour::new()
        .args(["env", "--json"])
        .stdout_json(serde_json::json!({ "FROM_DIR": "{cwd}", "PATH": "{env:PATH}" })),
)?;
```

## Behaviour

Helpers perform blocking work; `TestRepo` requires Git on PATH and disables global Git configuration and hooks. `TestHome::configure` sets home variables on a command without changing the caller's environment. Temporary directories are removed on drop, so stop owned processes first.

Server descendants deliberately survive parent termination. Tests must clean them up and supply an exit deadline as a fallback. Clear the subprocess environment before testing `--print-env` to avoid recording credentials.

## License

Apache-2.0
