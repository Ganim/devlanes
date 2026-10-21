# Contributing

devlanes is a Cargo workspace: one binary (`bins/devlanes`) built from small crates under `crates/`. Each crate has a README saying what it does, and `AGENTS.md` has the code conventions. This file covers setting up, running the CLI from a checkout and testing each area.

## Setup

- Rust comes from `rust-toolchain.toml`; rustup installs the pinned version on the first `cargo` call.
- Install nextest: `cargo install cargo-nextest --locked`.
- Install kache, the build cache every compile goes through (`.cargo/config.toml` sets it as rustc wrapper): `cargo install kache --locked`, then `kache init` once.
- macOS (Apple Silicon and Intel) and Linux with glibc (x86_64 and arm64) are supported. Windows support is in progress.

On Windows you also need:

- Visual Studio Build Tools with the C++ workload (`cl.exe`), for crates with C code such as `ring` and `libsqlite3-sys`.
- Git for Windows; its `sh.exe` runs `run:` strings.
- kache built from git. kache 1.2.2 can't pass `cl.exe` through, so those C crates fail to build; the fix ([kunobi-ninja/kache#1640](https://github.com/kunobi-ninja/kache/pull/1640)) is merged but not released yet. Build it without the wrapper, since the released kache would hit the same bug on kache's own C dependencies:

  ```sh
  env -u RUSTC_WRAPPER cargo install --locked --git https://github.com/kunobi-ninja/kache --rev a37fb5ff3bcc86adfded7f3ecb4f7c5100f744c2 kache
  ```

Optional tools, needed only by the ignored tests that mention them: Docker (OrbStack or Docker Desktop on macOS), `node` with npm/pnpm/yarn/bun, `just`, `deno`, `uv`, `poetry`, `mise`, `direnv`, `fnox` 1.39+, `certutil` (NSS), `dig`, `tmux`.

CI also shares kache's cache through a remote bucket when the repository has one configured, and falls back to the Actions cache when it hasn't. Contributors don't need the remote; kache's local cache is enough, and connecting to a remote is an optional setting in your own `~/.config/kache/config.toml`.

## Checks

