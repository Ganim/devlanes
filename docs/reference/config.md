# Config reference

<!-- Generated from crates/config/schema.json and crates/config/user-schema.json by crates/config/tests/it/reference_docs.rs. Edit the doc comments in crates/config, then run DEVLANES_BLESS=1 cargo nextest run -p devlanes-config. -->

Every field of `devlanes.yaml` and of the per-user `config.yaml`, generated from the JSON Schemas devlanes publishes. In a path, `<name>` stands for a key you choose (a service, task or profile name) and `[]` for an item of a list. A type marked *templated* also accepts a `${{ ... }}` expression, evaluated for each lane. A *duration* is a number with `ms`, `s`, `m`, `h` or `d`, and a *scalar* is a string, number or boolean. [examples/devlanes.yaml](../examples/devlanes.yaml) shows the fields in context.

## Project config

`devlanes.yaml` at the repository root (or a subdirectory, in a monorepo), with optional overrides in `devlanes.local.yaml` next to it. Schema: `https://devlanes.run/v1/schema.json`.

### fnox

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `fnox` | object | Project-wide fnox profile for secrets resolved by the runtime. | `{"profile":"dev"}` |
| `fnox.profile` | string (templated) | Secrets profile passed to fnox; uses fnox's default profile when omitted. | `dev` |

### global_env

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `global_env` | map of scalar (templated) | Environment overrides for tasks and services, after derived DEVLANES values. | `{"API_URL":"${{ svc.api.url }}"}` |

