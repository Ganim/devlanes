# Migrating to devlanes

Read this when the user asks to move an existing setup (mprocs, dekit, process-compose, a Procfile runner, pm2, tmuxinator, tmuxp, `concurrently` or `npm-run-all` scripts, mise tasks, Docker Compose) to devlanes. Write the result with [configuration.md](configuration.md); this file says how each source maps.

## Procedure

1. Find every setup in the repository root: the file names are listed in each section below. Several setups merge into one `devlanes.yaml`; when two define the same process differently, ask the user which one wins.
2. Read each command to its end. Open the `package.json` scripts, Makefile targets and entrypoints the setup runs: they tell you which port a process listens on, whether it has a health route, what it logs once it is up, and whether it ends by itself.
3. Write `devlanes.yaml` with the rules below. Keep the old files; delete them only when the user asks.
4. Run `devlanes config check`, then `devlanes start`, and fix what fails (`devlanes debug errors`, `devlanes logs UNIT --tail 200`). Check `devlanes status` shows each service ready on its hostname.
5. Tell the user what did not carry over (the "not mapped" rows that apply) and what they need to do: `devlanes certs trust` once, secrets into fnox, a ready log line to add to a worker.

## Rules for every source

Services and tasks. A process that keeps running is a service. A command that ends by itself (install, build, codegen, migrate, seed, reset, a test run, `docker compose up -d`) is a task: `when: lane.create` when it runs once per checkout, `when: lane.start` when it runs before every start, no `when` when the user runs it on demand. Add `after: services` when it needs a running service, and `confirm` when it destroys data. A source that marks processes as one-shot says so directly: dekit `type: job`, process-compose `process_completed` dependencies, overmind `OVERMIND_CAN_DIE`, the Heroku `release` type.

Containers are infra. Processes that run `docker compose up`, `docker run`, a local database or a broker become `infra.compose` or `infra.containers`, with per-lane databases, vhosts and buckets created in `infra.setup` from `lanes.vars`. A local reverse proxy (Caddy, nginx, Traefik) goes in `infra.exclude`: the devlanes proxy serves `hosts` over HTTPS.

Ports. Each lane gives each service its own port in `PORT` and `${{ self.port }}`. Replace a literal port in a command (`--port 3000`, `-p 3000`, `PORT=3000`, `localhost:3000`) with `${{ self.port }}`, or use a preset that adds the flag. Drop a literal `PORT` from env. Never write `env: { PORT: "${{ self.port }}" }`: devlanes rejects it as a cycle and `PORT` already has that value. Where another process reached this one at `localhost:3000`, use `${{ svc.<name>.port }}`, `${{ svc.<name>.url }}` or `${{ svc.<name>.public_url }}`. Pass an app that reads its port from its own variable `MY_PORT: ${{ self.port }}`. When a command has several literal ports, find out which one the process listens on before replacing anything.

Readiness. Give every service a `ready` check that fails when the app is broken: `http` on a health route, `tcp: true` for a plain server, `log` with the line a worker prints once connected. Under a watcher (`tsx --watch`, `nodemon`) a crashed process keeps running, so `ready: started` would report it ready. If a worker prints nothing usable, tell the user which line to add. Translate the source's own checks: a substring match becomes an escaped `ready.log` regex, a probe on the service's own port becomes `tcp: true` or `http: { path }`, a command becomes `ready.cmd`.

Names. Unit keys are lowercase letters, digits, `-` and `_` (`API Server` becomes `api-server`). Keep the source's label as the display name with `name:` where devlanes supports it. Put the category tag first (`api`, `web`, `worker`, `queue`, `stub`), so colors follow, and selection tags such as `essential` after it.

What is gone. devlanes never restarts a process, has no dependencies between services and stops everything with a signal. Drop autorestart, restart backoff, watch-and-restart and stop commands; mention them to the user. Drop CA and TLS workarounds (`NODE_OPTIONS=--use-system-ca`, `NODE_TLS_REJECT_UNAUTHORIZED`, a hand-set `NODE_EXTRA_CA_CERTS`) and `WRANGLER_REGISTRY_PATH`: devlanes sets the CA for Node and Python units and the registry path itself. Drop TUI settings (keymaps, scrollback, layouts) and log-file settings; devlanes keeps every unit's output.

