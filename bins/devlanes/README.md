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

On a terminal, `start`, `restart`, `lane resume` and `lane new --start` run inside the TUI, like mprocs: the request goes out first, and the TUI opens on the lane once the daemon has accepted it, so a refused start (no config, unknown unit) is a normal CLI error. The TUI follows the start's own events and exits 0 when it was ready, 1 when it failed, and 130 when the user quits before the result. `--no-ui`, `DEVLANES_NO_UI=1` or `tui.open_on_start: false` keep the text output; `start --attach` forces the TUI past the last two. Agent, JSON, plain, CI and piped output never open it. `with`, `wait`, `run` and `stop` stay text.

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

Any command except `devlanes daemon status`, `daemon stop` and `daemon reset-state` auto-spawns the daemon (`devlanes daemon run`, a hidden subcommand) when none is running; `daemon status` reports it as not running and exits 1. The daemon inherits only `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LC_*`, `TZ`, `TMPDIR`, `TERM`, `XDG_*`, `DOCKER_*`, `SSH_AUTH_SOCK`, the proxy variables, `DEVLANES_*` and CI markers from that shell. Services and tasks still get the environment of the shell that sent each `start`, `run` or `exec`, and Docker Compose gets the daemon's Docker-related variables plus `infra.env` and the primary worktree's `.env`. Inside an agent sandbox the CLI refuses to auto-spawn; start the daemon from a normal shell or install it once with `devlanes daemon install`. `devlanes daemon stop`, `devlanes daemon restart` and `devlanes daemon reset-state` are the only commands that stop all managed processes, and they ask first; `reset-state` also empties `state.db`.

Everything lives under `~/.devlanes`, or `DEVLANES_HOME` when set: `state.db`, `config.yaml`, `logs/` (including `daemon.log`), `ca/` and `run/daemon.sock`. When the socket path is too long for macOS, it moves to a short temporary directory. devlanes refuses a home that other users can read or write, so create it with `mkdir -m 700 ~/.devlanes` when you add `config.yaml` before the first run.

## Diagnostics

`-v` (or `DEVLANES_VERBOSE=1`) prints timestamped debug lines on stderr: the output mode and why it was chosen, the home, config and socket paths, connecting to or spawning the daemon and how long that took, the versions exchanged, each request with its duration, and why the exit code is what it is. The daemon adds its own lines for that request, marked `daemon +<ms>`: lane resolution, config reads, port allocation, the Docker Compose commands it runs and their output, start gate waits, readiness probes and each spawn with its program, working directory and environment variable names. The run ends with a table of timings per request and start phase. `-vv` (or `DEVLANES_VERBOSE=2`) adds trace detail and span durations. stdout is untouched, so `--json -v` still prints parseable JSON. Environment and secret values never appear, only variable names. `-v` keeps `start` out of the TUI so the lines stay visible.

The daemon writes its own log to `~/.devlanes/logs/daemon.log` (rotated at 10 MiB to `daemon.log.1`). `DEVLANES_LOG` sets its level with `RUST_LOG` syntax, such as `DEVLANES_LOG=debug` or `DEVLANES_LOG=info,devlanes_daemon=trace`; the default is `info`. The variable is read when the daemon starts, so restart it with `devlanes daemon restart` after changing it. `devlanes daemon logs` (`-f` to follow, `--tail N`) prints the file.

`devlanes debug bundle` writes a `.tar.gz` for bug reports and prints its path: client and daemon versions, `doctor`, `config check` and `status` as JSON, and the end of `daemon.log`. `devlanes debug bundle -- start api` also runs `devlanes -vv start api` and adds its output. Values of variables named like tokens, keys or passwords, passwords in URLs, `env` maps and the home directory are replaced before anything is written. It never starts a daemon. `--to PATH` chooses the file; by default it goes to the temporary directory.

```sh
devlanes -v start                   # what the client and daemon did, with timings
devlanes --json -vv start 2>trace.txt
DEVLANES_LOG=debug devlanes daemon restart
devlanes debug bundle -- start api
```

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