### infra

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `infra` | object | Shared and per-lane containers, setup commands, and teardown commands. | `{"compose":"docker-compose.yml","hidden":["postgres"]}` |
| `infra.compose` | string or list of string | Compose file or ordered list of Compose files. | `docker-compose.yml` |
| `infra.containers` | map of object | Plain Docker containers managed alongside Compose services. | `{"cerbos":{"image":"ghcr.io/cerbos/cerbos:0.40.0","per_lane":true,"ports":{"http":3592}}}` |
| `infra.containers.<name>.command` | string (templated) or list of string (templated) | Command replacing the image's default command. | `["server","--config=/policies/config.yaml"]` |
| `infra.containers.<name>.env` | map of scalar (templated) | Environment variables passed to this container. | `{"LOG_LEVEL":"debug"}` |
| `infra.containers.<name>.image` | string (templated) | Docker image to run; required after local overrides are merged. | `ghcr.io/cerbos/cerbos:0.40.0` |
| `infra.containers.<name>.per_lane` | boolean (templated) | Create one container per lane; defaults to a shared project container. | `true` |
| `infra.containers.<name>.ports` | map of integer (templated) | Named container ports; host ports are allocated by devlanes. | `{"grpc":3593,"http":3592}` |
| `infra.containers.<name>.ready` | `started` or object | Readiness check for the container. | `started` |
| `infra.containers.<name>.ready.cmd` | string (templated) or list of string (templated) | Retry this command until it exits successfully. | `curl -fsS ${{ self.url }}/healthz` |
| `infra.containers.<name>.ready.http` | object | Wait for an HTTP response matching the requested path and status. | `{"path":"/healthz","status":200,"timeout":"60s"}` |
| `infra.containers.<name>.ready.http.path` | string (templated) | Request path, such as /healthz. | `/healthz` |
| `infra.containers.<name>.ready.http.status` | integer (templated) | HTTP response status required for readiness, between 100 and 599. | `200` |
| `infra.containers.<name>.ready.http.timeout` | duration (templated) | Maximum time to wait for the HTTP probe to succeed. | `15s` |
| `infra.containers.<name>.ready.log` | string (templated) | Regular expression matched against output with terminal escape codes removed. | `Connected to Redis` |
| `infra.containers.<name>.ready.tcp` | boolean or string | Wait for the primary port with true, or a named port such as inspector. | `true` |
| `infra.containers.<name>.ready.timeout` | duration (templated) | Maximum time to wait for the readiness check. | `15s` |
| `infra.containers.<name>.volumes` | list of string (templated) | Docker source:destination[:mode] mounts; relative sources use the owning worktree. | `["./infra/cerbos:/policies:ro"]` |
| `infra.env` | map of scalar (templated) | Variables for Compose interpolation, separate from process global_env. | `{"ACME_DB_PORT":15432}` |
| `infra.exclude` | list of string | Compose services never started by devlanes. | `["caddy"]` |
| `infra.hidden` | list of string | Containers whose ports must not be mirrored to remote clients. | `["postgres"]` |
| `infra.hosts` | map of list of string (templated) or object | Container name to HTTP proxy route declarations. | `{"s3mock":["s3.${{ lane.domain }}"]}` |
| `infra.hosts.<name>[].fallback` | boolean (templated) | Use this route only when no other hostname matches; defaults to false. | `true` |
| `infra.hosts.<name>[].host` | string (templated) | Exact hostname or `*.` wildcard pattern, evaluated for the current lane. Labels hold letters, digits and inner hyphens; case and a trailing dot are ignored. | `api.${{ lane.domain }}` |
| `infra.hosts.<name>[].host_header` | `preserve` or `upstream` | Preserve the browser's Host header by default, or rewrite it for the upstream. | `preserve` |
| `infra.hosts.<name>[].port` | string | Named port to route to; uses the unit's primary port when omitted. | `inspector` |
| `infra.per_lane` | list of string | Compose services instantiated separately in each lane. | `["mailpit"]` |
| `infra.profiles` | list of string | Compose profiles enabled when resolving and starting infrastructure. | `["development"]` |
| `infra.project_name` | string (templated) | Shared Compose project name; defaults to Compose's own name. | `acme` |
| `infra.setup` | map of object or string (templated) or list of string (templated) | Named commands run once per lane after containers become healthy. | `{"database":"docker compose exec -T postgres createdb acme_${{ lane.name }}"}` |
| `infra.setup.<name>.container` | string | Container whose restart invalidates this setup entry's completion record, so it runs again. | `rabbitmq` |
| `infra.setup.<name>.cwd` | string (templated) | Working directory relative to the lane's worktree. | `infra` |
| `infra.setup.<name>.env` | map of scalar (templated) | Environment overrides for this command. | `{"AWS_DEFAULT_REGION":"us-east-1"}` |
| `infra.setup.<name>.ignore_errors` | boolean (templated) | Continue when the command fails; defaults to false. | `true` |
| `infra.setup.<name>.run` | string (templated) or list of string (templated) | Shell text or argv; mutually exclusive with run_script. | `pnpm dev` |
| `infra.setup.<name>.run_script` | string or object | Script source and name; mutually exclusive with run. | `apps/api/package.json#dev` |
| `infra.setup.<name>.run_script.name` | string | Script, target, or recipe name within the source file. Required, unless an override or extends child keeps the inherited name. | `dev` |
| `infra.setup.<name>.run_script.source` | string | Script source relative to the worktree, such as package.json, Makefile, or justfile. Required, unless an override or extends child keeps the inherited source. | `apps/api/package.json` |
| `infra.setup.<name>.timeout` | duration (templated) | Maximum run duration for this command. | `15s` |
| `infra.teardown` | map of object or string (templated) or list of string (templated) | Named cleanup commands run when a lane is explicitly cleaned. | `{"database":"docker compose exec -T postgres dropdb --if-exists acme_${{ lane.name }}"}` |
| `infra.teardown.<name>.container` | string | Container whose restart invalidates this setup entry's completion record, so it runs again. | `rabbitmq` |
| `infra.teardown.<name>.cwd` | string (templated) | Working directory relative to the lane's worktree. | `infra` |
| `infra.teardown.<name>.env` | map of scalar (templated) | Environment overrides for this command. | `{"AWS_DEFAULT_REGION":"us-east-1"}` |
| `infra.teardown.<name>.ignore_errors` | boolean (templated) | Continue when the command fails; defaults to false. | `true` |
| `infra.teardown.<name>.run` | string (templated) or list of string (templated) | Shell text or argv; mutually exclusive with run_script. | `pnpm dev` |
| `infra.teardown.<name>.run_script` | string or object | Script source and name; mutually exclusive with run. | `apps/api/package.json#dev` |
| `infra.teardown.<name>.run_script.name` | string | Script, target, or recipe name within the source file. Required, unless an override or extends child keeps the inherited name. | `dev` |
| `infra.teardown.<name>.run_script.source` | string | Script source relative to the worktree, such as package.json, Makefile, or justfile. Required, unless an override or extends child keeps the inherited source. | `apps/api/package.json` |
| `infra.teardown.<name>.timeout` | duration (templated) | Maximum run duration for this command. | `15s` |

