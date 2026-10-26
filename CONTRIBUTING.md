# Contributing

devlanes is a Cargo workspace: one binary (`bins/devlanes`) built from small crates under `crates/`. Each crate has a README saying what it does, and `AGENTS.md` has the code conventions. This file covers setting up, running the CLI from a checkout and testing each area.

## Setup

- Rust comes from `rust-toolchain.toml`; rustup installs the pinned version on the first `cargo` call.
- Install nextest: `cargo install cargo-nextest --locked`.
- Install kache, the build cache every compile goes through (`.cargo/config.toml` sets it as rustc wrapper): `cargo install kache --locked`, then `kache init` once.
- macOS (Apple Silicon and Intel), Linux with glibc (x86_64 and arm64) and Windows 11 24H2 or later (x64 and arm64) are supported.
- On Windows, also install the Visual Studio Build Tools (the "Desktop development with C++" workload) and [Git for Windows](https://gitforwindows.org). kache 1.2.2 can't wrap the C compiler there, so build kache from a commit with the fix, outside the checkout and without the wrapper: `cargo install --locked --git https://github.com/kunobi-ninja/kache --rev a37fb5ff3bcc86adfded7f3ecb4f7c5100f744c2 kache` with `RUSTC_WRAPPER` unset. Switch to `cargo install kache` once a release includes it.

Optional tools, needed only by the ignored tests that mention them: Docker (OrbStack or Docker Desktop on macOS, Docker Desktop on Windows), `node` with npm/pnpm/yarn/bun, `just`, `deno`, `uv`, `poetry`, `mise`, `direnv`, `certutil` (NSS), `dig`, `tmux`.

CI also shares kache's cache through a remote bucket. Pushes to `main` write to it, and pull requests write to a separate prefix that `main` never reads. Contributors don't need the remote; kache's local cache is enough, and connecting to a remote is an optional setting in your own `~/.config/kache/config.toml`.

## Checks

Run these before sending a change. CI runs the same ones on macOS, Linux and Windows.

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
git ls-files -z '*.sh' | xargs -0 shellcheck
actionlint
sh packaging/scripts/check.sh   # release scripts and install.sh against fake binaries
```

The agent plugin's hooks run `devlanes __agent-hook`, which lives in `crates/agent-hooks`; `bins/devlanes/tests/it/agent_hook.rs` runs the plugin's hook command lines in the shells the agents use.

`install.ps1`, `package.ps1`, the npm launcher and the Windows wheel have a Windows check against a fake `devlanes.exe`, which CI runs in its `packaging-windows` job. It needs PSScriptAnalyzer (CI pins 1.25.0), both `powershell.exe` and `pwsh`, `node` and `python`. It changes your user `PATH` while it runs and writes the old value back at the end:

```powershell
pwsh packaging/scripts/check.ps1
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

The daemon's proxy listens on 443 and 80, and falls back to 8443 and 8080 when it can't bind them. macOS and Windows let unprivileged processes bind both; on Linux, run `sudo setcap cap_net_bind_service=+ep target/debug/devlanes` or use the fallback ports.

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

`crates/hosts-helper/README.md` explains what that installs and why. On Windows there is no helper: `*.localhost` resolves natively, `certs trust` shows a Windows security warning instead of a password prompt, `curl.exe` needs `--ssl-revoke-best-effort`, and `certs trust --resolver` writes an NRPT rule after a UAC prompt ([Windows](docs/guides/windows.md)).

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

`e2e/` runs the real `devlanes` binary against the projects in `fixtures/`. The default nextest profile skips it (`.config/nextest.toml`); the `e2e` profile runs only it. Without `DEVLANES_E2E_BIN_DIR`, each test process on macOS and Linux builds `devlanes` and `fixture-server` with cargo first; set it to skip that. On Windows tests never run cargo and take the binaries next to the test binary, so the build below is required there:

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
| `devlanes-env-mise`, `devlanes-env-direnv` | mise, direnv |
| `devlanes-trust-macos` | someone to click the keychain password prompt |
| `devlanes-trust-linux` | sudo and certutil |
| `devlanes-trust-windows` | a desktop session, to answer the two Windows security warnings for the real user root store |
| `devlanes-resolver-nrpt` | a desktop session to approve two UAC prompts, and nothing (no devlanes daemon) on `127.53.0.1:53`; it writes a rule under `HKEY_LOCAL_MACHINE` and removes it |
| `devlanes-resolver-dns` | dig |
| `devlanes-daemon` | `DEVLANES_FIXTURE_SERVER=$PWD/target/debug/fixture-server` |

## Generated files

`crates/config/schema.json` and `crates/config/user-schema.json` are rewritten by `cargo build -p devlanes` whenever the config types change. Commit them together with the change; a test fails when they're stale.

`crates/tui/screens/*.txt` and its `index.md` are every TUI state drawn against a fake daemon, as text with the styles of each cell, so a pull request shows what changed on screen. Any change to what the TUI draws changes them, including changes outside `crates/tui`. Rewrite them with `DEVLANES_BLESS=1 cargo nextest run -p devlanes-tui -E 'test(screens_are_current)'`, check the diff and commit them with the change; the test fails in CI while they're stale. `cargo run -p devlanes-tui --example screens` writes the same files plus gitignored PNG and ANSI versions for looking at the screens.

## Change files

A pull request that changes something a devlanes user can notice adds a change file in `.sampo/changesets/`. [Sampo](https://github.com/bruits/sampo) reads these files to choose the next versions and write each crate's `CHANGELOG.md` in the release pull request. Refactors, tests, CI and documentation need none; say so in the pull request description. No check enforces it, so reviewers look for one.

Create a change file with `sampo add` (`cargo install --locked sampo`), without prompts:

```sh
sampo add -p cargo/devlanes -p cargo/devlanes-daemon -b patch -m 'Fixed `devlanes start` leaving services stopped after a config reload.'
```

or write it by hand under any name ending in `.md`:

```markdown
---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
---

Fixed `devlanes start` leaving services stopped after a config reload.
```

Write one change file per feature or fix a user would look for in the changelog, not one per adjustment made along the way. Small tweaks and fixes to the same part of devlanes go in one entry, so these two:

```markdown
Changed the TUI monitor (`M`) to keep each project's shared infra on top and its lanes below in lane order, instead of reordering them by usage.
```

```markdown
Fixed TUI monitor (`M`) graphs drawing history as separate stripes for the time before the monitor was open.
```

belong in one file:

```markdown
Changed the TUI monitor (`M`) to keep each project's shared infra on top and its lanes in lane order, and fixed its graphs drawing separate stripes for the time before it was open.
```

Separate features keep separate files: a new daemon row in the monitor and a rescaled memory graph are two entries. A fix or tweak to something the same pull request adds needs no entry, since users never saw the earlier version. If the work grows during review, edit its change file instead of adding another, and reread all of the pull request's change files before pushing.

### Crates

List the crates whose code changed in a way their users notice, as `cargo/<package name>`. Add `cargo/devlanes` whenever someone running devlanes notices: its changelog is the one people read, and Sampo only writes an entry there when the change file names it. Don't list crates that only depend on a changed one; Sampo bumps them on its own.

### Bump

Until 1.0, use `minor` for a change that breaks something that worked: a removed or renamed command, flag or config key, a config that no longer loads, or a protocol or on-disk state change that needs a daemon restart or a migration. Use `patch` for everything else, new features included. Never use `major`: Sampo applies plain SemVer, so it would release 1.0.0.

### Wording

The first line is the changelog entry, read by someone deciding whether to upgrade. Write one sentence that:

- starts with Added, Changed, Fixed, Deprecated or Removed;
- names the command, flag or config key in backticks;
- says what changes for the user, not how the code changed. Leave out crate, module and type names unless a crate's API is the change.

Most change files are that one line. Add a body after a blank line only when the line can't carry what a user needs to act on:

- a breaking change or removal: what stops working and what to use instead;
- a deprecation: what replaces it and when the old form goes away;
- a large feature: the smallest config or command that gets someone started, and a link to the guide in `docs/`.

Keep a body to a few lines and at most one short example. Don't restate the documentation, explain the implementation, or list every option.

```markdown
---
cargo/devlanes: minor
cargo/devlanes-config: minor
---

Removed the `services.<name>.legacy_env` config key.

Configs that still set it fail to load. Move its entries to `services.<name>.env`.
```

Lines that don't belong in a changelog:

- `Improved the daemon's handling of edge cases.` says nothing a user can check.
- `Refactored the lane registry to share state behind a lock.` describes the code, not the effect.
- `Added a powerful new way to manage ports seamlessly across lanes.` is marketing; say what was added.

The `Writing style` rules in `AGENTS.md` apply to change files too. Before committing one, cut every word a user wouldn't miss.

### Versions

Don't edit versions or `CHANGELOG.md` files by hand; the release pull request does. Crates have their own versions, and `devlanes` and `devlanes-hosts-helper` carry the release version (`packaging/README.md`). A new crate needs an entry in `packages.linked` of `.sampo/config.toml`, so its changes reach the `devlanes` version. Code that reports the devlanes version uses `devlanes_version::VERSION`, not its own crate's `CARGO_PKG_VERSION`.

## Releases

Merging the release pull request, which `.github/workflows/sampo.yml` keeps up to date on every push to `main`, releases everything; `packaging/README.md` describes the workflow and the Homebrew and npm packages.

## License

Contributions are licensed under Apache-2.0, the license of this repository.
