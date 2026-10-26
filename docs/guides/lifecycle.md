# Lane lifecycle

## Start, stop and restart

`devlanes start` brings up the lane of the current worktree: infra, setup, `lane.create` and `lane.start` tasks, services, then `after: services` tasks. It waits until every started unit is ready or one fails. Units that already run are left alone, so starting twice is safe, and Ctrl-C detaches without stopping anything. `devlanes start api web` starts only those services and `devlanes start --profile e2e` the units of a profile.

`devlanes stop` without unit names runs the lane's `lane.stop` tasks and stops its processes; containers keep running ([infra](infra.md#lifecycle)). `devlanes stop api` stops one service. `devlanes restart api` stops a unit and starts it again with the environment of the shell that ran `restart`.

A service that exits shows as failed and stays that way until you start or restart it. devlanes never restarts a process by itself.

## Bringing a lane back

The first plain `devlanes start` of a lane starts its `default` profile, or every service with `autostart` left on when the project has no `default` profile. After that, a plain start brings back what the lane ran: the services you started (by profile or by name) and did not stop, with the same profiles. That is what makes a reboot or a daemon crash cost one command. On a lane that still runs, a plain start also starts what its profiles select and is missing, such as a service you just added. `devlanes stop` without unit names forgets what ran, so the next plain start uses the `default` profile again; stopping one service only leaves that service out. Set `start.restore: false` in `~/.devlanes/config.yaml` to always start the `default` profile.

The daemon never stores a shell's environment, so after a reboot, a crash or `devlanes daemon stop` each lane needs one start from a shell. A plain `devlanes start` brings back its own lane with the current shell's environment and names the other lanes that ran. `devlanes lane resume` uses the current shell's environment for every lane that was not stopped by hand and starts the services each one ran. `devlanes daemon restart` asks first when processes are running, then restarts the daemon and starts the services that ran again with the environment of the shell that ran it. `lane resume` and `daemon restart` bring services back even with `start.restore: false`.

## Services that start on demand

```yaml
services:
  storybook:
    run_script:
      file: packages/ui/package.json
      name: storybook
    hosts: "storybook.${{ lane.domain }}"
    autostart: false
  stripe-listen:
    run: stripe listen --forward-to ${{ services.api.url }}/webhooks/stripe
    autostart: false
```

`devlanes start storybook` or the TUI starts them; globs in profiles skip them, names and tags include them. The proxy answers their hostnames with a page that names the start command while they are stopped. `autostart` also takes an expression, such as `${{ lane.index == 0 }}`, evaluated per lane.

## Lifecycle tasks

```yaml
tasks:
  install:
    run: [pnpm, install, --frozen-lockfile]
    when: lane.create
    timeout: 10m
  codegen:
    run_script:
      file: package.json
      name: codegen
    when: lane.create
  migrate:
    run_script:
      file: apps/api/package.json
      name: db:migrate
    when: lane.start
  build-libs:
    run_script:
      file: package.json
      name: build:libs
    when: lane.start
    profiles: [default, e2e]
  seed:
    run_script:
      file: tools/seed/package.json
      name: start
    when: lane.create
    after: services
    timeout: 15m
  reset-db:
    steps: [infra.teardown.database, infra.setup.database, migrate, seed]
    confirm: Drops ${{ lane.db_name }} and re-seeds it
  prisma-login:
    run: pnpm prisma login
    interactive: true
```

Start order: infra, setup, `lane.create` tasks (until they succeed once per lane), `lane.start` tasks (every start, in file order), services together, `after: services` tasks. A failed task before services fails the start and prints its log tail. `devlanes run reset-db` runs a task by hand; `devlanes run --list` shows them. Tasks with `confirm` need `--yes` without a terminal, and `interactive` tasks get your terminal and never run from a hook.