### lanes

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `lanes` | object | Worktree registration, per-lane variables, and port-block policy. | `{"hosts_file":"always","primary":"main","vars":{"db_name":"acme_${{ lane.name }}"}}` |
| `lanes.block_size` | integer (templated) | Fixed port-block capacity; otherwise allocated with fifty percent headroom. | `80` |
| `lanes.hosts_file` | `auto` or `never` or `always` | Project-specific host-file hint, overriding the user's policy. | `auto` |
| `lanes.matrix` | list of map of scalar (templated) | Explicit lane values; entry N describes lane N. Mutually exclusive with vars. | `[{"domain":"acme.localhost","port":20000},{"domain":"acme-1.localhost","port":20100}]` |
| `lanes.max` | integer (templated) | Maximum assigned lanes; existing lanes are retained if this is lowered. | `6` |
| `lanes.new_worktrees` | `register` or `setup` or `start` | What happens when a worktree gets its lane; defaults to register. A worktree becomes a lane the first time devlanes runs in it (or `lane new` creates it), not because it exists; only the primary worktree gets one on its own. | `register` |
| `lanes.primary` | string | Branch assigned lane zero; defaults to the remote default branch, main, or master. | `main` |
| `lanes.removed_worktrees` | `stop` or `clean` | What happens to a running lane whose worktree folder disappears; defaults to stop. A lane whose folder is gone and that runs nothing is cleaned; its logs stay until log retention removes them. | `stop` |
| `lanes.vars` | map of scalar (templated) | Scalar lane values available through the lane expression namespace. | `{"db_name":"acme_${{ lane.name }}"}` |
| `lanes.worktree_dir` | string (templated) | Directory for newly created worktrees, relative to the project. | `../${{ project.name }}-lanes` |

### profiles

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `profiles` | map of list of string or object | Named service selectors with optional environment and infrastructure overrides. | `{"default":["+api","+web"],"e2e":{"include":["*"],"run_scripts":{"dev":"start"}}}` |
| `profiles.<name>.env` | map of scalar (templated) | Profile environment overrides, applied above service environment values. | `{"NODE_ENV":"production"}` |
| `profiles.<name>.exclude` | list of string | Selectors removed from the included service set. | `["+desktop","sms-stub"]` |
| `profiles.<name>.include` | list of string | Service names, globs, and +tag selectors included by this profile. | `["+api","+web"]` |
| `profiles.<name>.infra` | boolean (templated) | Set false to skip containers and infra setup. | `false` |
| `profiles.<name>.run_scripts` | map of string | Remap service script names once, such as dev to start; source files stay unchanged. | `{"dev":"start"}` |

### project

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `project` | string | Project identity shared by every worktree; lowercase letters, digits, dots, underscores, and dashes. | `acme` |