`.env`. Never copy `.env` values into `devlanes.yaml`; they are often secrets. Tools such as foreman, honcho and process-compose load `.env` into every process, while devlanes reads it only for `${{ env.NAME }}` expressions. Either the app loads its own dotenv (keep that), or name each variable it needs: `DB_PASSWORD: ${{ env.DB_PASSWORD }}`. Secrets the team shares belong in fnox. Variables from mise `[env]` and an allowed `.envrc` already reach every unit.

Profiles. A source's "start these by default" set becomes `profiles.default` (usually `[+essential]` with an `essential` tag on those services). Named groups the user switched between become more profiles. Use either `default` or per-service `autostart: false`, not both.

## mprocs

`mprocs.yaml`, else `mprocs.json`. The global `~/.config/mprocs/mprocs.yaml` holds UI settings only.

| mprocs | devlanes |
| --- | --- |
| `procs.<name>: "cmd"` or `shell: cmd` | `run: cmd` (both use `sh -c`) |
| `procs.<name>: [..]` or `cmd: [..]` | `run: [..]`; `[pnpm, -F, pkg, dev]` is better as `run_script: <pkg dir>/package.json#dev` |
| proc name | unit key (lowercase, dashes) plus `name:` with the original label |
| `cwd` (`<CONFIG_DIR>` prefix) | `cwd:` relative to the project |
| `env` | `env:`; a `null` value cannot be expressed (devlanes cannot unset a variable) |
| top-level `env` | `global_env:` |
| `add_path` | `env: { PATH: "<dir>:${{ env.PATH }}" }`; this replaces the PATH devlanes builds with mise tools, so prefer mise or direnv |
| `autostart: true` / `false` | a tag in `profiles.default`, or `autostart: false` |
| `autorestart` | not mapped |
| `stop: SIGINT` / `SIGTERM` / `SIGKILL` / `hard-kill` | `stop.signal` (`hard-kill` is `SIGKILL`) |
| `stop: {send-keys: ["<C-c>"]}` | `stop.signal: SIGINT`; other keys are not mapped |
| `stop: {cmd}` | not mapped |
| `deps` | not mapped; use infra and `when` tasks for order |
| `log` | not mapped |
| `$select: os` | the macOS or Linux value |
| `keymap_*`, `scrollback`, `hide_keymap_window`, `proc_list_width`, `server`, `on_init`, `on_all_finished` | not mapped |

Procs that run once (seed, migrate, build, reset) become tasks; procs that run `docker compose up/down` become `infra`; a TUI such as `htop` becomes an `interactive: true` task or goes away (`devlanes debug top` shows CPU and memory per unit).

Before:

```yaml
env:
  NODE_OPTIONS: --use-system-ca
procs:
  'API Admin':
    cmd: [pnpm, --silent, -F, '@acme/admin-api', dev]
  'WRK Mailer':
    shell: pnpm -F @acme/mailer dev
    autostart: false
  'Migrate DB':
    cmd: [pnpm, -F, '@acme/models', db-deploy]
    autostart: false
  'Docker Start':
    cmd: [docker, compose, up, -d, --wait]
```

After:

```yaml
project: acme
infra:
  compose: docker-compose.yml
tasks:
  migrate-db:
    run_script: packages/models/package.json#db-deploy
    when: lane.start
services:
  api-admin:
    run_script: servers/admin/package.json#dev
    hosts: ["admin-api.${{ lane.domain }}"]
    ready: { http: { path: /-/health } }
    tags: [api, essential]
  wrk-mailer:
    run_script: workers/mailer/package.json#dev
    ready: { log: "mailer ready" }   # a line the worker prints once connected
    tags: [worker]
profiles:
  default: [+essential]
```

## dekit

`dekit.yaml` plus the fragments its `load` lists (paths or globs, with `at` mounting them under a path).

