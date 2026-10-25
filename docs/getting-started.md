# Getting started

devlanes runs a project's dev services once per Git worktree. Each worktree is a lane with its own ports, hostnames and data, and a per-user daemon keeps the processes running after the terminal closes. This page goes from install to two lanes running side by side.

## Install

```sh
brew install devlanes-run/tap/devlanes
```

The [README](../README.md#install) lists the npm, cargo and `install.sh` options and the source build. devlanes runs on macOS, Linux and Windows 11 ([Windows](guides/windows.md) covers its install and differences).

## First config

Put a `devlanes.yaml` at the root of the repository and commit it, so every worktree has it:

```yaml
project: acme
services:
  api:
    run: pnpm --filter api dev
    hosts: "api.${{ lane.domain }}"
  web:
    preset: vite
    run_script:
      file: web/package.json
      name: dev
    env:
      VITE_API_URL: "${{ services.api.public_url }}"
    hosts: "${{ lane.domain }}"
```

devlanes gives every service a port from the lane's block and passes it as `PORT`. A service must listen on that port; `preset: vite` adds Vite's `--port` and `--host` flags for you. A preset needs `run_script`, which names a script in a project file; prefer it to `run` whenever the command is such a script ([run and run_script](guides/run-scripts.md)). `lane.domain` is `acme.localhost` in lane 0 (the worktree on the primary branch) and `acme-<N>.localhost` in lane N. Expressions such as `${{ services.api.public_url }}` are evaluated per lane, so the web app of lane 2 talks to the API of lane 2.

`devlanes config check` validates the file and points at the line of any mistake. [examples/minimal.yaml](examples/minimal.yaml) adds shared Postgres with a database per lane, [examples/devlanes.yaml](examples/devlanes.yaml) shows every form, and the [config reference](reference/config.md) lists each field.

### Coming from another process manager

If the repository already runs its dev processes with mprocs, dekit, process-compose, a Procfile (foreman, overmind, hivemind, honcho), pm2, tmuxinator, tmuxp, `concurrently` or `npm-run-all` in package.json, mise tasks or Docker Compose, install the [devlanes skill](../agent-plugin/README.md) and ask your coding agent to migrate it. The skill's [references/migration.md](../agent-plugin/skills/devlanes/references/migration.md) maps each tool to `devlanes.yaml`: fixed ports become each lane's own port, one-shot commands become tasks, containers become infra, and the agent tells you what devlanes cannot express (restart policies, dependencies between services, `.env` loading).

## Start and inspect

```sh
devlanes start           # this worktree's lane, followed in the TUI
devlanes status          # lanes and units with state, ports, URLs, CPU and memory
devlanes logs api -f     # follow one service; Ctrl-C stops following, not the service
devlanes url ls          # routed hostnames
devlanes url open web    # open a service in the browser
devlanes stop            # stop the lane
```

The daemon starts on first use, and stops on its own about 30 seconds after the last TUI closes while nothing runs (`daemon.exit_when_idle`). `start` is safe to repeat: running services are left alone.

On a terminal, `start` opens the TUI on the lane and runs the start inside it: the header shows the phase (infra, setup, tasks, services), the tree shows each service coming up, and a service that fails shows its reason and the end of its log in the details pane. When the start ends, the header keeps the result and the TUI stays open. Quitting keeps services running unless you choose otherwise: while lanes run, `q` asks whether to keep or stop them (`tui.on_quit` in `~/.devlanes/config.yaml` answers for you). The exit status is 0 when the start was ready, 1 when it failed, and 130 when you quit before it ended (the start keeps going in the daemon). A start the daemon refuses outright, such as a worktree without `devlanes.yaml` or an unknown unit, prints its error before any TUI opens. `restart`, `lane resume` and `lane new --start` work the same way.

`--no-ui`, `DEVLANES_NO_UI=1` or `tui.open_on_start: false` in `~/.devlanes/config.yaml` print the start as text instead: it returns once every service is ready, and a failure exits 1 with the end of the service's log. Agents, scripts, CI and piped output always get the text. `devlanes debug why api` explains why a unit is not up.

For a second lane, add a worktree and start it there:

```sh
git worktree add ../acme-checkout -b feat/checkout
cd ../acme-checkout
devlanes start
```

A worktree becomes a lane the first time devlanes runs in it (here, `start`), or when `devlanes lane new <branch>` creates it; a worktree that only exists gets no lane, except the one on the primary branch. The daemon follows moved and removed worktrees by itself. `devlanes lane ls` lists every lane, and commands take a lane reference such as `2`, `feat/checkout` or `acme@2:api` from any directory. Removing a worktree stops its lane, and a lane whose folder is gone is cleaned once nothing runs in it; its logs stay until log retention removes them. `devlanes lane clean <lane> --yes` removes a lane's data and frees its ports by hand.

To run a folder twice, for example with two profiles, `devlanes lane new --name review` in it adds a second lane with its own index, ports and domain. `--lane review` addresses it; commands run in the folder keep using its first lane.

## HTTPS and certificates

The proxy serves `https://api.acme.localhost` and the other hostnames on port 443, or 8443 when 443 cannot be bound. Browsers resolve `*.localhost` to your machine without any setup. To make browsers and command-line tools accept the certificates, trust the devlanes CA once:

```sh
devlanes certs trust            # system and browser trust stores; asks for your password
devlanes certs trust --hosts    # also for programs that cannot resolve *.localhost
devlanes doctor                 # checks the daemon, Docker, proxy, CA and hosts file
```

Node and Python services get the CA through `NODE_EXTRA_CA_CERTS` and `SSL_CERT_FILE` without trusting it system-wide.

## TUI

Run `devlanes` with no command (or `devlanes ui`) on a terminal. It lists every lane of the project, the state of each unit and a live terminal for the selected one. Closing the TUI leaves everything running.

## Coding agents

Agents should start services through devlanes instead of backgrounding `pnpm dev &` in their shell. The [agent plugin](../agent-plugin/README.md) packages a guide for them as a Claude Code and Codex plugin, a skill for the skills CLI and a Codex `AGENTS.md` to paste into a repository; the plugin also has a hook that rejects backgrounded dev servers. When devlanes detects an agent it switches to terse output with failures first; `devlanes exec --name NAME -- COMMAND` runs a one-off process as a unit, and `devlanes wait` and budgeted `devlanes logs` let the agent check on it.

In CI, `devlanes with -- pnpm test:e2e` starts the lane, runs the command with the lane's URLs in its environment, stops what it started and exits with the command's code.

## GUI tools

devlanes does not stream windows. A GUI tool that serves HTTP or TCP is an ordinary service: give it the service's port and, for a browser UI, a hostname.

```yaml
services:
  ios-sim:
    run: npx serve-sim --port ${{ self.port }}
    hosts: "sim.${{ lane.domain }}"
    autostart: false
```

Xpra's HTML5 client, ws-scrcpy and an `adb` server follow the same pattern. On a remote box, `devlanes remote connect` mirrors these ports and hostnames to your machine; only TCP is mirrored. [Remote boxes](remote.md#gui-tools-as-services) has the Xpra and adb examples.

## Troubleshooting

When a service fails, the start names it with the reason and a next command. A program that cannot run says which one and why, for example `api could not start: fixture-server: command not found on PATH`. Services get the PATH of the shell that ran `devlanes start`, and `devlanes env show api` prints the environment the service got. The same line is in the service's log, so `devlanes logs api` shows it too.

```sh
devlanes debug why api          # why a unit is not running or not ready, with a fix
devlanes debug errors           # failed units, crash counts and recent error lines
devlanes -v start               # what the CLI and the daemon did, step by step, on stderr
devlanes daemon logs --tail 200 # the daemon's own log
devlanes doctor                 # daemon, Docker, proxy, CA and hosts file
```

`-v` works with every command and output mode and ends with a table of how long each step took; `-vv` prints more. To report a problem, `devlanes debug bundle -- start` writes an archive with versions, `doctor`, `status`, the end of the daemon's log and a verbose run of `devlanes start`, with secret-looking values removed, and prints where it is. Read it before you send it.

For more detail in the daemon's log, restart it with a level: `DEVLANES_LOG=debug devlanes daemon restart`.

## Next steps

- [CLI reference](reference/cli.md): every command and option.
- [Config reference](reference/config.md): every field of `devlanes.yaml` and the user `config.yaml`.
- Guides: [run and run_script](guides/run-scripts.md), [languages](guides/languages.md), [framework presets](guides/presets.md), [ports and environment](guides/ports-and-env.md), [hostnames and HTTPS](guides/https.md), [infra](guides/infra.md), [service patterns](guides/services.md), [terminals](guides/terminals.md), [remote setup](guides/remote-setup.md).
- [Remote boxes](remote.md): run lanes on another machine and use them locally.
- [Design](design.md): lanes, ports, the daemon and startup order.
- [Observability](observability.md): the daemon's spans and diagnostics, and plans for tracing and benchmarks.