### services

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `services` | map of object | Long-running processes started together after infrastructure and before-service tasks. | `{"api":{"hosts":["api.${{ lane.domain }}"],"run_script":"apps/api/package.json#dev"}}` |
| `services.<name>.autostart` | boolean (templated) | Start by default unless explicitly selected; defaults to true. | `true` |
| `services.<name>.cwd` | string (templated) | Working directory relative to the lane worktree. | `apps/api` |
| `services.<name>.env` | map of scalar (templated) | Service-specific environment overrides, above global_env. | `{"API_URL":"${{ svc.api.url }}"}` |
| `services.<name>.extends` | string | Template name; resolved after devlanes.local.yaml is merged. | `fastify-api` |
| `services.<name>.fnox` | object | Override the project fnox profile for this service. | `{"profile":"dev"}` |
| `services.<name>.fnox.profile` | string (templated) | Secrets profile passed to fnox; uses fnox's default profile when omitted. | `dev` |
| `services.<name>.hidden` | boolean (templated) | Prevent remote port mirroring; defaults to false. | `false` |
| `services.<name>.hosts` | list of string (templated) or object | HTTP hosts, wildcard hosts, and routes to named ports. | `["api.${{ lane.domain }}"]` |
| `services.<name>.hosts[].fallback` | boolean (templated) | Use this route only when no other hostname matches; defaults to false. | `true` |
| `services.<name>.hosts[].host` | string (templated) | Exact hostname or `*.` wildcard pattern, evaluated for the current lane. Labels hold letters, digits and inner hyphens; case and a trailing dot are ignored. | `api.${{ lane.domain }}` |
| `services.<name>.hosts[].host_header` | `preserve` or `upstream` | Preserve the browser's Host header by default, or rewrite it for the upstream. | `preserve` |
| `services.<name>.hosts[].port` | string | Named port to route to; uses the unit's primary port when omitted. | `inspector` |
| `services.<name>.name` | string | Display name shown in the TUI and human output instead of the service key; any text, spaces allowed. Commands, references and `svc.<key>` keep using the key. | `Admin API` |
| `services.<name>.next` | object | Options under the service's `next:` key. | `{"args":["--turbo"]}` |
| `services.<name>.next.args` | list of string | Extra arguments appended after the preset's flags. | `["--turbo"]` |
| `services.<name>.platform` | string | Platform: node, bun, deno, python or rust; detected from project files when omitted. | `node` |
| `services.<name>.preset` | `vite` or `next` or `wrangler` or `tauri` | Registered framework preset; options use a sibling key with this name. | `vite` |
| `services.<name>.pty` | boolean (templated) | Use a pseudoterminal; defaults to true. | `true` |
| `services.<name>.ready` | `started` or object | Readiness policy; defaults to TCP with hosts, otherwise started. | `started` |
| `services.<name>.ready.cmd` | string (templated) or list of string (templated) | Retry this command until it exits successfully. | `curl -fsS ${{ self.url }}/healthz` |
| `services.<name>.ready.http` | object | Wait for an HTTP response matching the requested path and status. | `{"path":"/healthz","status":200,"timeout":"60s"}` |
| `services.<name>.ready.http.path` | string (templated) | Request path, such as /healthz. | `/healthz` |
| `services.<name>.ready.http.status` | integer (templated) | HTTP response status required for readiness, between 100 and 599. | `200` |
| `services.<name>.ready.http.timeout` | duration (templated) | Maximum time to wait for the HTTP probe to succeed. | `15s` |
| `services.<name>.ready.log` | string (templated) | Regular expression matched against output with terminal escape codes removed. | `Connected to Redis` |
| `services.<name>.ready.tcp` | boolean or string | Wait for the primary port with true, or a named port such as inspector. | `true` |
| `services.<name>.ready.timeout` | duration (templated) | Maximum time to wait for the readiness check. | `15s` |
| `services.<name>.replicas` | integer (templated) | Number of instances; replica.index starts at one. Defaults to one. | `1` |
| `services.<name>.run` | string (templated) or list of string (templated) | Shell command or argv; exactly one of run and run_script is required after inheritance. | `pnpm dev` |
| `services.<name>.run_script` | string or object | Script file and name, in shorthand or object form. | `apps/api/package.json#dev` |
| `services.<name>.run_script.name` | string | Script, target, or recipe name within the source file. Required, unless an override or extends child keeps the inherited name. | `dev` |
| `services.<name>.run_script.source` | string | Script source relative to the worktree, such as package.json, Makefile, or justfile. Required, unless an override or extends child keeps the inherited source. | `apps/api/package.json` |
| `services.<name>.stop` | object | Stop signal and grace period before killing the process tree. | `{"signal":"SIGINT","timeout":"15s"}` |
| `services.<name>.stop.signal` | string | Initial stop signal; defaults to SIGTERM, followed by SIGKILL if the grace period expires. On Windows, SIGINT sends Ctrl-C, other signals send Ctrl-Break plus WM_CLOSE to visible windows, SIGKILL terminates at once, and processes still running when the grace period expires are terminated. | `SIGTERM` |
| `services.<name>.stop.timeout` | duration (templated) | Grace period before SIGKILL; defaults to 10s. | `15s` |
| `services.<name>.sync` | list of object | Folders copied to stable local or remote-client paths. | `[{"from":"apps/extension/.output/chrome-mv3","ignore":["*.map"]}]` |
| `services.<name>.sync[].from` | string (templated) | Directory to copy, relative to the lane's worktree. | `apps/extension/.output/chrome-mv3` |
| `services.<name>.sync[].ignore` | list of string | Glob patterns excluded from the copy and subsequent updates. | `["*.map"]` |
| `services.<name>.sync[].to` | string (templated) | Destination on the client; defaults to a stable project/lane path under ~/devlanes/sync. | `~/devlanes/sync/acme/${{ lane.name }}/chrome-mv3` |
| `services.<name>.tags` | list of string | Tags used by +tag profile selectors. | `["api","essential"]` |
| `services.<name>.tauri` | object | Options under the service's `tauri:` key, after expression evaluation. | `{"before_dev_command":"","dev_url":"${{ svc.desktop_web.url }}","identifier":"io.acme.app.lane${{ lane.index }}.client${{ replica.index }}"}` |
| `services.<name>.tauri.before_dev_command` | string | `build.beforeDevCommand` override. Set it to an empty string when the frontend runs as its own devlanes service; otherwise every replica starts its own frontend on the port from `tauri.conf.json`. | `""` |
| `services.<name>.tauri.dev_url` | string | `build.devUrl`, usually another service's `url`. | `${{ svc.desktop_web.url }}` |
| `services.<name>.tauri.identifier` | string | Bundle identifier for this lane and replica. Required; it must differ per lane and replica, for example `com.acme.app.dev${{ '' if lane.index == 0 else '.lane' ~ lane.index }}.client${{ replica.index }}`. | `io.acme.app.lane${{ lane.index }}.client${{ replica.index }}` |
| `services.<name>.tauri.product_name` | string | `productName` override, shown in the dock and window menus. | `Acme Dev` |
| `services.<name>.vite` | object | Options under the service's `vite:` key. | `{"args":["--open"]}` |
| `services.<name>.vite.args` | list of string | Extra arguments appended after the preset's flags. | `["--open"]` |
| `services.<name>.wrangler` | object | Options under the service's `wrangler:` key. | `{"args":["--env-file",".dev.vars"],"isolate_registry":true,"vars":{"PUBLIC_BASE_URL":"${{ self.public_url }}"}}` |
| `services.<name>.wrangler.args` | list of string | Extra arguments appended after the preset's flags. | `["--env-file",".dev.vars"]` |
| `services.<name>.wrangler.isolate_registry` | boolean | Give each lane its own dev registry through `WRANGLER_REGISTRY_PATH`. Default: true. | `true` |
| `services.<name>.wrangler.vars` | map of string or number or boolean | Worker variables, passed as `--var KEY:VALUE`. Numbers and booleans are accepted because config scalars evaluate to them; wrangler sees strings. | `{"PUBLIC_BASE_URL":"${{ self.public_url }}"}` |

