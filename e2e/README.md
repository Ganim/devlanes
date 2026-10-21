# devlanes-e2e

End-to-end tests that run the real `devlanes` binary as separate processes against the projects in `fixtures/`. The library in `src/` is the harness; the tests are in `tests/it/`. The SSH tests under `e2e/ssh` are separate (see its README).

The harness and tests are Unix only for now. On Windows the crate compiles to nothing and runs no tests.

## Usage

```sh
cargo nextest run --profile e2e                                           # hermetic tests
cargo nextest run --profile e2e --run-ignored only -E 'test(/^docker_/)'  # needs Docker
DEVLANES_E2E_DUMP=1 cargo nextest run --profile e2e --no-capture -E 'test(with_)'
```

The default nextest profile in `.config/nextest.toml` leaves this crate out, so `cargo nextest run --workspace` never builds the binaries from inside a test.

`DEVLANES_E2E_DUMP=1` prints the end of each test's daemon log on teardown. A test that panics (a failed `assert!`) prints it without the variable; one that returns an error does not.

## Binaries

Tests need `devlanes` and `fixture-server`. When `DEVLANES_E2E_BIN_DIR` is set, they are taken from that directory. Otherwise the first test of each test process runs `cargo build -p devlanes -p devlanes-testing` into `$CARGO_TARGET_DIR` (relative paths are taken from the workspace root) or `target`, and uses its `debug` directory. The build time does not count against a test's budget. Under nextest every test is its own process, so CI builds once and sets the variable:

```sh
cargo build -p devlanes -p devlanes-testing
DEVLANES_E2E_BIN_DIR="$PWD/target/debug" cargo nextest run --profile e2e
```

## API

- `Lab::new(fixture, budget)`, `Lab::with_config(yaml, budget)`, `Lab::without_config(budget)`: an isolated environment with a committed copy of the fixture, a given `devlanes.yaml`, or none.
- `Lab::docker(fixture, compose_project, budget)`, `Lab::docker_projects(fixture, projects, budget)`: the same for Docker tests, plus `Lab::docker_cli(args)` to run `docker` with the outer shell's client settings.
- `Lab::devlanes(cwd, args)` and `Lab::json(cwd, args)`: run one CLI command, bounded by 60 s and the test's budget.
- `Lab::command` plus `Lab::run` or `Lab::spawn`: the same command with a custom environment, or left running.
- `Lab::add_worktree(branch)`, `Lab::worktrees_dir()`, `Lab::lanes(cwd)`, `Lab::eventually(what, timeout, probe)`, `Lab::daemon_pid()`.

## Isolation

Each lab has a short `DEVLANES_HOME` under `/tmp` (the socket path must fit), a Git repository with a local identity and `commit.gpgsign=false`, an empty file as the global Git config, and an environment cleared down to `PATH` (with the binary directory first), `DEVLANES_AGENT=0` and `DEVLANES_OUTPUT=json`. Commands never see CI or agent markers from the outer shell.

Labs lock one of 60 slots of 100 ports, starting at 26000, through files in `/tmp/devlanes-e2e-ports-<uid>`. The slot holds the lane port pool and the proxy's HTTP and HTTPS ports, written to `$DEVLANES_HOME/config.yaml`, so parallel tests and a developer's own daemon never compete for 443 or 8443.

Dropping a lab runs `devlanes daemon stop --yes`, then sends SIGKILL to every PID the test saw in JSON output whose start time still matches. Docker labs also run `docker compose -p <project> down --volumes` for each of their Compose projects, before the test as well, so a run that died cannot leave data behind.

## License

Apache-2.0
