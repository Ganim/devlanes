# Framework presets

A preset appends the flags a framework needs to listen on the lane's port, and nothing more. Without a preset, pass the port yourself with `${{ self.port }}`. Options go under a key named after the preset.

| Preset | Flags added | Also | Options |
| --- | --- | --- | --- |
| `vite` | `--port <port> --strictPort --host 127.0.0.1` | | `vite.args` |
| `next` | `--port <port> --hostname 127.0.0.1` | | `next.args` |
| `wrangler` | `--port <port> --inspector-port <ports.inspector>`, `--var K:V` per var | `WRANGLER_REGISTRY_PATH` per lane | `wrangler.vars`, `wrangler.args`, `wrangler.isolate_registry` |
| `tauri` | `--config <overlay.json>` | `DEVLANES_TAURI_CONFIG` | `tauri.identifier`, `tauri.dev_url`, `tauri.product_name`, `tauri.before_dev_command` |

With `run_script` the flags go after the script name, placed per package manager (see [run and run_script](run-scripts.md#packagejson)). The script's last command must be the framework binary: `"dev": "vite"` works, `"dev": "vite && echo done"` does not. With a plain `run`, the flags are appended to the command text, with a `--` inserted only for `npm run`.

## Vite

```yaml
services:
  web:
    preset: vite
    run_script: web/package.json#dev
    env: { VITE_API_URL: "${{ svc.api.public_url }}" }
    hosts: ["${{ lane.domain }}"]
```

`--strictPort` makes Vite exit instead of picking another port, which is what you want: a port hop would leave the proxy pointing at nothing. Vite accepts `.localhost` hostnames by default; with a custom TLD set `server.allowedHosts` in `vite.config.ts`. HMR works through the proxy because the client connects back to the page's own host over the same TLS connection; no `hmr.clientPort` is needed.

`vite preview` takes the same flags, so an `e2e` profile with `run_scripts: { dev: preview }` serves the built app on the same hostname. `serve -s dist` does not accept them.

The Cloudflare Vite plugin runs workerd inside Vite. Its inspector port is a literal in `vite.config.ts` by default; read it from an environment variable and feed it `${{ self.ports.inspector }}` so two lanes do not collide.

## Next

```yaml
services:
  web:
    preset: next
    run_script: apps/web/package.json#dev
    next: { args: [--turbopack] }
    hosts: ["${{ lane.domain }}"]
    ready: { http: { path: /, timeout: 120s } }
```

Next 15.3 and later warn about dev requests from an origin other than the one it listens on and may block them; set `allowedDevOrigins: ["*.localhost"]` in `next.config.js` when that happens. Server-side `fetch` to other lane services trusts the CA through `NODE_EXTRA_CA_CERTS`.

## Wrangler

```yaml
templates:
  worker:
    platform: node
    preset: wrangler
    ready: { http: { path: /-/health, timeout: 90s } }
services:
  api-gateway:
    extends: worker
    run_script: workers/api-gateway/package.json#dev
    wrangler:
      vars: { PUBLIC_BASE_URL: "${{ self.public_url }}" }
      args: [--env-file, .dev.vars]
    hosts: ["api.${{ lane.domain }}"]
lanes:
  hosts_file: always
```

- `inspector` is allocated automatically for every wrangler service; `--inspector-port` gets it. Route a hostname to it with `hosts: [{ host: "inspect.${{ lane.domain }}", port: inspector }]`.
- `vars` become `--var KEY:VALUE`, which override the `vars` in `wrangler.toml`. Numbers and booleans are stringified.
- Each lane gets its own dev registry (`<worktree>/.devlanes/wrangler-registry`), so service bindings between workers of one lane resolve to that lane's workers. `isolate_registry: false` turns that off. The same variable is set for every unit of a project that contains a `wrangler.toml|json|jsonc`, so a plain `node` worker in the same repo talks to the right registry too.
- A `dev` script that already passes `--port` or `--inspector-port` is refused with the fix in the message. Move the default into `wrangler.toml` (`dev.port`, `dev.inspector_port`) and let the flag override it.
- workerd does not resolve `*.localhost` itself. `lanes.hosts_file: always` writes the lane's exact hostnames to `/etc/hosts` (after `devlanes certs trust --hosts`), so worker-to-worker `fetch` by hostname works. Wildcards cannot go in `/etc/hosts`; use service bindings or the registry for those.

Durable Objects, D1, KV and R2 local state live under `.wrangler/` in the worker's directory, which is per worktree and therefore per lane.

## Tauri

```yaml
services:
  desktop-web:
    preset: vite
    run_script: apps/desktop/package.json#dev:web
  desktop:
    preset: tauri
    run_script: apps/desktop/package.json#dev
    tauri:
      identifier: com.acme.app.dev${{ '' if lane.index == 0 else '.lane' ~ lane.index }}.client${{ replica.index }}
      dev_url: ${{ svc.desktop_web.url }}
      before_dev_command: ""
    replicas: 2
    env:
      CDP_PORT: ${{ self.ports.cdp }}
    ready: { tcp: cdp, timeout: 15m }
```

The preset writes `<worktree>/.devlanes/tauri/<service>.<replica>.conf.json` and passes `--config`, which Tauri 2 merges over `tauri.conf.json`. Nothing in the repository is edited.

- `identifier` is required and must differ per lane and replica, since it decides where the app stores its data and which running instance a deep link reaches. The example above keeps lane 0 on the production-like id. Two units with the same id are refused at start, across lanes too.
- `dev_url` points Tauri at a frontend that devlanes already runs. Then set `before_dev_command: ""`, otherwise every replica starts its own frontend on the port from `tauri.conf.json`.
- `replicas` opens several windows of the same app in one lane; `replica.index` starts at 1. Named ports are per service, so with replicas every instance shares them. Use separate services when each instance needs its own debugger port.
- Wrappers that do not forward arguments can read `DEVLANES_TAURI_CONFIG` instead.

## Writing a preset-less service

```yaml
services:
  docs:
    run: pnpm -F @acme/docs exec rspress dev --host 127.0.0.1 --port ${{ self.port }}
    hosts: ["docs.${{ lane.domain }}"]
  storybook:
    run_script: packages/ui/package.json#storybook   # "storybook": "storybook dev --ci -p $PORT"
    hosts: ["storybook.${{ lane.domain }}"]
```

Any tool that takes a port flag or reads `PORT` works this way. Bind `127.0.0.1` when the tool lets you; the proxy dials loopback.