`lane.stop` tasks run before the processes stop when the whole lane stops: `devlanes stop` without unit names, `devlanes daemon stop` and `daemon restart`. `lane.clean` tasks run on `devlanes lane clean`, after the processes stop and before `infra.teardown`.

New worktrees have no `node_modules`, so `install` with `when: lane.create` is what makes `git worktree add && devlanes start` work with nothing else.

## Idle lanes

A lane that runs services and has no activity for `lanes.throttle_after` (default `5m`) is throttled, and after `lanes.freeze_after` (default `15m`) it is frozen. Both apply to the whole lane: activity on any of its services thaws every service of the lane and its own containers. The `idle` state in `devlanes status` is a different thing and means a lane or service that runs nothing.

A throttled lane's processes run at background priority and keep serving requests. A frozen lane's processes are all suspended, and its own containers are paused with `docker pause` (or `docker compose pause` for `per_lane` Compose services). Shared infra containers are never paused. Memory stays allocated while a lane is frozen, and devlanes does not trim it on any OS. Ports, PIDs, routes and the lane's environment are kept. The kernel keeps accepting connections to a frozen server's ports, and they are answered after the thaw. A request through the proxy to a frozen lane waits up to 10 seconds for the thaw before it is forwarded.

Both settings live in the user config:

```yaml
# ~/.devlanes/config.yaml
lanes:
  throttle_after: 5m
  freeze_after: 30m
```

`0s` turns either one off. When both are set, `throttle_after` must be shorter than `freeze_after`. The daemon reads them when it starts, like the other `lanes.*` settings, so a change takes effect after `devlanes daemon restart`.

### Activity

Each of these resets the lane's timer, and thaws the lane when it is throttled or frozen:

- a request through the devlanes proxy to one of the lane's hostnames;
- bytes the client sends on an open WebSocket or other upgraded connection through the proxy;
- a new TCP connection to any of the lane's ports: service ports, literal ports and the ports of the lane's containers. This covers database clients, gRPC, direct `http://127.0.0.1:PORT` calls and the tunnels of a machine connected with `devlanes remote connect`;
- the lane's services together using more than 25% of one CPU core in a metrics sample;
- a devlanes command that needs the lane's processes: `start`, `stop`, `restart`, `run`, `exec`, `lane clean`, `infra up` and `debug thaw`;
- `devlanes attach` with a terminal you can type into, and typing into an attached terminal.

Commands that only read, such as `status`, `logs` (with or without `-f`), `url`, `env` and `wait`, do not count, and neither does the TUI showing a unit's output without you typing into it. `infra down` only touches shared containers and does not thaw a lane. New connections are checked every 5 seconds, and every half second while a lane is frozen, so a direct connection thaws a frozen lane within about half a second.

A lane is never throttled or frozen while a start, stop, clean or infra change runs on it, while a task, setup step or `exec` unit runs, while it has orphaned processes from a previous daemon, while it runs no service, or while a service with `keep_awake: true` runs.

### Keeping a lane awake

```yaml
services:
  queue-worker:
    run_script:
      file: apps/worker/package.json
      name: dev
    keep_awake: true
```

While a service with `keep_awake: true` runs, its whole lane is never throttled or frozen. Set it on workers, queue consumers, schedulers and tunnels such as `stripe listen`, which do their work without inbound requests and would stall in a frozen lane. It takes an expression like other boolean fields (`${{ lane.index == 0 }}` keeps only the main lane awake), and works in templates and in `devlanes.local.yaml`.

### macOS

Throttling sets every process of the lane to `PRIO_DARWIN_BG`: the lowest CPU priority, throttled disk and network I/O, and only the efficiency cores on Apple Silicon. Freezing sends `SIGSTOP` to each service's process group and to the descendants devlanes tracks that left the group, and thawing sends `SIGCONT`. macOS compresses or swaps out the memory of suspended processes on its own when memory runs short.

### Linux