Run these before sending a change. CI runs the same ones on macOS and Linux.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo test --workspace --doc --all-features
cargo build --workspace --all-targets --all-features
```

While working on one crate, test just that crate: `cargo nextest run -p devlanes-config`.

Changes to shell scripts or workflows also have a check, which CI runs in its `packaging` job:

```sh
shellcheck install.sh packaging/scripts/*.sh packaging/homebrew/render.sh packaging/npm/build.sh agent-plugin/scripts/*.sh e2e/ssh/run.sh
actionlint
sh packaging/scripts/check.sh                       # release scripts and install.sh against fake binaries
sh agent-plugin/scripts/test-block-background.sh    # the Claude Code hook
```

## Running the CLI from a checkout

Use a separate state directory so your development daemon never touches a real install in `~/.devlanes`:

```sh
cargo build -p devlanes -p devlanes-testing
export DEVLANES_HOME="$PWD/target/dev-home"
alias dl="$PWD/target/debug/devlanes"

dl doctor
dl daemon status
```

The first command that needs the daemon starts it in the background, from the same binary (`devlanes daemon run`). After rebuilding, restart it so it runs the new code:

```sh
cargo build -p devlanes && dl daemon restart --yes
dl daemon logs -f                              # the daemon's log, $DEVLANES_HOME/logs/daemon.log
DEVLANES_LOG=debug dl daemon restart --yes     # more detail in that log
```

`DEVLANES_OUTPUT=human|plain|agent|json` forces an output mode. When you run the CLI from inside an AI agent session it detects the agent and prints agent output; `DEVLANES_AGENT=0` turns that off.

## Trying a project

`fixtures/` has small projects that use `fixture-server`, a test HTTP server from `crates/testing`, so they run without Node or Docker. Lanes are git worktrees, so copy a fixture into a temporary repo:

```sh
repo="$PWD"
work=$(mktemp -d)
cp fixtures/two-services/devlanes.yaml "$work/" && cd "$work"
git init -q -b main && git add . && git commit -qm init
git worktree add -q -b feat/x "$work-x"

export PATH="$repo/target/debug:$PATH"   # fixture-server must be on PATH
dl start                                 # lane 0, on main
(cd "$work-x" && dl start)               # lane 1, on feat/x
dl status
dl logs api --tail 20
dl ui                                    # the TUI; q quits and leaves everything running
dl stop && dl daemon stop --yes
```

`fixtures/many-services` starts 40 servers, which is useful for TUI and startup-pacing work. The `dogfood-*` fixtures are config-only (they reference compose files that don't exist) and are checked by the `devlanes-config` tests.

## HTTPS and hostnames

The daemon's proxy listens on 443 and 80, and falls back to 8443 and 8080 when it can't bind them. macOS lets unprivileged processes bind both; on Linux, run `sudo setcap cap_net_bind_service=+ep target/debug/devlanes` or use the fallback ports.

```sh
dl url ls                                   # routed hostnames with their lane, unit and port
curl --cacert "$DEVLANES_HOME/ca/ca.pem" https://api.fixture-two.localhost/healthz
dl certs trust                              # installs the dev CA (asks for your password)
dl certs untrust
```

The `/etc/hosts` helper needs a release build, because its installer refuses debug builds:

```sh
cargo build --release -p devlanes-hosts-helper
DEVLANES_HOSTS_HELPER="$PWD/target/release/devlanes-hosts" dl certs trust --hosts   # CA + helper
dl certs untrust --hosts             # removes both
dl certs untrust --hosts --keep-ca   # removes only the helper
```

`crates/hosts-helper/README.md` explains what that installs and why.

## Docker and compose

Docker tests are ignored by default and named `docker_*`:

```sh
cargo nextest run --workspace --run-ignored only -E 'test(/^docker_/)'
```

They create containers labelled `dev.devlanes.*` and remove them afterwards. To try infra by hand, add an `infra:` section to a fixture copy (see `docs/examples/devlanes.yaml`) and use `dl infra up`, `dl infra status` and `dl lane clean <lane> --yes`.

## Remote and SSH

Remote is mirroring only: a client never runs commands on a box. Most of it is tested without SSH. `crates/daemon/tests/it/remote.rs` runs a box daemon and a client daemon in one process over an in-memory transport, covering mirrored ports, proxy routes, refused daemon requests, sync, reconnect and failure:

```sh
cargo nextest run -p devlanes-daemon -E 'test(/remote/)'
```

The real SSH path runs against a throwaway sshd container (`e2e/ssh/README.md`):

```sh
e2e/ssh/run.sh                              # builds a Linux devlanes for the box, runs the SSH tests
DEVLANES_TEST_SSH_BIN=skip e2e/ssh/run.sh   # no box binary; only the connection test runs
```

To use `dl remote connect` by hand you need a box with `devlanes` on its PATH (or pass `--install`) and an entry for it in `~/.ssh/config`. A Linux VM or a spare machine works best: a box on the same machine as the client is confusing, because mirroring binds the box's port numbers locally and they collide with the box's own listeners.

```sh
dl remote connect mybox      # streams connection status; Ctrl-C detaches, the connection stays up
dl remote ls                 # mirrored ports, routes, syncs and warnings
ssh mybox devlanes status    # lanes are managed on the box itself
dl remote disconnect mybox
```

## End-to-end tests

`e2e/` runs the real `devlanes` binary against the projects in `fixtures/`. The default nextest profile skips it (`.config/nextest.toml`); the `e2e` profile runs only it. Without `DEVLANES_E2E_BIN_DIR`, each test process builds `devlanes` and `fixture-server` with cargo first; set it to skip that:

```sh
cargo build -p devlanes -p devlanes-testing
export DEVLANES_E2E_BIN_DIR="$PWD/target/debug"
cargo nextest run --profile e2e
cargo nextest run --profile e2e --run-ignored only -E 'test(/^docker_/)'   # needs Docker
```

`e2e/README.md` covers isolation, port slots and debugging.

## Tests that need other tools

Run a crate's ignored tests with `cargo nextest run -p <crate> --run-ignored only`.

| Crate | Needs |
|---|---|
| `devlanes-run-script-npm` | node with npm, pnpm, yarn and bun |
| `devlanes-run-script-just`, `-cargo`, `-deno`, `-python` | just, cargo, deno, uv or poetry |
| `devlanes-env-mise`, `devlanes-env-direnv`, `devlanes-secrets-fnox` | mise, direnv, fnox 1.39+ |
| `devlanes-trust-macos` | someone to click the keychain password prompt |
| `devlanes-trust-linux` | sudo and certutil |
| `devlanes-resolver-dns` | dig |
| `devlanes-daemon` | `DEVLANES_FIXTURE_SERVER=$PWD/target/debug/fixture-server` |

## Generated files

`crates/config/schema.json` and `crates/config/user-schema.json` are rewritten by `cargo build -p devlanes` whenever the config types change. Commit them together with the change; a test fails when they're stale.

## Releases

`packaging/README.md` describes the tag-driven release workflow and the Homebrew and npm packages.

## License

Contributions are licensed under Apache-2.0, the license of this repository.