### tags

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `tags` | map of object | Tag settings. Units are colored by their first tag; colors are inferred from common tag names (api, web, worker, db, ...) when not set here. | `{"api":{"color":"cyan","description":"HTTP APIs"}}` |
| `tags.<name>.color` | `red` or `green` or `yellow` or `blue` or `magenta` or `cyan` or `white` or `gray` or `bright-red` or `bright-green` or `bright-yellow` or `bright-blue` or `bright-magenta` or `bright-cyan` | Terminal palette color; inferred by the resolver when omitted. | `red` |
| `tags.<name>.description` | string | Human-readable description available to the TUI. | `HTTP APIs` |

### tasks

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `tasks` | map of object | One-shot commands; lifecycle hooks execute in declaration order. | `{"migrate":{"run_script":"apps/api/package.json#migrate","when":"lane.start"}}` |
| `tasks.<name>.after` | `services` | Delay a create/start hook until all selected services are ready. | `services` |
| `tasks.<name>.confirm` | string (templated) | Confirmation text; noninteractive clients must explicitly approve. | `Reset ${{ lane.db_name }} and re-seed it?` |
| `tasks.<name>.cwd` | string (templated) | Working directory relative to the lane's worktree. | `apps/api` |
| `tasks.<name>.env` | map of scalar (templated) | Task-specific environment overrides. | `{"API_URL":"${{ svc.api.url }}"}` |
| `tasks.<name>.fnox` | object | Task-specific fnox profile. | `{"profile":"dev"}` |
| `tasks.<name>.fnox.profile` | string (templated) | Secrets profile passed to fnox; uses fnox's default profile when omitted. | `dev` |
| `tasks.<name>.interactive` | boolean (templated) | Require an attached terminal; incompatible with lifecycle hooks. | `true` |
| `tasks.<name>.profiles` | list of string | Profiles for which this lifecycle hook runs automatically. | `["default","backend"]` |
| `tasks.<name>.pty` | boolean (templated) | Allocate a pseudoterminal instead of pipes. | `true` |
| `tasks.<name>.run` | string (templated) or list of string (templated) | Shell text or an argument vector. Mutually exclusive with run_script and steps. | `pnpm dev` |
| `tasks.<name>.run_script` | string or object | Script source and name, such as package.json#migrate or Makefile#build. | `apps/api/package.json#dev` |
| `tasks.<name>.run_script.name` | string | Script, target, or recipe name within the source file. Required, unless an override or extends child keeps the inherited name. | `dev` |
| `tasks.<name>.run_script.source` | string | Script source relative to the worktree, such as package.json, Makefile, or justfile. Required, unless an override or extends child keeps the inherited source. | `apps/api/package.json` |
| `tasks.<name>.steps` | list of string | Sequential task names or infra.setup/infra.teardown entries, always run explicitly. | `["infra.teardown.database","infra.setup.database","migrate","seed"]` |
| `tasks.<name>.timeout` | duration (templated) | Maximum run duration, for example 10m. | `15s` |
| `tasks.<name>.when` | `lane.create` or `lane.start` or `lane.stop` or `lane.clean` | Lifecycle hook; omitted tasks run only on demand. | `lane.create` |

