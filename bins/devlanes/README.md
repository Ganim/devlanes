# devlanes

`devlanes` runs a project's dev environment once per git worktree, each in its own lane with its own ports, hostnames and data. This package is the single binary: the CLI, the TUI and the daemon, wired together with every concrete integration.

## Usage

```sh
devlanes start                      # infra, setup, tasks, then services of this worktree's lane
devlanes start web --profile e2e    # one service, with a profile
devlanes status                     # lanes of this project; outside a repo, all projects
devlanes status acme@2              # one lane, by project and lane number
devlanes logs web --tail 50 -f      # follow one unit's output
devlanes attach web                 # raw terminal; C-a d detaches
devlanes lane ls                    # worktree lanes, including idle and unassigned ones
devlanes with --profile e2e -- pnpm test:e2e
devlanes daemon status
devlanes daemon install             # launchd agent or systemd user unit
```

Run `devlanes` with no arguments on a terminal to open the TUI; elsewhere it prints `status`. Add `--json` to any command for machine-readable output. Closing the TUI, or pressing Ctrl-C during `start`, `logs -f` or `wait`, leaves everything running.

`devlanes with` starts the lane, waits until it is ready, runs the command with the lane's env and `DEVLANES_PUBLIC_URL_<SERVICE>` variables, stops what it started and exits with the command's code.

Configuration examples and design notes are in `docs/` at the repository root. The Claude Code and Codex plugin, with the agent skill, is in `agent-plugin/`.

## Config example

```yaml
# ~/.devlanes/config.yaml: per-user settings for every project.
# Ports the HTTPS and HTTP proxy listen on.
proxy:
  https_port: 443
  http_port: 80
# Terminal app the TUI's O key opens a unit in.
terminal: ghostty
```

Project settings live in each repository's `devlanes.yaml`; `docs/examples/user-config.yaml` lists every user setting.

## Behaviour

Any command auto-spawns the daemon (`devlanes daemon run`, a hidden subcommand) when none is running. Inside an agent sandbox the CLI refuses to auto-spawn; start the daemon from a normal shell or install it once with `devlanes daemon install`. `devlanes daemon stop`, `devlanes daemon restart` and `devlanes daemon reset-state` are the only commands that stop all managed processes, and they ask first; `reset-state` also empties `state.db`.

Everything lives under `~/.devlanes`, or `DEVLANES_HOME` when set: `state.db`, `config.yaml`, `logs/` (including `daemon.log`), `ca/` and `run/daemon.sock`. When the socket path is too long for macOS, it moves to a short temporary directory.

## Release notices

Three minutes after it starts, and then once a day, the daemon reads the version in `https://github.com/devlanes-run/devlanes/releases/latest/download/release-manifest.json` with `curl` and caches it in `update-check.json`. A failed request keeps the cached answer. A release is reported once it is newer than the daemon and has been seen for two hours, the time Homebrew and npm take to publish it. devlanes never downloads, installs or restarts anything itself.

In human output, a command then prints one line on stderr, at most once a day per version, with the upgrade command for how this binary was installed (Homebrew, npm, pnpm, Bun, Yarn, cargo binstall, cargo install or `install.sh`). When the daemon runs another version than the CLI, the line says to run `devlanes daemon restart` when convenient, since a restart stops services. Agent, JSON, plain and CI output never get these lines. The TUI shows the same notice in its header, and `u` hides it for that version. `devlanes doctor` always shows the version, build, install method, latest release and upgrade command. Shown and hidden notices are kept in `notices.json`.

Checks are off in debug builds, when the daemon starts with CI variables set, with `DEVLANES_NO_UPDATE_CHECK=1`, or with this in `config.yaml`:

```yaml
updates:
  check: false
```

## License

Apache-2.0