| dekit | devlanes |
| --- | --- |
| `tasks.<path>` | service, or a task for `type: job`; `web/dev` becomes `web-dev` |
| `cmd` (argv or line) | `run:` argv or shell text (dekit forbids shell syntax in a line, so `sh` reads it the same) |
| `script` (JavaScript on dekit's runtime) | not mapped; ask the user for an equivalent command |
| `autostart: true` | in `profiles.default`, together with every service and job it depends on |
| job that runs with the stack | task with `when: lane.start`, plus `after: services` when it needs a service |
| `deps` on a service | not mapped; when a service depends on a job that needs a service, move that service to infra so the job can run first |
| `tags` | `tags:` without `autostart` and `dynamic` |
| `label` | `name:` |
| `cwd`, `env`, `add_path` | as for mprocs; paths are relative to the declaring file |
| `ready.log` | `ready.log:` with regex characters escaped |
| `ready.tcp: <own port>` / `ready.http: <own URL>` | `tcp: true` / `http: { path }` |
| `ready.cmd` | `ready.cmd:` |
| `ready.file` | `ready.cmd: test -e <file>` |
| `ready.timeout` | `ready.timeout:`; `interval` is not mapped |
| `stop: <signal>`, `stop.signal`, `stop.timeout` | `stop.signal`, `stop.timeout` |
| `stop.keys: ["<C-c>"]` | `stop.signal: SIGINT` |
| `stop.cmd`, `stop.group: false`, `autorestart` | not mapped |
| `defaults` | a template every service `extends`, or the values written into each unit |
| `on_init`, `on_idle`, `kernel`, `log`, `tui`, `keymap`, `scrollback_len`, `mouse_scroll_speed` | not mapped |

Before:

```yaml
tasks:
  db:
    cmd: ["postgres", "-D", ".data/db"]
    ready: {log: "ready to accept connections"}
  migrate:
    type: job
    cmd: ["./migrate.sh"]
    deps: [db]
  api:
    cmd: ["cargo", "run", "-p", "api"]
    deps: [db, migrate]
    ready: {http: "http://localhost:3000/health"}
    autostart: true
```

After (Postgres moves to infra, so `migrate` runs before services):

```yaml
project: acme
infra:
  compose: docker-compose.yml        # a postgres service
tasks:
  migrate:
    run: ./migrate.sh
    when: lane.start
services:
  api:
    run: cargo run -p api -- --port ${{ self.port }}   # the app must listen on the port it is given
    hosts: ["api.${{ lane.domain }}"]
    ready: { http: { path: /health } }
```

## process-compose

The first of `compose.yml`, `compose.yaml`, `process-compose.yml`, `process-compose.yaml` that has `processes:`, merged with its `*.override.yml` or `*.override.yaml`.

| process-compose | devlanes |
| --- | --- |
| `processes.<name>` | service |
| process others wait for with `process_completed(_successfully)` | task with `when: lane.start` |
| `is_foreground: true` | task with `interactive: true` |
| `availability.exit_on_end: true` | on-demand task, or the command of `devlanes with -- COMMAND` |
| `command` | `run:` shell text (`$$` becomes `$`) |
| `entrypoint` | `run:` argv with `command` as its last item |
| `working_dir` | `cwd:` |
| `environment` (list or map), top-level `environment` | `env:`, `global_env:`; `$VAR` becomes `${{ env.VAR }}`; envsubst functions such as `${VAR^^}` are not mapped |
| `disabled: true` | `autostart: false`, or left out of `profiles.default` |
| `replicas` | `replicas:`; `PC_REPLICA_NUM` counts from 0, `${{ replica.index }}` from 1 |
| `namespace` | `tags:` (without `default`) |
| `description` | `name:` or a comment |
| `ready_log_line` | `ready.log:` (already a regex) |
| `readiness_probe.exec.command` | `ready.cmd:` |
| `readiness_probe.http_get` on the own port | `ready: { http: { path, status } }` |
| `shutdown.signal` (number), `shutdown.timeout_seconds` | `stop.signal` (2 SIGINT, 15 SIGTERM, ...), `stop.timeout` |
| `shutdown.send_keys: '\x03'` | `stop.signal: SIGINT` |
| `depends_on` between services, `availability.restart`, `liveness_probe`, `is_daemon`, `shutdown.command`, `parent_only`, `env_file`, `log_location`, `vars` and Go templates, probe timings | not mapped |

Before:

```yaml
processes:
  migrate:
    command: ./scripts/migrate.sh
  api:
    command: go run ./cmd/api --port 8080
    working_dir: ./services/api
    depends_on:
      migrate: { condition: process_completed_successfully }
    readiness_probe:
      http_get: { host: 127.0.0.1, port: 8080, path: /health }
    availability: { restart: on_failure }
  web:
    command: npm run dev -- --port 5173
    working_dir: web
    environment: ["API_URL=http://localhost:8080"]
```

After:

```yaml
project: acme
tasks:
  migrate:
    run: ./scripts/migrate.sh
    when: lane.start
services:
  api:
    run: go run ./cmd/api --port ${{ self.port }}
    cwd: services/api
    hosts: ["api.${{ lane.domain }}"]
    ready: { http: { path: /health } }
  web:
    preset: vite
    run_script: web/package.json#dev
    env: { API_URL: "${{ svc.api.url }}" }
    hosts: ["${{ lane.domain }}"]
    ready: { tcp: true }
```

## Procfile (foreman, overmind, hivemind, honcho)

The Procfile named by `OVERMIND_PROCFILE` (in `.overmind.env` or `.env`), `procfile:` in `.foreman`, or honcho's `PROCFILE` in `.env`; else `Procfile.dev`; else `Procfile` (often the production one, so check with the user).

| Procfile runners | devlanes |
| --- | --- |
| `name: command` | service with `run: command`; `$PORT` keeps working because devlanes sets `PORT` |
| `$OVERMIND_PROCESS_<name>_PORT` | `${{ svc.<name>.port }}` |
| `OVERMIND_FORMATION` / `.foreman` `formation` (`web=2`) | `replicas: 2`; `=0` means left out of the default |
| `OVERMIND_PROCESSES`, `OVERMIND_IGNORED_PROCESSES` | `profiles.default`, or `autostart: false` on the others |
| `OVERMIND_CAN_DIE` | task with `when: lane.start` |
| `release:` | on-demand task (Heroku runs it on deploy) |
| `OVERMIND_PORT`, `OVERMIND_PORT_STEP`, `OVERMIND_NO_PORT`, `.foreman` `port`, `OVERMIND_AUTO_RESTART`, `OVERMIND_ANY_CAN_DIE` | not mapped |
| `.env` loaded into every process | see `.env` above |

Before (`Procfile.dev`, with `OVERMIND_CAN_DIE=assets_install` in `.overmind.env`):

```
web: bin/rails server -p $PORT
css: bin/rails tailwindcss:watch
worker: bundle exec sidekiq
assets_install: yarn install
```

After:

```yaml
project: acme
tasks:
  assets-install:
    run: yarn install
    when: lane.start
services:
  web:
    run: bin/rails server -p $PORT -b 127.0.0.1
    hosts: ["${{ lane.domain }}"]
    ready: { http: { path: /up } }
  css:
    run: bin/rails tailwindcss:watch
    ready: { log: "Done in" }
  worker:
    run: bundle exec sidekiq
    ready: { log: "Booted Rails" }
    tags: [worker]
```

## pm2

The first of `ecosystem.config.json`, `ecosystem.json`, `ecosystem.config.yaml/.yml`, `ecosystem.yaml/.yml`, `ecosystem.config.js/.cjs/.mjs`. Read a JavaScript file only for literal values; if it computes them, run `node -e "console.log(JSON.stringify(require('./ecosystem.config.js'), null, 2))"` and read the JSON.

| pm2 | devlanes |
| --- | --- |
| `apps[]` | service; `name` defaults to the script's file name |
| `script`, `args`, `interpreter`, `interpreter_args` / `node_args` | `run:` (pm2's interpreter by extension: `.js` node, `.ts` bun, `.py` python, `.sh` bash; `interpreter: none` runs the file); `script: npm, args: run dev` is better as `run_script` |
| `cwd`, `env` | `cwd:`, `env:` |
| `instances: N` | `replicas: N`; `max`, `0` and `-1` are not mapped |
| pm2's stop (SIGINT, then SIGKILL after `kill_timeout`) | `stop: { signal: SIGINT, timeout: <kill_timeout> }` |
| `exec_mode: cluster` | not mapped: each replica gets its own port, nothing shares one |
| `env_<name>` | a profile with `env` |
| `wait_ready`, `listen_timeout` | a `ready` check |
| `watch`, `autorestart`, `max_restarts`, `max_memory_restart`, `cron_restart`, `min_uptime`, `restart_delay`, `shutdown_with_message`, log files, `deploy` | not mapped |

Before:

```json
{
  "apps": [
    { "name": "api", "script": "./api/server.js", "args": "--port 4000",
      "env": { "NODE_ENV": "development" }, "env_production": { "NODE_ENV": "production" },
      "instances": 2, "kill_timeout": 3000 }
  ]
}
```

After:

```yaml
project: acme
services:
  api:
    run: node ./api/server.js --port ${{ self.port }}
    env: { NODE_ENV: development }
    replicas: 2
    stop: { signal: SIGINT, timeout: 3s }
    ready: { tcp: true }
profiles:
  production:
    include: [api]
    env: { NODE_ENV: production }
```

## tmuxinator and tmuxp

tmuxinator's project-local `.tmuxinator.yml`; tmuxp's `.tmuxp.yaml`, `.tmuxp.yml` or `.tmuxp.json`.

| tmuxinator / tmuxp | devlanes |
| --- | --- |
| pane that runs a program | service; its commands joined into one `run:` script, `source` written as `.` (devlanes uses `/bin/sh`) |
| pane with an editor, shell, pager or `git` | left out |
| window name, plus pane title or number when several panes run | unit key |
| `root` / `start_directory` (session, window, pane) | `cwd:` relative to the project (`~` expanded) |
| `pre_window` / `shell_command_before` | the first lines of each pane's script, or better `env:` and mise for tool versions |
| tmuxp `environment` | `global_env:` (session) or `env:` (window, pane) |
| `on_project_start`, `pre`, tmuxp `before_script` | task with `when: lane.start` |
| `on_project_first_start` | task with `when: lane.create` |
| `on_project_stop` | task with `when: lane.stop` |
| `on_project_restart`, `on_project_exit`, `post`, layouts, tmux options | not mapped |

Before (`.tmuxinator.yml`):

```yaml
root: ~/code/acme
on_project_first_start: bin/setup
windows:
  - editor: vim
  - server: bundle exec rails s -p 3000
  - frontend:
      root: ~/code/acme/web
      panes:
        - npm run dev
```

After:

```yaml
project: acme
tasks:
  setup:
    run: bin/setup
    when: lane.create
services:
  server:
    run: bundle exec rails s -p ${{ self.port }} -b 127.0.0.1
    hosts: ["${{ lane.domain }}"]
    ready: { tcp: true }
  frontend:
    preset: vite
    run_script: web/package.json#dev
    hosts: ["app.${{ lane.domain }}"]
    ready: { tcp: true }
```

## package.json with concurrently or npm-run-all

A root `package.json` script (usually `dev` or `start`) that runs `concurrently`, `npm-run-all --parallel` or `run-p`.

| package.json | devlanes |
| --- | --- |
| concurrently `npm:` / `pnpm:` / `yarn:` / `bun:` / `node:` shortcut, with `*` and `(!x)` | a service per matching script, `run_script: package.json#<script>` |
| concurrently `npm run <script>` (and the pnpm, yarn, bun forms) | `run_script:` |
| concurrently other commands | `run:`; a package binary needs `pnpm exec`, `npx`, `yarn run` or `bun x`, because `node_modules/.bin` is on PATH only inside scripts |
| concurrently `-n` / `--names` | unit keys |
| npm-run-all `--parallel` / run-p patterns (`*` within a `:` segment, `**` across) | a service per matching script |
| npm-run-all steps before the parallel group | tasks with `when: lane.start` |
| `"name -- args"` | `run: pnpm run name -- args` (with the project's package manager) |
| `-k`, `-r`, prefixes, colors, `--max-parallel` | not mapped |

Keep the original script for people who do not use devlanes.

Before:

```json
{ "scripts": {
    "dev": "concurrently -n api,web \"pnpm:dev:api\" \"vite --port 5173\"",
    "dev:api": "tsx watch src/server.ts --port 4000" } }
```

After:

```yaml
project: acme
services:
  api:
    run: pnpm exec tsx watch src/server.ts --port ${{ self.port }}
    hosts: ["api.${{ lane.domain }}"]
    ready: { http: { path: /health } }
  web:
    preset: vite
    run: pnpm exec vite
    env: { VITE_API_URL: "${{ svc.api.public_url }}" }
    hosts: ["${{ lane.domain }}"]
    ready: { tcp: true }
```

`dev:api` keeps its literal port, so it runs as its own command with `${{ self.port }}`; a script that reads `PORT` stays `run_script: package.json#dev:api`.

## mise tasks

`[tasks]` in `.config/mise/config.toml`, `.config/mise.toml`, `mise/config.toml`, `.mise/config.toml`, `mise.toml`, `.mise.toml`, and executable files in `mise-tasks/`, `.mise-tasks/`, `mise/tasks/`, `.mise/tasks/`, `.config/mise/tasks/`. `*.local.toml` files are personal.

mise does not mark long-running tasks. Make a task a service only when it clearly serves (`dev`, `serve`, `watch`); everything else becomes a devlanes task, on demand unless it set up the project. Running through devlanes gives a task the lane's ports and URLs.

| mise | devlanes |
| --- | --- |
| `name = "cmd"`, `run` | `run:`; several entries or lines get `set -e` first, as mise stops at the first failure |
| `run` entry `{ task = "x" }` | a `mise run x` line, or `steps:` |
| `file`, file tasks | `run: ./<path>`; `mise-tasks/db/seed.sh` is mise's `db:seed`, key `db-seed` |
| only `depends` | `steps:` |
| `depends` with `run` | `steps:` listing the dependencies and a task for the command |
| `dir` (`{{config_root}}/...`) | `cwd:` |
| `env`, `description`, `confirm` | `env:`, `name:` or a comment, `confirm:` |
| `[env]`, `[tools]` | nothing: devlanes applies the mise environment to every unit |
| `alias`, `hide`, `sources`, `outputs`, `shell`, `usage`, `run_windows`, Tera templates | not mapped; a shebang script in `run` moves to a file |

Before:

```toml
[tasks.build]
run = "vite build"
dir = "{{config_root}}/web"

[tasks.ci]
depends = ["lint", "test"]

[tasks.release]
confirm = "Cut a new release?"
file = "scripts/release.sh"
```

After:

```yaml
tasks:
  build:
    run: vite build
    cwd: web
  ci:
    steps: [lint, test]
  release:
    run: ./scripts/release.sh
    confirm: Cut a new release?
```

## Docker Compose

`compose.yaml`, `compose.yml`, `docker-compose.yaml`, `docker-compose.yml` (a file with `services:` and no `processes:`). Do not turn containers into devlanes services.

| Compose | devlanes |
| --- | --- |
| the file | `infra.compose:`, or a list with override files |
| services every lane can share (Postgres, Redis, RabbitMQ, S3 mocks) | shared by default; per-lane databases, vhosts and buckets through `infra.setup` and `lanes.vars` |
| services each lane needs its own copy of (mail catchers) | `infra.per_lane:` |
| a reverse proxy (Caddy, nginx, Traefik) | `infra.exclude:`; give the services `hosts` instead |
| an HTTP container users open in the browser | `infra.hosts: { s3mock: ["s3.${{ lane.domain }}"] }` |
| `${VAR}` in the file | `infra.env:` (Compose interpolation only) |
| `profiles:` | `infra.profiles:` |
| fixed host ports | keep them for shared containers; processes read the port from `${{ infra.<service>.port }}` |

```yaml
infra:
  compose: docker-compose.yml
  exclude: [caddy]
  per_lane: [mailpit]
  setup:
    database: docker compose exec -T postgres createdb -U postgres acme_${{ lane.name }}
  teardown:
    database: docker compose exec -T postgres dropdb -U postgres --if-exists acme_${{ lane.name }}
global_env:
  DATABASE_URL: postgres://postgres:postgres@127.0.0.1:${{ infra.postgres.port }}/acme_${{ lane.name }}
```