### templates

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `templates` | map of object | Reusable service settings, inherited with extends after local overrides are merged. | `{"fastify-api":{"platform":"node","ready":{"http":{"path":"/-/health"}},"tags":["api"]}}` |
| `templates.<name>.autostart` | boolean (templated) | Start by default unless explicitly selected; defaults to true. | `true` |
| `templates.<name>.cwd` | string (templated) | Working directory relative to the lane worktree. | `apps/api` |
| `templates.<name>.env` | map of scalar (templated) | Service-specific environment overrides, above global_env. | `{"API_URL":"${{ svc.api.url }}"}` |
| `templates.<name>.extends` | string | Template name; resolved after devlanes.local.yaml is merged. | `fastify-api` |
| `templates.<name>.fnox` | object | Override the project fnox profile for this service. | `{"profile":"dev"}` |
| `templates.<name>.fnox.profile` | string (templated) | Secrets profile passed to fnox; uses fnox's default profile when omitted. | `dev` |
| `templates.<name>.hidden` | boolean (templated) | Prevent remote port mirroring; defaults to false. | `false` |
| `templates.<name>.hosts` | list of string (templated) or object | HTTP hosts, wildcard hosts, and routes to named ports. | `["api.${{ lane.domain }}"]` |
| `templates.<name>.hosts[].fallback` | boolean (templated) | Use this route only when no other hostname matches; defaults to false. | `true` |
| `templates.<name>.hosts[].host` | string (templated) | Exact hostname or `*.` wildcard pattern, evaluated for the current lane. Labels hold letters, digits and inner hyphens; case and a trailing dot are ignored. | `api.${{ lane.domain }}` |
| `templates.<name>.hosts[].host_header` | `preserve` or `upstream` | Preserve the browser's Host header by default, or rewrite it for the upstream. | `preserve` |
| `templates.<name>.hosts[].port` | string | Named port to route to; uses the unit's primary port when omitted. | `inspector` |
| `templates.<name>.name` | string | Display name shown in the TUI and human output instead of the service key; any text, spaces allowed. Commands, references and `svc.<key>` keep using the key. | `Admin API` |
| `templates.<name>.next` | object | Options under the service's `next:` key. | `{"args":["--turbo"]}` |
| `templates.<name>.next.args` | list of string | Extra arguments appended after the preset's flags. | `["--turbo"]` |
| `templates.<name>.platform` | string | Platform: node, bun, deno, python or rust; detected from project files when omitted. | `node` |
| `templates.<name>.preset` | `vite` or `next` or `wrangler` or `tauri` | Registered framework preset; options use a sibling key with this name. | `vite` |
| `templates.<name>.pty` | boolean (templated) | Use a pseudoterminal; defaults to true. | `true` |
| `templates.<name>.ready` | `started` or object | Readiness policy; defaults to TCP with hosts, otherwise started. | `started` |
| `templates.<name>.ready.cmd` | string (templated) or list of string (templated) | Retry this command until it exits successfully. | `curl -fsS ${{ self.url }}/healthz` |
| `templates.<name>.ready.http` | object | Wait for an HTTP response matching the requested path and status. | `{"path":"/healthz","status":200,"timeout":"60s"}` |
| `templates.<name>.ready.http.path` | string (templated) | Request path, such as /healthz. | `/healthz` |
| `templates.<name>.ready.http.status` | integer (templated) | HTTP response status required for readiness, between 100 and 599. | `200` |
| `templates.<name>.ready.http.timeout` | duration (templated) | Maximum time to wait for the HTTP probe to succeed. | `15s` |
| `templates.<name>.ready.log` | string (templated) | Regular expression matched against output with terminal escape codes removed. | `Connected to Redis` |
| `templates.<name>.ready.tcp` | boolean or string | Wait for the primary port with true, or a named port such as inspector. | `true` |
| `templates.<name>.ready.timeout` | duration (templated) | Maximum time to wait for the readiness check. | `15s` |
| `templates.<name>.replicas` | integer (templated) | Number of instances; replica.index starts at one. Defaults to one. | `1` |
| `templates.<name>.run` | string (templated) or list of string (templated) | Shell command or argv; exactly one of run and run_script is required after inheritance. | `pnpm dev` |
| `templates.<name>.run_script` | string or object | Script file and name, in shorthand or object form. | `apps/api/package.json#dev` |
| `templates.<name>.run_script.name` | string | Script, target, or recipe name within the source file. Required, unless an override or extends child keeps the inherited name. | `dev` |
| `templates.<name>.run_script.source` | string | Script source relative to the worktree, such as package.json, Makefile, or justfile. Required, unless an override or extends child keeps the inherited source. | `apps/api/package.json` |
| `templates.<name>.stop` | object | Stop signal and grace period before killing the process tree. | `{"signal":"SIGINT","timeout":"15s"}` |
| `templates.<name>.stop.signal` | string | Initial stop signal; defaults to SIGTERM, followed by SIGKILL if the grace period expires. On Windows, SIGINT sends Ctrl-C, other signals send Ctrl-Break plus WM_CLOSE to visible windows, SIGKILL terminates at once, and processes still running when the grace period expires are terminated. | `SIGTERM` |
| `templates.<name>.stop.timeout` | duration (templated) | Grace period before SIGKILL; defaults to 10s. | `15s` |
| `templates.<name>.sync` | list of object | Folders copied to stable local or remote-client paths. | `[{"from":"apps/extension/.output/chrome-mv3","ignore":["*.map"]}]` |
| `templates.<name>.sync[].from` | string (templated) | Directory to copy, relative to the lane's worktree. | `apps/extension/.output/chrome-mv3` |
| `templates.<name>.sync[].ignore` | list of string | Glob patterns excluded from the copy and subsequent updates. | `["*.map"]` |
| `templates.<name>.sync[].to` | string (templated) | Destination on the client; defaults to a stable project/lane path under ~/devlanes/sync. | `~/devlanes/sync/acme/${{ lane.name }}/chrome-mv3` |
| `templates.<name>.tags` | list of string | Tags used by +tag profile selectors. | `["api","essential"]` |
| `templates.<name>.tauri` | object | Options under the service's `tauri:` key, after expression evaluation. | `{"before_dev_command":"","dev_url":"${{ svc.desktop_web.url }}","identifier":"io.acme.app.lane${{ lane.index }}.client${{ replica.index }}"}` |
| `templates.<name>.tauri.before_dev_command` | string | `build.beforeDevCommand` override. Set it to an empty string when the frontend runs as its own devlanes service; otherwise every replica starts its own frontend on the port from `tauri.conf.json`. | `""` |
| `templates.<name>.tauri.dev_url` | string | `build.devUrl`, usually another service's `url`. | `${{ svc.desktop_web.url }}` |
| `templates.<name>.tauri.identifier` | string | Bundle identifier for this lane and replica. Required; it must differ per lane and replica, for example `com.acme.app.dev${{ '' if lane.index == 0 else '.lane' ~ lane.index }}.client${{ replica.index }}`. | `io.acme.app.lane${{ lane.index }}.client${{ replica.index }}` |
| `templates.<name>.tauri.product_name` | string | `productName` override, shown in the dock and window menus. | `Acme Dev` |
| `templates.<name>.vite` | object | Options under the service's `vite:` key. | `{"args":["--open"]}` |
| `templates.<name>.vite.args` | list of string | Extra arguments appended after the preset's flags. | `["--open"]` |
| `templates.<name>.wrangler` | object | Options under the service's `wrangler:` key. | `{"args":["--env-file",".dev.vars"],"isolate_registry":true,"vars":{"PUBLIC_BASE_URL":"${{ self.public_url }}"}}` |
| `templates.<name>.wrangler.args` | list of string | Extra arguments appended after the preset's flags. | `["--env-file",".dev.vars"]` |
| `templates.<name>.wrangler.isolate_registry` | boolean | Give each lane its own dev registry through `WRANGLER_REGISTRY_PATH`. Default: true. | `true` |
| `templates.<name>.wrangler.vars` | map of string or number or boolean | Worker variables, passed as `--var KEY:VALUE`. Numbers and booleans are accepted because config scalars evaluate to them; wrangler sees strings. | `{"PUBLIC_BASE_URL":"${{ self.public_url }}"}` |

