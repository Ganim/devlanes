# devlanes-migrate

Converts an existing process-manager setup (mprocs, dekit, process-compose, Procfile runners, pm2, tmuxinator, tmuxp, package.json parallel scripts, mise tasks) into `devlanes.yaml` text plus a list of notes. `devlanes migrate` in `bins/devlanes` calls it; it calls `devlanes-config` to validate what it produces and never writes files.

## Usage

```rust
use devlanes_migrate::{Options, detect, migrate};

let found = detect(std::path::Path::new("."));
let migration = migrate(std::path::Path::new("."), &found, &Options::default())?;
print!("{}", migration.yaml);
for note in &migration.notes {
    eprintln!("{}: {}", note.source, note.message);
}
```

## API

- `detect(dir)` and `detect_kind(dir, kind)`: the setups found in one directory, each with the files that will be read.
- `migrate(dir, found, options)`: converts and merges them into a `Migration` (`yaml`, `notes`, `sources`, unit names). The YAML has already passed `devlanes_config::parse_project` and `validate`.
- `check(yaml)`: the same validation on its own.
- `SourceKind`: the supported tools, their `--from` names (`SourceKind::parse` also takes `foreman`, `overmind`, `hivemind`, `honcho`, `concurrently`, `npm-run-all`) and the file names each looks for.
- `MigrateError::Conflict`: two sources define the same unit name differently; the caller asks which source to use.

Each tool is one module under `src/sources` implementing the `Source` trait (`file_names`, `detect`, `convert`). It parses its files with serde into typed models; fields it does not know are kept and reported as notes instead of failing.

## Behaviour

Every unit becomes a service unless the source says it runs once: dekit `type: job`, a process-compose process that others wait for with `process_completed` or `process_completed_successfully`, overmind `OVERMIND_CAN_DIE`, the Heroku `release` process type, steps npm-run-all runs before its parallel group, tmux session hooks and every mise task. A one-shot task that ran with the stack gets `when: lane.start`; if it waited for services it also gets `after: services` and a note, because devlanes has no dependencies between services.

Ports: devlanes gives each service its own port per lane and sets `PORT`. When a service has exactly one literal port (`env.PORT`, `--port N`, `-p N`, `PORT=N` or `localhost:N` in its command), the number becomes `${{ self.port }}`, a literal `PORT` is dropped, and `localhost:N` in other units becomes `${{ svc.<name>.port }}`. Several candidate ports leave the command alone with a note. `env.PORT: ${{ self.port }}` is not written: devlanes rejects it as a cycle, and `PORT` already holds that value. A readiness probe on the service's own port becomes `tcp: true` or `http: {path}`; on another port it becomes a `curl` command with a note.

Names are lowercased and limited to letters, digits, `_` and `-` (renames are noted). The project name comes from the directory name in the same way, allowing dots. Notes are written as `# migrated from <file>: ...` comments above the unit they concern, or at the top of the file. A Docker Compose file in the directory (`services` without `processes`) is suggested as a commented `infra.compose`; its services are not converted, since devlanes runs Compose itself.

`.env` values are never copied: they often hold secrets. devlanes reads `.env` only for `${{ env.NAME }}` expressions, so for tools that load it into every process the note names its variables and suggests `NAME: ${{ env.NAME }}` in `global_env`, or `dotenv` in `.envrc` (devlanes applies direnv).

## Mapping

"Not mapped" fields produce a note.

### mprocs

`mprocs.yaml`, else `mprocs.json`, in the directory mprocs starts in. The global `~/.config/mprocs/mprocs.yaml` holds only UI settings (`procs` is allowed only locally) and is not read.

| mprocs | devlanes |
| --- | --- |
| `procs.<name>: "cmd"` / `shell` | service, `run:` shell text (mprocs also ran it with `sh -c`) |
| `procs.<name>: [..]` / `cmd` | service, `run:` argv |
| `procs.<name>: null` | skipped, as in mprocs |
| `cwd` | `cwd:` relative to the project; `<CONFIG_DIR>` dropped; outside the project: not mapped |
| `env` | `env:`; a `null` value: not mapped, devlanes cannot unset a variable |
| `add_path` | `env.PATH: <entries>:${{ env.PATH }}`, with a note that it replaces the PATH devlanes builds |
| `autostart: false` | `autostart: false` |
| `autorestart` | not mapped: devlanes does not restart processes |
| `stop: SIGINT` / `SIGTERM` / `SIGKILL` / `hard-kill` | `stop.signal` (`hard-kill` is `SIGKILL`) |
| `stop: {send-keys: ["<C-c>"]}` | `stop.signal: SIGINT`; other keys not mapped |
| `stop: {cmd}` | not mapped: devlanes stops with a signal |
| `deps` | not mapped: devlanes starts services together |
| `log` | not mapped: devlanes keeps every unit's output |
| `$select: os` | resolved for the current OS (or `$else`), with a note |
| `hide_keymap_window`, `scrollback`, `keymap_*`, `proc_log`, `server`, `on_init`, ... | not mapped: mprocs UI and remote control |

### dekit