Linux lanes are not throttled, because lowering a process's priority in a way that can be undone needs privileges or cgroups. A lane goes straight from running to frozen after `freeze_after`, and `throttle_after` has no effect. Freezing works as on macOS, with `SIGSTOP` and `SIGCONT`. The kernel swaps out a frozen lane's memory only when swap or zswap is configured.

### Windows

Throttling puts every process of the lane in efficiency mode, what Task Manager's "Efficiency mode" does: `IDLE_PRIORITY_CLASS` plus EcoQoS. Freezing calls `NtSuspendProcess` on every process in the service's job object, and thawing calls `NtResumeProcess`. devlanes does not trim the memory of a frozen lane; Windows trims the working sets of idle processes itself when memory is short. Windows containers that use process isolation cannot be paused, so in a lane that has them only the processes freeze.

### Status

`devlanes status` shows `frozen` or `throttled` in place of `ready` or `running` for each service of the lane, and the lane line says how long it has been that way, such as `frozen 12m`. JSON keeps `state: ready`, since a frozen service still answers once thawed, and adds `power` (`throttled` or `frozen`) to each unit and `power: {level, since_ms, last_activity_ms, last_activity}` to the lane. Agent output keeps `ready` as well and adds `power=frozen` or `power=throttled`.

In the TUI a frozen service shows a blue ❄ in place of its dot and a throttled one a dim `z` (`~` and `z` with ASCII glyphs), and the lane row says `frozen` or `throttled` after `@N`.

`devlanes debug freeze` freezes the current lane now, whatever the timers say, which is the quickest way to check that a project still works after a freeze. `devlanes debug thaw` resumes it. Both take a lane reference. Freeze refuses a lane that is not running, is busy, has a `keep_awake` service running or has orphaned processes.

### Stops, restarts and crashes

A thaw unpauses the lane's own containers before it resumes the services, so they find their database ready. If resuming fails, the lane stays marked frozen or throttled and the next activity tries again. A start or `infra up` also unpauses the lane's containers first, so a lane whose earlier unpause failed still starts.

Stopping, restarting or cleaning a frozen lane thaws it first, so stop signals and `lane.stop` tasks behave as usual. `devlanes daemon stop` and `daemon restart` thaw lanes before stopping them. If the daemon crashes while a lane is frozen, the next daemon resumes the processes left over, on macOS, Linux and Windows, and unpauses the lane's containers when it starts. `daemon.exit_when_idle` treats a frozen lane as running, so the daemon stays up to thaw it.

### After a thaw

File watchers catch up on changes made while the lane was frozen. On Linux, a large checkout during a freeze can overflow the inotify queue and a watcher may miss changes; restart the service if it looks stale. Timers that came due while the lane was frozen fire at once. The other side may have closed database and other connections a service held, and most connection pools open new ones. A process you stopped with Ctrl-Z in `devlanes attach` resumes along with the lane.

## Lanes that never freeze

Every byte a client sends on an open WebSocket counts as activity, so an open browser tab whose dev client sends messages keeps its lane awake. The Next.js dev client pings every few seconds and some HMR clients ping too; closing the tab lets the lane freeze. A dev server whose file watcher polls, or one that stays busy for another reason, can keep the lane above the CPU threshold. A running `keep_awake` service, orphaned processes from a previous daemon and a running task also keep a lane awake.

`devlanes debug events -f` shows `lane.idle` when a lane is throttled or frozen, `lane.thawed` with the kind of activity that thawed it (`proxy`, `connection`, `cpu`, `command`, or `stop` when a stop, restart or clean thawed it first), and `lane.idle_failed` when throttling or freezing failed and was undone. A thaw that failed shows as `lane.idle_failed` with level `normal` and code `thaw_failed`. `devlanes status --json` shows the lane's `power.last_activity` while it is throttled or frozen.

To turn both off:

```sh
devlanes config set lanes.freeze_after 0s
devlanes config set lanes.throttle_after 0s
devlanes daemon restart
```
