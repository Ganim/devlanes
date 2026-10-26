# Service patterns

Configurations for the shapes most repositories have. Each one assumes the basics from [getting started](../getting-started.md): a service gets `PORT`, a lane gets `lane.domain`, hostnames go through the HTTPS proxy.

## Frontend and API

```yaml
services:
  api:
    run_script:
      file: apps/api/package.json
      name: dev
    hosts: "api.${{ lane.domain }}"
    ready:
      http:
        path: /healthz
    env:
      COOKIE_DOMAIN: ${{ lane.domain }}      # the app reads DEVLANES_ORIGINS for its CORS allow-list
  web:
    preset: vite
    run_script:
      file: apps/web/package.json
      name: dev
    hosts: "${{ lane.domain }}"
    env:
      VITE_API_URL: "${{ services.api.public_url }}"
```

The browser talks to `https://acme-2.localhost` and `https://api.acme-2.localhost`; both share the site `acme-2.localhost`, so a cookie with `Domain=acme-2.localhost` and `SameSite=Lax` works across them and never leaks into lane 3. `DEVLANES_ORIGINS` is in every service's environment and lists the https origins of the lane, so the API's CORS allow-list can be `process.env.DEVLANES_ORIGINS.split(',')` instead of a hardcoded list. It is not reachable as `${{ env.DEVLANES_ORIGINS }}`, since `env.*` reads your shell, not devlanes' own variables.

For server-to-server calls prefer `${{ services.api.url }}` (`http://127.0.0.1:<port>`, no TLS, no proxy hop). Use `public_url` when the value ends up in a browser, an OAuth redirect or a webhook.

The `http` check on `api` passes on any 2xx answer; with `status` set, only that code passes. Redirects are not followed, so a route that redirects to a login needs `status: 302`. The check sends `Host: 127.0.0.1:<port>`; an app that serves its health route only on its own hostname needs `host: ${{ self.hosts[0] }}` under `http`. Every check request sends `User-Agent: devlanes-ready`; match that header to keep the checks, retried every 150 ms until they pass, out of request logs, traces or metrics.

A frontend that proxies `/api` to the backend itself (Vite `server.proxy`, Next rewrites) points at `${{ services.api.url }}` and needs no CORS at all:

```yaml
  web:
    env:
      API_PROXY_TARGET: "${{ services.api.url }}"
```

## Workers and consumers

```yaml
services:
  queue-worker:
    run_script:
      file: apps/worker/package.json
      name: dev
    ready:
      log: "consumer connected"
    pty: false
    stop:
      signal: SIGINT
      timeout: 30s
    tags: [worker]
```