## User config

`config.yaml` in the devlanes home (`~/.devlanes`, or `$DEVLANES_HOME`), for settings of this machine. Schema: `https://devlanes.run/v1/user-schema.json`.

### history

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `history` | object | Retention for finished unit/task history and disconnected remote hosts in the database. | `{"remote_hosts_retention":"90d","retention":"30d"}` |
| `history.remote_hosts_retention` | duration | Maximum age since a remote host last connected successfully; defaults to 90d. Records without a timestamp are retained. | `90d` |
| `history.retention` | duration | Maximum age since a unit or task finished; defaults to 30d. Disk pressure may prune it sooner. | `30d` |

### hostnames

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `hostnames` | object | Hostname resolution strategy and optional hosts-file policy. | `{"hosts_file":"auto","resolver":"localhost"}` |
| `hostnames.base_domain` | string | Reserved public-resolver base domain; not supported yet. | `dev.example.invalid` |
| `hostnames.dns_provider` | object | Reserved DNS-01 provider settings; not supported yet. | `{"cloudflare":{"token":"${{ env.CF_API_TOKEN }}"}}` |
| `hostnames.dns_provider.cloudflare` | object | Cloudflare DNS API credentials supplied by an environment expression. | `{"token":"${{ env.CF_API_TOKEN }}"}` |
| `hostnames.dns_provider.cloudflare.token` | string (templated) | API token with permission to edit the domain's DNS records; prefer an environment expression. | `${{ env.CF_API_TOKEN }}` |
| `hostnames.hosts_file` | `auto` or `never` or `always` | Whether exact hostnames should also be written to the system hosts file. | `auto` |
| `hostnames.resolver` | `localhost` or `hosts_file` or `resolver` or `public` | localhost or resolver in v1; public parses but is not supported yet. | `localhost` |
| `hostnames.tld` | string | Custom top-level domain for the embedded DNS strategy. | `test` |

