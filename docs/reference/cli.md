# CLI reference

<!-- Generated from the clap definitions in crates/cli by crates/cli/tests/it/reference_docs.rs. Edit the help text in crates/cli, then run DEVLANES_BLESS=1 cargo nextest run -p devlanes-cli. -->

Every `devlanes` command and option with its help text; `devlanes <command> --help` prints the same.

Run a dev environment per git worktree, side by side.

Every worktree of a project is a lane with its own ports, hostnames such as
web.acme-2.localhost and environment, so branches run at the same time. A
per-user daemon owns the processes and containers and keeps them running when
the terminal closes; this command and the TUI are its clients. Without a
command, devlanes opens the TUI on a terminal and prints status otherwise.

```text
devlanes [OPTIONS] [COMMAND]
```

```text
Examples:
  devlanes start                                bring up this worktree's lane
  devlanes status                               lanes and units of this project
  devlanes logs acme@2:api --tail 200           last 200 lines of api in lane 2
  devlanes with --profile e2e -- pnpm test:e2e  start, run the tests, stop
  devlanes stop --mine                          stop what this agent session started

References:
  A lane is [project@]lane: a lane number (2), a branch (feat/checkout), a
  lane or worktree directory name, or a path. Without project@ the project of
  the current directory is used.
  A unit is [lane:]unit, such as api, 2:api or acme@feat/checkout:web.
  Commands that take several units also accept globs ("api-*") and tags (+web).
```

## Global options

Accepted by every command, before or after the command name.

