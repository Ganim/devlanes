# Languages

devlanes runs any process. What it adds per language is small: a `platform` contributor that detects the runtime from project files and sets a few variables, mostly so HTTPS calls to other lane hostnames trust the devlanes CA. Everything else is the usual dev command with the port taken from `PORT` or `${{ self.port }}`.

Set `platform` only when detection is wrong. Detection looks at the `run_script` file first, then the working directory.

| Platform | Detected from | Variables set |
| --- | --- | --- |
| `node` | `package.json` | `NODE_EXTRA_CA_CERTS`, `FORCE_COLOR=1` |
| `bun` | `packageManager: bun@…`, `bun.lock`, `bun.lockb`, `bunfig.toml` (walking up to the worktree) | `NODE_EXTRA_CA_CERTS`, `FORCE_COLOR=1` |
| `deno` | `deno.json`, `deno.jsonc` | `DENO_CERT`, `DENO_NO_UPDATE_CHECK=1`, `FORCE_COLOR=1` |
| `python` | `pyproject.toml`, `requirements.txt` | `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `PYTHONUNBUFFERED=1` |
| `rust` | `Cargo.toml` | `SSL_CERT_FILE`, `CARGO_TERM_COLOR=always` |

`FORCE_COLOR` and `CARGO_TERM_COLOR` are left alone when your shell already sets them or `NO_COLOR`. A `NODE_EXTRA_CA_CERTS` or `SSL_CERT_FILE` from your shell is replaced and `env show --explain` says so; put the value in the service's `env` if it must win.

Every unit also gets `DEVLANES_CA_CERT` (the CA) and `DEVLANES_CA_BUNDLE` (system roots plus the CA), for runtimes not listed here. Nothing changes `PATH`: tool managers are the job of mise or direnv (below).

## Node

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
```

The server must read `PORT` (and ideally `HOST`, which is `127.0.0.1`). With Fastify, Express, Hono or Koa that is `listen({ port: Number(process.env.PORT), host: process.env.HOST })`. If the script hardcodes a port, pass it: `run: node --watch server.mjs --port ${{ self.port }}`.

Node reads `NODE_EXTRA_CA_CERTS` once at startup, so `fetch('https://auth.acme-2.localhost')` from a service works without `NODE_TLS_REJECT_UNAUTHORIZED`. Drop any `--use-system-ca` or custom CA setup from the dev scripts; a script that overwrites `NODE_EXTRA_CA_CERTS` loses the devlanes CA.

A debugger port is another named port: `run: node --inspect=127.0.0.1:${{ self.ports.inspect }} server.mjs`. See [ports and environment](ports-and-env.md#several-ports-for-one-service).

Dev servers under a watcher (`tsx watch`, `nodemon`, `node --watch`) keep running after the program crashes, so give them a `ready` check that reaches the app, not `ready: started`.

## Bun

```yaml
services:
  api:
    run_script:  # bun run dev, when bun.lock or packageManager says bun
      file: package.json
      name: dev
```

With `Bun.serve({ port: Number(process.env.PORT), hostname: process.env.HOST })` nothing else is needed. Bun honours `NODE_EXTRA_CA_CERTS`.

The package manager and the platform are detected separately. `bunfig.toml` alone makes the platform `bun`, but the npm runner still needs `bun.lock` or `packageManager: bun@x` to run `bun run`. Add the `packageManager` field; it settles both.

## Deno

```yaml
services:
  api:
    run_script:
      file: deno.json
      name: dev
```

with

```json
{ "tasks": { "dev": "deno run --watch --allow-net --allow-env=PORT,HOST,DENO_CERT main.ts" } }
```

and `Deno.serve({ port: Number(Deno.env.get("PORT")), hostname: Deno.env.get("HOST") }, handler)`. `DENO_CERT` adds the devlanes CA to Deno's store. A `package.json` next to `deno.json` would send `run_script` to the npm runner, so name the Deno file.

## Python

`run_script` only reaches `[project.scripts]` entry points (`uv run <name>` or `poetry run <name>`). Dev servers are usually started by a tool, so `run` is the common form:

```yaml
services:
  api:
    platform: python
    cwd: services/api
    run: uv run uvicorn acme.main:app --host 127.0.0.1 --port ${{ self.port }} --reload --proxy-headers
    hosts: "api.${{ lane.domain }}"
    ready:
      http:
        path: /healthz
        timeout: 60s
  admin:
    platform: python
    cwd: services/admin
    run: uv run python manage.py runserver 127.0.0.1:${{ self.port }}
    hosts: "admin.${{ lane.domain }}"
```

`uv run` and `poetry run` take care of the virtualenv; devlanes never activates one. `PYTHONUNBUFFERED=1` makes logs arrive immediately.

Behind the proxy the app sees `http://127.0.0.1:<port>` with `X-Forwarded-Proto: https` and `X-Forwarded-Host`. Frameworks need to be told to trust them:

- Django: `ALLOWED_HOSTS = [".localhost"]`, `CSRF_TRUSTED_ORIGINS = os.environ.get("DEVLANES_ORIGINS", "").split(",")`, `SECURE_PROXY_SSL_HEADER = ("HTTP_X_FORWARDED_PROTO", "https")`.
- FastAPI/uvicorn: `--proxy-headers` (shown above).
- Flask: `ProxyFix(app.wsgi_app, x_for=1, x_proto=1, x_host=1)`.

`DEVLANES_ORIGINS` lists every https origin of the lane, so CORS and CSRF settings can read it instead of hardcoding hostnames.

`SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` point at the combined bundle (system roots plus the devlanes CA), because `requests` and `httpx` replace the default store instead of adding to it. `ssl`, `urllib3`, `requests`, `httpx` and `aiohttp` all read one of the two.

## Rust

```yaml
services:
  api:
    run_script:  # cargo run --bin server, -p <member> in a workspace
      file: Cargo.toml
      name: server
    hosts: "api.${{ lane.domain }}"
    ready:
      http:
        path: /healthz
        timeout: 5m
  api-watch:
    run: cargo watch -x 'run --bin server' # or bacon run; pass --port if the binary needs it
    autostart: false
```

Read `PORT` and `HOST` in `main` (`std::env::var("PORT")`). The first start compiles, so give `ready` a long timeout. `cargo run` builds into the worktree's own `target/`, which is what you want for parallel lanes (a shared `CARGO_TARGET_DIR` would serialise builds on the target lock); sccache or kache as `RUSTC_WRAPPER` shares compiled artefacts between worktrees instead.

`SSL_CERT_FILE` covers `rustls-native-certs`, `openssl` and `reqwest` with native roots. Clients built with `webpki-roots` only have no way to add a CA; the alternative is `reqwest` with the `rustls-tls-native-roots` feature.

## Tool versions: mise and direnv

Both run before anything else and only when the lane's root has their file.

- mise: with a `mise.toml`, `.mise.toml`, `.config/mise.toml`, `.tool-versions` or similar in the worktree root, devlanes runs `mise env --json` there and applies the result, so the pinned node, python or rust are on `PATH` for every unit. An untrusted config shows a note telling you to run `mise trust` in that folder.
- direnv: with an `.envrc` in the worktree root, `direnv export json` is applied, including variables it unsets. A blocked `.envrc` is skipped with the note `run direnv allow`.

Only the worktree root is checked, not the unit's `cwd`. Both tools are optional; when the binary is missing the note says so and the start goes on. Their output sits below devlanes' own variables, so they cannot change `PORT` or `HOST`.
