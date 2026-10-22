# Writing devlanes.yaml

Read this before you create or change a `devlanes.yaml`. It covers each area with the options that matter and the mistakes agents make. Every field, with types and examples, is in [docs/reference/config.md](https://github.com/devlanes-run/devlanes/blob/main/docs/reference/config.md); [docs/examples/devlanes.yaml](https://github.com/devlanes-run/devlanes/blob/main/docs/examples/devlanes.yaml) shows them in context.

After every edit run `devlanes config check`. It validates expressions, references and preset options and points at the line of a mistake. Then `devlanes start` and `devlanes status`.

## File layout

`devlanes.yaml` sits at the repository root (or a package root in a monorepo) and is committed, so every worktree has it. Start it with the schema modeline so editors complete fields:

```yaml
# yaml-language-server: $schema=https://devlanes.run/v1/schema.json
project: acme
```

`project` is lowercase letters, digits, dots, underscores and dashes, shared by every worktree. The other top-level keys are `lanes`, `infra`, `global_env`, `fnox`, `tags`, `templates`, `tasks`, `services` and `profiles`.

## Services

A service is a long-running process started with the lane. Exactly one of `run` or `run_script` is required, directly or through `extends`.

```yaml
services:
  api:
    run_script: apps/api/package.json#dev     # the project's package manager runs the script
    hosts: ["api.${{ lane.domain }}"]
    ready: { http: { path: /-/health } }
  worker:
    run: [node, worker.mjs]                   # a list is an argv, run without a shell
    cwd: apps/worker
    ready: { log: "consumer connected" }
  docs:
    run: >-                                   # a string runs with /bin/sh -c
      pnpm -F @acme/docs dev --port ${{ self.port }} --host 127.0.0.1
    hosts: ["docs.${{ lane.domain }}"]
```

- `run_script: <file>#<name>` works with `package.json` (npm, pnpm, yarn or bun, from `packageManager` or the lockfile), `Makefile`, `justfile`, `deno.json`, `pyproject.toml` (uv or poetry) and `Cargo.toml` (a bin), and runs in that file's directory. Prefer it to `run: pnpm -F pkg dev`: presets place their flags correctly and profiles can remap the script name.
- A multi-line `run` string is a shell script. Without `cwd`, `run` starts at the lane's worktree root.
- `run` strings and `ready.cmd` are POSIX `sh` on every OS: on Windows devlanes runs them with Git for Windows' `sh.exe` (`cmd.exe` when it is missing, with a `shell_fallback` warning from `devlanes config check`). Don't write PowerShell or cmd syntax in them. A list `run` is an argv and needs no shell.
- `autostart: false` keeps a service out of a plain `devlanes start` when no `default` profile exists; naming the service or its tag still starts it.
- `replicas: N` runs N instances (`replica.index` counts from 1), each with its own port.
- `stop: { signal: SIGINT, timeout: 15s }` replaces the default SIGTERM and 10 s grace period before the process tree is killed.
- `pty: false` uses pipes instead of a terminal. On Windows the terminal is a ConPTY pseudo console, which rewrites the output stream (line endings, cursor and colour sequences), so set `pty: false` when the logs must hold exactly what the process wrote. `hidden: true` keeps the port off machines connected with `devlanes remote connect`.
- `name: API Admin` sets the label shown in the TUI and status, where the installed devlanes supports it (`devlanes config check` reports an unknown key otherwise). Keep the key short and dash-separated; the key is what commands and expressions use.

devlanes has no dependencies between services and never restarts a process that exits. Order comes from `tasks` with `when` and from infra; a crash shows as `failed` with its log tail.

## Readiness

`devlanes start` returns once every started service is ready or one failed. Without `ready`, a service with `hosts` is ready when its port accepts TCP, and one without hosts as soon as it starts.

Give every long-running server a check that fails when the app is broken. Dev servers under a watcher (`tsx --watch`, `nodemon`, `cargo watch`, `uvicorn --reload`) keep running after the program crashes and wait for the next file change, so `ready: started` reports them ready while nothing works. The usual gap is a worker without a port: have it print a line once it is connected and wait for that line.

```yaml
ready: { http: { path: /-/health, status: 200, timeout: 60s } }   # best when the app has a health route
ready: { tcp: true }                                              # the service's own port is open
ready: { tcp: inspector }                                         # a named port
ready: { log: "consumer connected" }                              # regex on output, escape codes removed
ready: { cmd: "pg_isready -h 127.0.0.1 -p ${{ infra.postgres.port }}" }
ready: started                                                    # only when there is nothing to probe
```

Use exactly one of `tcp`, `http`, `log` or `cmd`. `timeout` bounds the wait; make it generous for services that compile first. A health route that checks the database beats `tcp`, which only proves the socket is open.

## Ports

Every lane gets a block of ports and every service one port from it: `PORT` in its environment, `${{ self.port }}` in expressions, with `HOST=127.0.0.1`. The service must listen on that port.

- Pass the port with a preset (`preset: vite`), with `--port ${{ self.port }}` in `run`, or let the app read `PORT`.
- Never hardcode a port. A literal `env: { PORT: 3000 }` is the same in every lane, so two lanes collide. `env: { PORT: "${{ lane.port + 5 }}" }` pins an offset inside the lane's block.
- Never write `env: { PORT: "${{ self.port }}" }`. `self.port` is computed from `PORT`, devlanes rejects it as a cycle, and `PORT` already holds that value.
- Never put `PORT` in `global_env`: it overrides every service's own port.
- A second port: reference `${{ self.ports.debug }}` anywhere in the service and devlanes allocates a port named `debug`.
- Other services: `${{ svc.api.port }}`, `${{ svc.api.url }}` (`http://127.0.0.1:<port>`), `${{ svc.api.public_url }}` (its `https://` hostname). Write dashes as underscores: `svc.crm_api`. `devlanes with` also gives commands `DEVLANES_PUBLIC_URL_<SERVICE>`.

## Hostnames and HTTPS

```yaml
hosts:
  - api.${{ lane.domain }}
  - host: "*.${{ lane.domain }}"      # wildcard, used only when no other host matches
    fallback: true
  - host: inspect.${{ lane.domain }}  # routes to a named port instead of the main one
    port: inspector
  - host: funnels.${{ lane.domain }}  # sends the upstream address as Host
    host_header: upstream
```

`lane.domain` is `<project>.localhost` in lane 0 and `<project>-<N>.localhost` in lane N. Keep every hostname under it, or under a lane variable built the same way, so lanes never collide. The proxy serves them over HTTPS on 443 (8443 when 443 cannot be bound). Hosts also make `ready` default to `tcp`.

TLS trust is already handled. Node units get `NODE_EXTRA_CA_CERTS` and Python units `SSL_CERT_FILE` pointing at the devlanes CA, and the user runs `devlanes certs trust` once for browsers and the system. Do not add `NODE_OPTIONS=--use-system-ca`, `NODE_TLS_REJECT_UNAUTHORIZED=0` or your own `NODE_EXTRA_CA_CERTS`. `lanes.hosts_file: always` also writes exact hostnames to `/etc/hosts` for programs that cannot resolve `*.localhost`; wildcards cannot go there.

## Environment and expressions

Values may contain `${{ ... }}` (MiniJinja), evaluated per lane. Undefined names are errors; use `${{ env.X | default('') }}` for optional ones. A value that is one whole expression keeps its type. Write `${{ '${{' }}` for a literal `${{`.

| Namespace | Contents |
| --- | --- |
| `project` | `name`, `primary_path` (the primary worktree, for files only it has) |
| `lane` | `index`, `name`, `label`, `path`, `domain`, `port` (block start), plus `lanes.vars` |
| `self`, `svc.<name>` | `name`, `port`, `ports.<name>`, `url`, `public_url`, `hosts` |
| `infra.<container>` | `port` (primary published host port), `ports.<name or container port>` |
| `env` | the shell that ran devlanes, then fnox secrets, then the lane's `.env` |
| `replica` | `index`, `count` |
| `lanes.main.svc.<name>` | services of lane 0 |
| `projects.<project>.svc.<name>` | services of another devlanes project |

Layers, lowest first: the client's environment, contributors (mise, direnv, Node, Python), secrets, devlanes' variables (`PORT`, `HOST`, `DEVLANES_PROJECT`, `DEVLANES_LANE`, `DEVLANES_SERVICE`, `DEVLANES_PUBLIC_URL`), `global_env`, the service's `env`, the profile's `env`, `--env` on the command line.

`.env` is never injected into processes; it only feeds `${{ env.X }}`. Name what a process needs (`DB_PASSWORD: ${{ env.DB_PASSWORD }}`) or let the app load its own dotenv. mise `[env]` and an allowed `.envrc` are applied automatically, so do not copy them. Secrets belong in fnox (`fnox: { profile: dev }`, unlocked with `devlanes secrets unlock`), never as literals.

## Lanes

```yaml
lanes:
  primary: main                        # branch of lane 0
  worktree_dir: ../${{ project.name }}-lanes
  max: 6
  vars:
    db_name: acme_${{ lane.name }}
    redis_db: ${{ lane.index }}
```

`vars` are per-lane values read as `lane.<name>`: database names, vhosts, buckets, Redis DB numbers. A var named `domain` replaces the default domain and `port` fixes the block start; set them only when the default is wrong. `new_worktrees` (`register`, `setup`, `start`) and `removed_worktrees` (`stop`, `clean`) default to `register` and `stop`; leave them out unless changing them.

## Infra

Containers start before setup, tasks and services, and are healthy first. Compose services are shared by every lane unless listed in `per_lane`.

```yaml
infra:
  compose: docker-compose.yml          # or a list, merged like repeated -f
  exclude: [caddy]                     # never started; the devlanes proxy replaces local reverse proxies
  per_lane: [mailpit]
  env: { ACME_DB_PORT: 15432 }         # Compose interpolation only; processes do not see it
  hosts: { s3mock: ["s3.${{ lane.domain }}"] }
  containers:                          # plain Docker containers without Compose
    cerbos:
      image: ghcr.io/cerbos/cerbos:0.40.0
      ports: { http: 3592 }
      per_lane: true
```

Processes reach a container through `${{ infra.postgres.port }}`. Keep databases, brokers and object stores in infra, never as services. A shared container keeps one copy of the data, so give each lane its own database, vhost or bucket with `setup` and `lanes.vars`.

## Setup and teardown

Named commands run once per lane after its containers are healthy, recorded as done and skipped on later starts. `teardown` runs on `devlanes lane clean`.

```yaml
infra:
  setup:
    database: psql "postgres://postgres:postgres@127.0.0.1:${{ infra.postgres.port }}/postgres" -c "CREATE DATABASE ${{ lane.db_name }}"
    vhost:
      run: docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }}
      container: rabbitmq                # run again whenever this container restarts
      ignore_errors: true
      timeout: 60s
  teardown:
    database: psql "postgres://postgres:postgres@127.0.0.1:${{ infra.postgres.port }}/postgres" -c "DROP DATABASE IF EXISTS ${{ lane.db_name }} WITH (FORCE)"
```

## Tasks

One-shot commands with `run`, `run_script` or `steps`, plus `cwd`, `env`, `timeout` and `fnox`.

```yaml
tasks:
  install:  { run: [pnpm, install, --frozen-lockfile], when: lane.create, timeout: 10m }
  migrate:  { run_script: package.json#db-deploy, when: lane.start }
  seed:     { run_script: packages/seed/package.json#start, when: lane.create, after: services }
  reset-db:
    steps: [infra.teardown.database, infra.setup.database, migrate, seed]
    confirm: Drops ${{ lane.db_name }} and re-seeds it
  login:    { run: pnpm prisma login, interactive: true }
```

- `when: lane.create` runs until it succeeds once for the lane (installs, codegen, a first seed). `lane.start` runs on every start before services (migrations). `lane.stop` and `lane.clean` run on those events. Without `when`, a task runs only through `devlanes run NAME`.
- `after: services` delays a create or start task until the selected services are ready.
- `profiles: [default, backend]` limits an automatic task to those profiles.
- Put `confirm` on anything destructive; scripts and agents then have to pass `--yes`.
- `interactive: true` for commands that need the user's terminal, including TUIs such as `htop`; it cannot have `when`.
- A command that ends by itself is a task. As a service it would show as failed when it exits.

## Tags, colors and profiles

```yaml
tags:
  api: { color: cyan, description: HTTP APIs }
profiles:
  default: [+essential]                # used when no --profile is given
  backend: [+api, +worker]
  e2e:
    include: [+api, +web]
    run_scripts: { dev: start }        # services using ...#dev run ...#start
    env: { NODE_ENV: production }
  staging-web: { include: [+web], infra: false }
```

A unit's color comes from its first tag: a color set under `tags`, else the tag's category (`api`, `server`, `gateway`: blue; `web`, `app`, `ui`: green; `worker`, `queue`, `cron`: magenta; `db`, `redis`: yellow; `docs`: cyan; `admin`: white; `stub`, `mock`: gray), else the name. Put the category tag first and selection tags such as `essential` after it.

Profiles select services by name, glob or `+tag`. Globs skip `autostart: false` services; names and tags include them. When a `default` profile exists, a plain `devlanes start` starts exactly its selection and `autostart` stops mattering outside globs, so use one of the two mechanisms, not both.

## Templates and extends

```yaml
templates:
  api:
    tags: [api]
    ready: { http: { path: /-/health } }
services:
  crm:
    extends: api
    run_script: apps/crm/package.json#dev
    hosts: ["crm.${{ lane.domain }}"]
```

Templates resolve after local overrides. Mappings (`env`, `ready`, `stop`, preset options) merge by key; lists (`tags`, `hosts`) and scalars replace. Setting `run` or `run_script`, or one `ready` check, drops the inherited alternative.

## Presets and platforms

`platform` (node, bun, deno, python, rust) is detected from the run_script source or the working directory; set it only when detection is wrong. It adds CA and color variables.

| Preset | Adds | Options |
| --- | --- | --- |
| `vite` | `--port <port> --strictPort --host 127.0.0.1` | `vite: { args }` |
| `next` | `--port <port> --hostname 127.0.0.1` | `next: { args }` |
| `wrangler` | `--port`, `--inspector-port` (a named `inspector` port), `--var` per var, a per-lane `WRANGLER_REGISTRY_PATH` | `wrangler: { vars, args, isolate_registry }` |
| `tauri` | a per-lane, per-replica identifier and dev URL | `tauri: { identifier, dev_url, product_name, before_dev_command }` |

With `run_script`, a preset's flags are passed to the script, so the framework command must be the script's last command. The wrangler preset refuses a script that already passes `--port` or `--inspector-port`. Without a preset, pass the flags yourself with `${{ self.port }}`. Do not set `WRANGLER_REGISTRY_PATH`: devlanes sets it per lane for every unit of a project with a wrangler config.

## Sync

```yaml
sync:
  - from: apps/extension/.output/chrome-mv3   # copied under ~/devlanes/sync/<project>/<lane>/ by default
    ignore: ["*.map"]
```

For build output another program loads from a fixed path, such as an unpacked browser extension, locally or on a machine connected with `devlanes remote connect`.

## Local overrides and user config

`devlanes.local.yaml` next to `devlanes.yaml` holds personal changes and stays out of Git. It merges by key before templates, for example `services: { web: { autostart: false } }`.

Machine settings live in `~/.devlanes/config.yaml` (or `$DEVLANES_HOME/config.yaml`): `ports.range`, `proxy` ports and LAN access, `startup` (how many services start at once), `tls` (devlanes or mkcert CA, trust stores), `hostnames`, `terminal`, log and history retention, `updates.check`. They never go in the project file.

## Checklist

- Every service has `run_script` or `run`, listens on its own port, keeps its hosts under `lane.domain` and has a `ready` check that fails when the app is broken.
- No literal ports, no `PORT` in `global_env`, no CA or TLS workarounds, no secrets or `.env` values in the file.
- One-shot work is a task with the right `when`; destructive tasks have `confirm`.
- Databases and brokers are infra, with per-lane namespaces from `setup` and `lanes.vars`.
- Defaults are left out: a detected `platform`, default lane policies, a `lanes.vars.domain` equal to the built-in one.
- `devlanes config check` passes and `devlanes start` reaches ready.