- `--json`: Print JSON: one document, or NDJSON for streams (same as --output json)
- `--output <MODE>`: Output mode [default: agent when an AI agent runs devlanes, plain in CI or without a terminal, else human]
  Values: `human` (Colors and symbols, for a terminal), `plain` (The same information without color, one line per event), `agent` (Terse lines for AI agents: failures first, with next commands, size-capped), `json` (The daemon's data as JSON, or NDJSON for streams).
- `--project <PROJECT>`: Use this project instead of the one containing the current directory
- `--lane <LANE>`: Use this lane ([project@]lane) instead of the current worktree's
- `--yes`: Confirm actions that would ask first; needed when no terminal can prompt
- `--quiet`: Print nothing on success; errors still go to stderr
- `--env <KEY=VALUE>`: Set a variable for started processes (start, restart, run TASK, exec); repeatable
- `--timeout <DURATION>`: Limit waits and streams, such as 30s, 5m or 1h [default for start and wait: 180s]
- `--budget <BYTES>`: Cap agent-mode output at this many bytes [default: 16384, minimum: 256]
- `-v, --verbose`: Print timestamped diagnostics of the client and daemon to stderr; -vv for more [env: DEVLANES_VERBOSE=1|2]

## Commands

### devlanes start

Bring up a lane: infra, setup, tasks and services, then wait until ready.

Starts the lane of the current worktree, or only the named units. Units that
already run are left alone, so starting twice is safe. Waits
until every started unit is ready or one fails; Ctrl-C detaches and leaves
everything running.

```text
devlanes start [OPTIONS] [UNIT]...
```

- `[UNIT]...`: Units to start ([lane:]unit, globs, +tag) [default: the whole lane]
- `--profile <PROFILE>`: Start the units of this profile from devlanes.yaml; repeat to combine profiles (later ones win on env)
- `--attach`: Run the start in the TUI even when DEVLANES_NO_UI or tui.open_on_start turn that off [default on a terminal]
- `--no-ui`: Print the start as text instead of running it in the TUI
- `--all-at-once`: Start every selected service immediately, bypassing the machine-wide start gate for this run

```text
Examples:
  devlanes start                       the whole lane of this worktree
  devlanes start api web               only these services
  devlanes start --profile e2e         the units of the e2e profile
  devlanes start 2:+api --timeout 5m   units tagged api in lane 2
```

### devlanes stop

Stop units, or the whole lane when none are named

```text
devlanes stop [OPTIONS] [UNIT]...
```

- `[UNIT]...`: Units to stop ([lane:]unit, globs, +tag) [default: the whole lane]
- `--mine`: Only stop units this agent session started; with no units, across every lane
- `--logs-dir <DIR>`: Write each unit's log of this run to DIR as text files, for CI artifacts

### devlanes restart

Stop and start units again with a fresh environment

```text
devlanes restart [OPTIONS] [UNIT]...
```

- `[UNIT]...`: Units to restart ([lane:]unit, globs, +tag) [default: pick from a list]
- `--all-at-once`: Start every selected service immediately, bypassing the machine-wide start gate for this run
- `--no-ui`: Print the restart as text instead of running it in the TUI

### devlanes status

Show lanes and units with state, ports, URLs, CPU and memory

```text
devlanes status [OPTIONS] [LANE]
```

- `[LANE]`: Lane to show ([project@]lane) [default: every lane of the current project]
- `--watch`: Keep printing lane changes until interrupted
- `--mine`: Only show units this agent session started

### devlanes logs

Read unit logs; each line carries a cursor to continue from.

Lines of several units are merged by time. Each line starts with a cursor such
as api#120 that --cursor and --around accept. In agent mode and without a
terminal the output defaults to the last 100 lines within the output budget.

```text
devlanes logs [OPTIONS] [UNIT]...
```

- `[UNIT]...`: Units to read ([lane:]unit, globs, +tag) [default: pick from a list]
- `--all`: Read every unit of the lane, grouped by unit
- `--tail <N>`: Show the last N lines [default: 100 in agent mode or without a terminal]
- `--head <N>`: Show the first N lines of the selected range
- `-f, --follow`: Keep streaming new lines; agent mode requires --timeout
- `--since <WHEN>`: Only lines newer than a duration (5m), a time in milliseconds, or start (the lane's last start)
- `--grep <REGEX>`: Only lines matching this regular expression
- `--level <LEVEL>`: Only lines at this level or above: debug, info, warn or error
- `--around <CURSOR>`: Show the lines around this cursor, such as api#120
- `-C, --context <N>`: Lines before and after --around
  Default: `3`.
- `--cursor <CURSOR>`: Continue after this cursor, as printed by an earlier logs call
- `--mine`: Only units this agent session started

```text
Examples:
  devlanes logs api --tail 200
  devlanes logs acme@2:api --since 5m --grep 'timeout|refused'
  devlanes logs api -f --timeout 30s
  devlanes logs --all --since start
```

### devlanes wait

Wait until a unit or lane is ready or failed.

Exits 0 when ready, 1 when it failed (with the reason and log tail) and 124
when --timeout passes first [default: 180s].

```text
devlanes wait [OPTIONS] [TARGET]
```

- `[TARGET]`: Unit ([lane:]unit) or lane ([project@]lane) to wait for [default: pick from a list]

### devlanes with

Start the lane, run a command against it, then stop what was started.

Waits for ready first; when a unit fails it prints the failure and exits 1
without running the command. The command gets the lane's environment, including
DEVLANES_PUBLIC_URL_<SERVICE> for every service. Afterwards devlanes writes
--logs-dir, prints peak CPU and memory when output is not a terminal (CI logs,
pipes), stops what it started (and the daemon, if it started it) and exits with
the command's exit code.

```text
devlanes with [OPTIONS] [UNIT]... -- <COMMAND>...
```

- `[UNIT]...`: Units to start ([lane:]unit, globs, +tag) [default: the whole lane]
- `--profile <PROFILE>`: Start the units of this profile from devlanes.yaml; repeat to combine profiles (later ones win on env)
- `--logs-dir <DIR>`: Write every unit's log of the run to DIR as text files, for CI artifacts
- `--keep`: Leave the lane running afterwards, for debugging
- `--all-at-once`: Start every selected service immediately, bypassing the machine-wide start gate for this run
- `<COMMAND>...`: Command and arguments to run once the lane is ready

```text
Examples:
  devlanes with -- pnpm test
  devlanes with --profile e2e -- pnpm test:e2e
  devlanes with api --logs-dir devlanes-logs -- cargo test --test api
```

### devlanes run

Run a task from devlanes.yaml and exit with its exit code.

The task's output streams to this terminal. Ctrl-C asks before stopping a
foreground task on a terminal and stops it in agent mode. Tasks with confirm:
need --yes when no terminal can prompt.

```text
devlanes run [OPTIONS] [TASK]
```

- `[TASK]`: Task to run, as named under tasks: in devlanes.yaml [default: pick from a list]
- `--list`: List the tasks and the script sources they use
- `--detach`: Start the task in the background and return

### devlanes exec

Run an ad-hoc command as a unit of the lane instead of backgrounding it with &.

The daemon owns the process: logs, stop, attach and status work on it by its
--name, and it gets the lane's environment.

```text
devlanes exec [OPTIONS] --name <NAME> -- <COMMAND>...
```

- `--name <NAME>`: Unit name for the command, used by logs, stop and attach
- `--no-pty`: Use pipes instead of a pseudoterminal
- `<COMMAND>...`: Command and arguments to run

### devlanes attach

Attach this terminal to a unit's terminal; C-a d detaches

```text
devlanes attach [OPTIONS] [UNIT]
```

- `[UNIT]`: Unit to attach to ([lane:]unit) [default: pick from a list]

### devlanes ui

Open the TUI; closing it never stops anything

```text
devlanes ui [OPTIONS]
```

### devlanes lane

Create, list, clean and resume worktree lanes

```text
devlanes lane [OPTIONS] <COMMAND>
```

### devlanes lane ls

List lanes, including moved, unassigned and missing ones

```text
devlanes lane ls [OPTIONS]
```

### devlanes lane new

Create a git worktree for a branch under lanes.worktree_dir and register its lane.

Without a branch, add another lane for the current folder: it has its own index, ports
and domain, and `--lane <name>` or `--lane <index>` addresses it. Commands run in the
folder keep using the folder's first lane.

Without --start, a terminal asks whether to start the new lane right away.

```text
devlanes lane new [OPTIONS] [BRANCH]
```

- `[BRANCH]`: Branch to check out in the new worktree [default: another lane for this folder]
- `--name <NAME>`: Name of the extra lane for this folder, used by `--lane` [default: <folder>-<n>]
- `--start`: Start the new lane right away
- `--no-ui`: With --start, print the start as text instead of running it in the TUI

### devlanes lane clean

Stop a lane, run its clean tasks and teardown, and free its ports (needs --yes)

```text
devlanes lane clean [OPTIONS] [LANE]
```

- `[LANE]`: Lane to clean ([project@]lane) [default: pick from a list]

### devlanes lane gc

Clean lanes whose worktrees are gone and drop unused port offsets (needs --yes)

```text
devlanes lane gc [OPTIONS]
```

### devlanes lane resume

Start the lanes that were running before a reboot or daemon restart.

The daemon never stores a shell's environment, so after a restart each lane
needs one start or resume from a shell; resume uses this shell's environment
for every lane whose last request was start.

```text
devlanes lane resume [OPTIONS]
```

- `--all-at-once`: Start every selected service immediately, bypassing the machine-wide start gate for this run
- `--no-ui`: Print the starts as text instead of running them in the TUI

### devlanes debug

Explain failures: why a unit is not up, errors, events and resource use

```text
devlanes debug [OPTIONS] <COMMAND>
```

### devlanes debug why

Explain why a unit is not running or not ready, with a fix for each reason

```text
devlanes debug why [OPTIONS] [UNIT]
```

- `[UNIT]`: Unit to explain ([lane:]unit) [default: pick from a list]

### devlanes debug errors

List failed units, crash counts, recent error lines and port conflicts

```text
devlanes debug errors [OPTIONS]
```

- `--since <DURATION>`: Count crashes and error lines from this long ago, such as 5m or 2h [default: all recorded runs]

### devlanes debug events

Print the daemon's event stream: lane, unit, task and port events

```text
devlanes debug events [OPTIONS]
```

- `-f, --follow`: Keep streaming until interrupted [default: print recent events]

### devlanes debug top

Show CPU and memory per lane and unit

```text
devlanes debug top [OPTIONS]
```

### devlanes debug bundle

Write a redacted archive to attach to a bug report and print its path.

The archive holds the client and daemon versions, doctor, config check and status
as JSON, the last lines of the daemon's log and, with -- ARGS, the output of
devlanes -vv ARGS. Values of secret-looking variables, URL passwords and the home
directory are replaced before anything is written; read the files before sending
them anyway.

```text
devlanes debug bundle [OPTIONS] [-- <ARGS>...]
```

- `--to <PATH>`: Write the archive here [default: devlanes-debug-<time>.tar.gz in the temporary directory]
- `--log-lines <N>`: Lines of the daemon's log to include
  Default: `500`.
- `[ARGS]...`: devlanes arguments to run again with -vv and include, such as: -- start api

```text
Examples:
  devlanes debug bundle
  devlanes debug bundle --to bug.tar.gz -- start api
```

### devlanes env

Show, export or enter the environment of a lane or unit

```text
devlanes env [OPTIONS] <COMMAND>
```

### devlanes env show

Print the environment of the lane or of one unit as KEY=VALUE lines

```text
devlanes env show [OPTIONS] [UNIT]
```

- `[UNIT]`: Unit whose environment to print ([lane:]unit) [default: the lane's variables]
- `--explain`: Show where each variable comes from; secret values are hidden

### devlanes env export

Print export KEY=VALUE lines for eval in a shell

```text
devlanes env export [OPTIONS] [UNIT]
```

- `[UNIT]`: Unit whose environment to export ([lane:]unit) [default: the lane's variables]

### devlanes env shell

Open a subshell with the lane's or one unit's environment

```text
devlanes env shell [OPTIONS] [UNIT]
```

- `[UNIT]`: Unit whose environment to use ([lane:]unit) [default: the lane's variables]

### devlanes url

Print, open and list service URLs and routed hostnames

```text
devlanes url [OPTIONS] <COMMAND>
```

### devlanes url print

Print a service's URL, for scripts

```text
devlanes url print [OPTIONS] [UNIT]
```

- `[UNIT]`: Service whose URL to print ([lane:]unit) [default: pick from a list]

### devlanes url open

Open a service's URL in the browser; agent mode only prints it

```text
devlanes url open [OPTIONS] [UNIT]
```

- `[UNIT]`: Service to open ([lane:]unit) [default: pick from a list]

### devlanes url ls

List routed hostnames with their lane, unit, port and state

```text
devlanes url ls [OPTIONS]
```

### devlanes infra

Start, stop and inspect the project's containers

```text
devlanes infra [OPTIONS] <COMMAND>
```

### devlanes infra up

Start shared and per-lane containers and wait until they are healthy

```text
devlanes infra up [OPTIONS]
```

### devlanes infra down

Stop the project's shared containers; their data is kept

```text
devlanes infra down [OPTIONS]
```

### devlanes infra status

Show the containers' health and ports

```text
devlanes infra status [OPTIONS]
```

### devlanes config

Validate devlanes.yaml

```text
devlanes config [OPTIONS] <COMMAND>
```

### devlanes config check

Validate this worktree's devlanes.yaml, including preset options

```text
devlanes config check [OPTIONS]
```

### devlanes certs

Trust the devlanes CA and install the /etc/hosts helper

```text
devlanes certs [OPTIONS] <COMMAND>
```

### devlanes certs trust

Trust the devlanes CA in the system and browser stores

```text
devlanes certs trust [OPTIONS]
```

- `--hosts`: Also install the /etc/hosts helper for programs that cannot resolve *.localhost
- `--resolver`: Also send lookups for hostnames.tld to the devlanes DNS server (/etc/resolver on macOS, systemd-resolved on Linux)

### devlanes certs untrust

Remove the devlanes CA from the trust stores

```text
devlanes certs untrust [OPTIONS]
```

- `--hosts`: Also remove the /etc/hosts helper, its sudo rule and the devlanes block in /etc/hosts
- `--resolver`: Also remove the OS resolver file for hostnames.tld
- `--keep-ca`: Keep the CA trusted; with --hosts or --resolver, remove only those

### devlanes remote

Connect to boxes over SSH and mirror their lanes here

```text
devlanes remote [OPTIONS] <COMMAND>
```

### devlanes remote connect

Connect to a box over SSH and mirror its lanes, ports and hostnames here

```text
devlanes remote connect [OPTIONS] <HOST>
```

- `<HOST>`: SSH destination, as in ~/.ssh/config
- `--background`: Return once connected instead of staying attached for prompts
- `--install`: If devlanes is missing on the box, install this machine's release there (HTTPS download, checksum verified, never replaces an existing binary)

### devlanes remote disconnect

Disconnect from a box and remove its mirrored ports and hostnames

```text
devlanes remote disconnect [OPTIONS] [HOST]
```

- `[HOST]`: Box to disconnect [default: every box]

### devlanes remote ls

List known boxes and their connection state

```text
devlanes remote ls [OPTIONS]
```

### devlanes remote sync

List the synced folders of running lanes

```text
devlanes remote sync [OPTIONS]
```

### devlanes daemon

Run, stop and install the background daemon

```text
devlanes daemon [OPTIONS] <COMMAND>
```

### devlanes daemon status

Show the daemon's version, pid, uptime and managed process count; never starts it

```text
devlanes daemon status [OPTIONS]
```

### devlanes daemon stop

Stop the daemon and every process it manages

```text
devlanes daemon stop [OPTIONS]
```

### devlanes daemon restart

Restart the daemon and start again, with this shell's environment, the services that ran; asks first when processes are running

```text
devlanes daemon restart [OPTIONS]
```

### devlanes daemon logs

Print the daemon's own log file

```text
devlanes daemon logs [OPTIONS]
```

- `-f, --follow`: Keep printing new lines
- `--tail <N>`: Lines to show [default: 100]

### devlanes daemon install

Install a login service so the daemon starts at login and outside agent sandboxes

```text
devlanes daemon install [OPTIONS]
```

### devlanes daemon uninstall

Remove the login service; a running daemon keeps running

```text
devlanes daemon uninstall [OPTIONS]
```

### devlanes daemon reset-state

Stop everything and empty the state database; lanes get new numbers and ports (needs --yes)

```text
devlanes daemon reset-state [OPTIONS]
```

### devlanes doctor

Check the daemon, Docker, proxy, CA, hosts file and output mode, each with a fix; never starts the daemon

```text
devlanes doctor [OPTIONS]
```

### devlanes completions

Print shell completions; lane and unit names come from the running daemon

```text
devlanes completions [OPTIONS] <SHELL>
```

- `<SHELL>`: Shell to generate completions for
  Values: `bash`, `elvish`, `fish`, `powershell`, `zsh`.
