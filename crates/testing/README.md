# devlanes-testing

Test helpers and native fixture binaries for devlanes crate tests. The library creates temporary homes and Git repositories without depending on core; `fixture-server` supplies HTTP and process-lifecycle behavior for integration tests, and `fake-tool` stands in for external programs such as `mise`, `direnv` and `fnox`.

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
- `--ignore-sigterm` consumes SIGTERM; SIGINT still exits. On Windows it consumes Ctrl-C and Ctrl-Break, which otherwise end the server cleanly.
- `--spawn-grandchild` creates a child and grandchild; `--setsid-grandchild` (Unix) creates that tree with the grandchild in its own session; `--detached-grandchild` and `--breakaway-grandchild` (Windows) start the grandchild with `DETACHED_PROCESS` or `CREATE_BREAKAWAY_FROM_JOB`.
- `--print-console-width` (Windows) prints `CONSOLE_WIDTH <columns>` for the attached console, or `CONSOLE_WIDTH none` without one.
- `--exit-after <duration>` accepts values such as `500ms` or `10s` and is inherited by descendants.

Flags for the other platform exit with an error before anything is spawned.

## Fake tools

`fake_tool::install(dir, "mise", &behaviour)` copies `fake-tool` to `dir/mise` (`mise.exe` on Windows) and writes the `Behaviour` to `mise.json` next to it. The copy can check its arguments, record them, sleep, print stdout/stderr and exit with a code; printed strings expand `{cwd}` (canonical working directory) and `{env:NAME}`.

```rust
use devlanes_testing::fake_tool::{Behaviour, install};

let fake = install(dir, "mise", &Behaviour::new()
    .args(["env", "--json"])
    .stdout_json(serde_json::json!({ "FROM_DIR": "{cwd}" })))?;
```

The binary is taken from `DEVLANES_FAKE_TOOL` or next to the running test (`target/<profile>/fake-tool`). A workspace test run builds it; a test of another crate run alone builds it on first use with `cargo build -p devlanes-testing --bin fake-tool`.

## Behaviour

Helpers perform blocking work; `TestRepo` requires Git on PATH and disables global Git configuration (through an empty file from `empty_git_config`) and hooks. `TestHome::configure` sets home variables on a command without changing the caller's environment. Temporary directories are removed on drop, so stop owned processes first.

On Unix, homes live under `/tmp` with mode `0700`. On Windows they live in the user's temp directory without mode bits, and `configure` also sets `USERPROFILE`. Ctrl-C reaches `fixture-server` only when it did not inherit the "ignore Ctrl-C" flag that `CREATE_NEW_PROCESS_GROUP` sets; Ctrl-Break always does.

Server descendants deliberately survive parent termination. Tests must clean them up and supply an exit deadline as a fallback. Clear the subprocess environment before testing `--print-env` to avoid recording credentials.

## License

Apache-2.0