`dekit.yaml` in the directory, plus every fragment its `load` names (plain paths or globs, with `at` mounts).

| dekit | devlanes |
| --- | --- |
| `tasks.<path>` | service; `type: job` becomes a task; `/` in the path becomes `-` |
| `cmd` (argv / line) | `run:` argv / shell text (dekit refuses shell syntax in a line, so `sh` reads it the same) |
| `script` (.js on dekit's runtime) | not mapped |
| `autostart: true` | started with the lane; so are the services and jobs it depends on (devlanes has no `deps`); others get `autostart: false` |
| job started with the lane | `when: lane.start`; `after: services` when it needs a service |
| `deps` | used for the rule above; otherwise not mapped |
| `tags` | `tags:` (without `autostart`, `dynamic`) |
| `label` | comment above the unit |
| `cwd`, `env`, `add_path` | as for mprocs; relative to the declaring file |
| `ready.log` | `ready.log:` (escaped, since devlanes reads a regular expression) |
| `ready.tcp` / `ready.http` | settled by the port rule above |
| `ready.cmd` | `ready.cmd:` |
| `ready.file` | `ready.cmd: test -e <file>` |
| `ready.timeout` | `ready.timeout:`; `interval` not mapped |
| `stop: <signal>` / `stop.signal` | `stop.signal:` (the seven signals devlanes accepts) |
| `stop.timeout` | `stop.timeout:` |
| `stop.keys` | as mprocs `send-keys` |
| `stop.cmd`, `stop.group: false` | not mapped |
| `autorestart` | not mapped |
| `defaults` | applied to each task first (the task wins; `stop` merges part by part) |
| `log`, `scrollback_len`, `mouse_scroll_speed`, `kernel`, `on_init`, `on_idle`, `tui`, `keymap` | not mapped |

### process-compose

The first of `compose.yml`, `compose.yaml`, `process-compose.yml`, `process-compose.yaml` that has `processes`, merged with the matching `*.override.y(a)ml`.

| process-compose | devlanes |
| --- | --- |
| `processes.<name>` | service, or a task by the rule above; `is_foreground` becomes an interactive task, `availability.exit_on_end` an on-demand task |
| `command` | `run:` shell text (`$$` becomes `$`) |
| `entrypoint` | `run:` argv, with `command` as its last argument |
| `working_dir` | `cwd:` |
| `environment` (list or map), top-level `environment` | `env:` / `global_env:`; `$VAR` and `${VAR}` become `${{ env.VAR }}`; envsubst functions such as `${VAR^^}`: not mapped |
| `disabled: true` | `autostart: false` |
| `replicas` | `replicas:` (devlanes counts from 1, `PC_REPLICA_NUM` from 0) |
| `namespace` | `tags:` (without `default`) |
| `description` | comment above the unit |
| `ready_log_line` | `ready.log:` (already a regular expression) |
| `readiness_probe.exec.command` | `ready.cmd:` |
| `readiness_probe.http_get` | port rule above; `headers` and probe timings not mapped |
| `shutdown.signal` / `timeout_seconds` | `stop.signal` / `stop.timeout` |
| `shutdown.send_keys: '\x03'` | `stop.signal: SIGINT`; other keys not mapped |
| `shutdown.command`, `parent_only` | not mapped |
| `depends_on` between services | not mapped |
| `availability.restart`, `liveness_probe`, `is_daemon`, `env_file`, `log_location`, `vars`, Go templates | not mapped |

### Procfile (foreman, overmind, hivemind, honcho)

The Procfile named by `OVERMIND_PROCFILE` (`.overmind.env` or `.env`), `procfile:` (`.foreman`) or `PROCFILE` (honcho's `.env`), else `Procfile.dev`, else `Procfile`.

| Procfile runners | devlanes |
| --- | --- |
| `name: command` | service, `run:` shell text; `$PORT` keeps working because devlanes sets `PORT` |
| `$OVERMIND_PROCESS_<name>_PORT` | `${{ svc.<name>.port }}` |
| `OVERMIND_FORMATION` / `.foreman` `formation` (`name=N`, `all=N`) | `replicas: N`; `0` is `autostart: false` |
| `OVERMIND_PROCESSES`, `OVERMIND_IGNORED_PROCESSES` | `autostart: false` on the left-out processes |
| `OVERMIND_CAN_DIE` | task with `when: lane.start` |
| `release` process type | on-demand task |
| `OVERMIND_PORT`, `OVERMIND_PORT_STEP`, `OVERMIND_NO_PORT`, `.foreman` `port`, `OVERMIND_AUTO_RESTART`, `OVERMIND_ANY_CAN_DIE` | not mapped |
| `.env` | not copied |

### pm2

The first of `ecosystem.config.json`, `ecosystem.json`, `ecosystem.config.y(a)ml`, `ecosystem.y(a)ml`, `ecosystem.config.js`, `.cjs`, `.mjs`. A JavaScript file is read only when it exports a plain literal; otherwise the note gives the command that exports it as JSON.

| pm2 | devlanes |
| --- | --- |
| `apps[]` | service; `name` defaults to the script's file name |
| `script`, `args`, `interpreter`, `interpreter_args` / `node_args` | `run:` shell text; the interpreter comes from the extension as pm2 picks it (`.js` node, `.ts` bun, `.py` python, ...), `interpreter: none` runs the script directly |
| `cwd`, `env` | `cwd:`, `env:` |
| `instances: N` | `replicas: N`; `max`, `0`, `-1`: not mapped |
| (pm2 stops with SIGINT) | `stop.signal: SIGINT` |
| `kill_timeout` | `stop.timeout:` |
| `exec_mode: cluster` | note: each replica gets its own port |
| `env_<name>` | not mapped (a devlanes profile can carry env) |
| `watch`, `autorestart`, `max_restarts`, `max_memory_restart`, `cron_restart`, `min_uptime`, `restart_delay`, `wait_ready`, `listen_timeout`, `shutdown_with_message`, log files, `deploy` | not mapped |

### tmuxinator and tmuxp

tmuxinator's project-local `.tmuxinator.yml`; tmuxp's `.tmuxp.yaml`, `.tmuxp.yml` or `.tmuxp.json`. Panes become services when their last command starts a program; panes running an editor, a shell, a pager or `git` need a person and are left out.

| tmuxinator / tmuxp | devlanes |
| --- | --- |
| window or pane commands | `run:` shell text, one line per command (`source` becomes `.` for `sh`) |
| window name (plus pane title or number when a window has several) | service name |
| `root` / `start_directory` (session, window, pane) | `cwd:` when inside the project (`~` expands) |
| `pre_window` / `shell_command_before` | prepended to each pane's commands |
| tmuxp `environment` | `global_env:` (session) or `env:` (window, pane) |
| `on_project_start`, `pre` / tmuxp `before_script` | task with `when: lane.start` |
| `on_project_first_start` | task with `when: lane.create` |
| `on_project_stop` | task with `when: lane.stop` |
| `on_project_restart`, `on_project_exit`, `post`, layouts, tmux options | not mapped |

### package.json

The root `package.json` when a script runs `concurrently`, `npm-run-all --parallel` or `run-p` (`dev` first, then `start`, `develop`, `serve`).

| package.json | devlanes |
| --- | --- |
| concurrently `npm:` / `pnpm:` / `yarn:` / `bun:` / `node:` shortcuts, with `*` and `(!x)` | service with `run_script: package.json#<script>` |
| concurrently `npm run <script>` (and the pnpm, yarn, bun forms) | `run_script:` |
| concurrently other commands | `run:` shell text, prefixed with `npx`, `pnpm exec`, `yarn run` or `bun x` when the program is a package binary (outside a script, `node_modules/.bin` is not on PATH) |
| concurrently `-n` / `--names` (with `--name-separator`) | service names |
| npm-run-all `--parallel` / run-p patterns (`*`, `**`) | services with `run_script:` |
| npm-run-all steps before the first parallel group | tasks with `when: lane.start` |
| `"name -- args"` patterns | `run: <pm> run name -- args` |
| `-k`, `-r`, prefixes, colors, `--max-parallel`, ... | not mapped |

### mise

Tasks from `.config/mise/config.toml`, `.config/mise.toml`, `mise/config.toml`, `.mise/config.toml`, `mise.toml`, `.mise.toml` (later files win) and executable files in `mise-tasks/`, `.mise-tasks/`, `mise/tasks/`, `.mise/tasks/`, `.config/mise/tasks/`. mise does not say which tasks keep running, so every one is an on-demand task. `*.local.toml` files are personal and not read.

| mise | devlanes |
| --- | --- |
| `tasks.<name> = "cmd"` / `run` | task, `run:` shell text; several entries or lines get `set -e`, as mise runs with errexit |
| `run` entries `{task = ...}` | `mise run <task>` lines |
| `file`, file tasks | `run: ./<path>` (file task names follow mise: `db/seed.sh` is `db:seed`, renamed `db-seed`) |
| only `depends` | `steps:` |
| `depends` with `run` | not mapped |
| `dir` (`{{config_root}}` prefix) | `cwd:` |
| `env` | `env:` |
| `description` | comment above the task |
| `confirm` | `confirm:` |
| `[env]`, `[tools]` | not copied: devlanes adds the mise environment itself |
| `alias`, `hide`, `sources`, `outputs`, `shell`, `usage`, `quiet`, `tools`, `run_windows`, Tera templates, shebang scripts in `run` | not mapped |

## Not supported

- zellij layouts: KDL files with no project-local file name zellij looks for (layouts come from `~/.config/zellij/layouts` or `--layout <path>`), so there is nothing to detect, and parsing KDL needs a new dependency.
- devenv: `processes` live in `devenv.nix`, a Nix expression with no static form to read; `devenv.yaml` holds only inputs.
- Tilt (`Tiltfile`, Starlark) and Skaffold (Kubernetes manifests): programs or cluster deployments, not a list of local commands.
- Docker Compose services: devlanes runs Compose itself through `infra.compose`, which the output suggests as a comment instead of converting containers into processes.

## License

Apache-2.0