A worker has no port, so `ready` defaults to `started`, which reports a crashed-then-watching process as ready. Have it print one line when it is connected and wait for that line. `pty: false` gives it pipes; some loggers behave better without a terminal. `stop` sets the signal and grace period before the process tree is killed, for consumers that finish in-flight jobs on `SIGINT`. A worker waiting for jobs gets no requests, so its lane would freeze after `lanes.freeze_after`; `keep_awake: true` keeps the lane awake while the worker runs ([idle lanes](lifecycle.md#idle-lanes)).

Cron-like processes and one-shot jobs are tasks, not services: a service that exits shows as failed.

## WebSockets, SSE and long polling

Nothing to configure. Upgrades are bridged with no idle timeout and streamed responses are not buffered. Browsers open WebSockets over HTTP/1.1 on a separate connection, which the proxy accepts.

## gRPC and raw TCP

The proxy speaks HTTP/1.1 to upstreams, so gRPC over HTTP/2 does not go through a hostname. Point clients at the port:

```yaml
services:
  grpc-api:
    run: cargo run --bin grpc-api          # listens on PORT
  web-bff:
    env:
      GRPC_TARGET: "127.0.0.1:${{ services.grpc_api.port }}"
```

The same goes for databases, debuggers and anything that is not HTTP. On a connected box these ports are mirrored under the same number ([remote boxes](../remote.md#mirroring)).

## Several ports, several hostnames

```yaml
services:
  realtime:
    run: node server.mjs
    env:
      WS_PORT: ${{ self.ports.ws }}
      INSPECT: 127.0.0.1:${{ self.ports.inspect }}
    hosts:
      - rt.${{ lane.domain }}
      - host: "ws.${{ lane.domain }}"
        port: ws
```

`hosts` takes one hostname or route mapping, or a list of them: `hosts: "api.${{ lane.domain }}"` is the same as a one-item list. Each `self.ports.<name>` allocates one more port, stable per lane. Route hostnames to them with `hosts[].port`. The admin UI and the public API of one process can live on two hostnames this way, and `devlanes url ls` lists all of them.

## Wildcard hostnames

```yaml
services:
  tenants:
    run_script:
      file: apps/tenants/package.json
      name: dev
    hosts:
      - app.${{ lane.domain }}
      - "*.${{ lane.domain }}"
  gateway:
    hosts:
      - host: "*.${{ lane.domain }}"
        fallback: true  # only when nothing else matched
```

Exact hostnames win over wildcards; a `fallback` wildcard loses to every other route. `*.acme-2.localhost` covers `foo.acme-2.localhost` and `a.b.acme-2.localhost`, not `acme-2.localhost` itself. For a second-level product domain use a lane var: `pages_domain: ${{ 'pages' if lane.index == 0 else 'pages-' ~ lane.index }}.localhost` and `"*.${{ lane.pages_domain }}"`. The var must end in `.localhost`, or in your `hostnames.tld` with the resolver strategy.

Quote wildcard entries: an unquoted leading `*` is a YAML alias. `/etc/hosts` takes only exact names, so programs that cannot resolve `*.localhost` reach wildcard hostnames only through the resolver strategy, and only under its `hostnames.tld`; a lane var hardcoded to `.localhost` stays out of their reach. See [Hostnames and HTTPS](https.md#programs-that-cannot-resolve-localhost).

## Replicas

```yaml
services:
  worker:
    run: node worker.mjs
    replicas: 3
    env:
      WORKER_ID: "${{ replica.index }}"
```

Each replica is a unit `worker.1`, `worker.2`, `worker.3` with its own `PORT`. `services.worker.url` is replica 1. Named ports are allocated per replica too.

## Services that start on demand

`autostart: false` is covered in [lane lifecycle](lifecycle.md#services-that-start-on-demand).

## One lane's frontend against another lane's backend

```yaml
profiles:
  attach:
    include: [+web]
    infra: false
    env:
      API_URL: ${{ lanes.main.services.api.url }}
      VITE_API_URL: ${{ lanes.main.services.api.public_url }}
```

`devlanes start --profile attach` in a feature worktree runs only the frontend and points it at lane 0's API, skipping containers and setup. `lanes.main.services.*` resolves against lane 0 of the same project; `projects.<name>.services.*` reaches another devlanes project that is running.

## Lifecycle tasks

Installs, migrations and seeds that run at lane events are covered in [lane lifecycle](lifecycle.md#lifecycle-tasks).

## Tags, profiles and templates

```yaml
tags:
  api:
    color: cyan
templates:
  fastify-api:
    platform: node
    tags: [api]
    ready:
      http:
        path: /-/health
services:
  catalog:
    extends: fastify-api
    run_script:
      file: apps/catalog/package.json
      name: dev
    hosts: "catalog.${{ lane.domain }}"
    tags: [api, essential]
  crm:
    extends: fastify-api
    run_script:
      file: apps/crm/package.json
      name: dev
    hosts: "crm.${{ lane.domain }}"
    tags: [api, essential]
profiles:
  default: [+essential]
  backend: [+api, +worker]
  everything:
    include: ["*"]
    exclude: [+desktop]
  e2e:
    include: [+api, +web]
    run_scripts:
      dev: start
    env:
      NODE_ENV: production
```

A unit's colour comes from its first tag with a `color` under `tags`, else its first category tag (`api`, `web`, `worker`, `db`), so put the category tag first and selection tags after. Profiles select by name, glob or `+tag`; with a `default` profile, a lane's first plain `devlanes start` starts exactly that selection. `--profile` repeats and the union is started.

After that, a plain `devlanes start` brings back what the lane ran instead ([bringing a lane back](lifecycle.md#bringing-a-lane-back)).

## Monorepo with several apps

One `devlanes.yaml` at the repository root, one service per app, `run_script` pointing at each package's `package.json`. `cwd` is the script's directory automatically. Shared libraries that need building before the apps start are a `lane.start` task (`build-libs` in [lifecycle tasks](lifecycle.md#lifecycle-tasks)) or a watcher service tagged `watcher`.

If an app has its own `devlanes.yaml` in a subdirectory, that directory is the project root for devlanes when you run it there; a repository can hold several independent projects that way, each with its own name, lanes and ports.

## Agents and CI

```sh
devlanes exec --name smoke -- pnpm test:smoke          # an ad-hoc unit with the lane's env; logs, stop and attach work on it
devlanes wait api --timeout 2m
devlanes with --profile e2e --logs-dir devlanes-logs -- pnpm test:e2e
```

`with` starts the lane, waits for ready, runs the command with the lane's environment (`DEVLANES_PUBLIC_URL_API`, `DEVLANES_PUBLIC_URL_WEB`, …), writes each unit's log to `--logs-dir`, stops what it started and exits with the command's code. In GitHub Actions:

```yaml
- run: devlanes certs trust --yes
- run: devlanes with --profile e2e --logs-dir devlanes-logs -- pnpm test:e2e
- if: failure()
  uses: actions/upload-artifact@v4
  with:
    name: devlanes-logs
    path: devlanes-logs
```

A CI checkout is lane 0 with no worktrees. On Linux runners, add `chrome` to `tls.trust` so Playwright's browsers trust the CA, and expect the proxy on 8443 unless the runner may bind 443.