### logs

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `logs` | object | Retention for service output files and their database metadata. | `{"retention":{"days":7,"max_size":"5GiB"}}` |
| `logs.retention` | object | Age and total-size bounds for log files; the age limit also bounds database log-segment metadata. | `{"days":7,"max_size":"5GiB"}` |
| `logs.retention.days` | integer | Number of days to retain logs and database log-segment metadata; defaults to 7. | `7` |
| `logs.retention.max_size` | string | Maximum total size of retained logs; defaults to 5GiB. | `5GiB` |

### metrics

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `metrics` | object | Retention for monitor history, including aggregates stored in the database. | `{"retention":"30d"}` |
| `metrics.retention` | duration | Maximum age of persisted monitor samples and database run summaries; defaults to 30d. | `30d` |

### ports

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `ports` | object | Machine-wide pool shared by all project lane blocks. | `{"range":"20000-32767"}` |
| `ports.range` | string | Inclusive port range, written start-end; defaults to 20000-32767. | `20000-32767` |

### proxy

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `proxy` | object | HTTP/HTTPS listening ports and LAN access policy. | `{"http_port":80,"https_port":443,"lan":false}` |
| `proxy.http_port` | integer | HTTP listening port; defaults to 80 and redirects to HTTPS when TLS is enabled. | `80` |
| `proxy.https_port` | integer | HTTPS listening port; defaults to 443. | `443` |
| `proxy.lan` | boolean | Accept connections from other machines; defaults to loopback-only access. | `false` |

### startup

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `startup` | object | How many services start at once, across all lanes and projects. | `{"max":4,"mode":"fixed"}` |
| `startup.initial` | integer | Adaptive starting window; default CPU cores / 4, at least 2, at most `max`. | `2` |
| `startup.max` | integer | Largest window (the fixed window in `fixed` mode); default CPU cores. | `8` |
| `startup.mode` | `adaptive` or `fixed` or `all` | `adaptive`, `fixed` or `all`; default `fixed`. | `adaptive` |
| `startup.settle_cpu` | string | A `ready: started` service has finished starting once its process tree stays below this share of one core for 2 s; default 10%. | `10%` |
| `startup.slot_timeout` | duration | A start slot is given back after this even if the service is not ready, so services waiting on each other cannot block the queue; default 20s. | `15s` |
| `startup.target_cpu` | string | Adaptive mode grows the window while system CPU stays below this; default 75%. | `75%` |

### state

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `state` | object | Soft disk budget for the state database and its SQLite sidecar files. | `{"max_size":"512MiB"}` |
| `state.max_size` | string | Budget for the database plus WAL and shared-memory files; defaults to 512MiB and must be nonzero. Old metrics and history are pruned first; protected live state may exceed it. | `512MiB` |

### terminal

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `terminal` | `iterm2` or `ghostty` or `wezterm` or `kitty` or `terminal` or `windows-terminal` or object | Terminal app for open-in-terminal actions, alone or with the progress-bar setting. | `ghostty` |
| `terminal.app` | `iterm2` or `ghostty` or `wezterm` or `kitty` or `terminal` or `windows-terminal` | Terminal app used by open-in-terminal actions; unset picks Terminal.app on macOS, Windows Terminal on Windows when installed, else the first one installed. | `ghostty` |
| `terminal.progress` | `auto` or `always` or `never` | Show start progress in the terminal tab's progress bar (OSC 9;4); defaults to auto. | `auto` |

### tls

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `tls` | object | Certificate authority and explicitly requested trust targets. | `{"ca":"devlanes","trust":["system","firefox"]}` |
| `tls.ca` | `devlanes` or `mkcert` | Use devlanes' own CA by default, or reuse mkcert's CA. | `devlanes` |
| `tls.trust` | list of `system` or `firefox` or `chrome` or `ios-simulator` or `android-emulator` | Trust stores that should accept the local CA; mobile targets are ignored with a warning for now. | `["system","firefox"]` |

### tui

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `tui` | object | How commands use the terminal UI. | `{"open_on_start":false}` |
| `tui.glyphs` | `auto` or `unicode` or `ascii` | Symbols the TUI draws states and markers with; defaults to auto. | `auto` |
| `tui.open_on_start` | boolean | Run `start`, `restart`, `lane resume` and `lane new --start` inside the TUI when typed at a terminal; defaults to true. false prints them as text, like `--no-ui` or DEVLANES_NO_UI=1. | `true` |
| `tui.theme` | `auto` or `dark` or `light` or `ansi` | Colors of the TUI; defaults to auto. NO_COLOR always turns colors off. | `auto` |

### updates

| Path | Type | Description | Example |
| --- | --- | --- | --- |
| `updates` | object | Whether the daemon looks for new devlanes releases. | `{"check":false}` |
| `updates.check` | boolean | Check GitHub once a day for a newer devlanes release and show the upgrade command; defaults to true. | `true` |

