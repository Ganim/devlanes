---
name: devlanes
description: Start, stop, inspect and test dev services with the devlanes CLI. Use in repositories that have a devlanes.yaml, or whenever you would otherwise start a dev server, watcher or other long-running command in the background with `&`.
---

# devlanes

devlanes runs each Git worktree of a project as a lane with its own ports, hostnames and data. A per-user daemon owns every process. The CLI sends it requests and prints a result, so a dev server keeps running after your shell command returns, and its logs stay queryable.

When the CLI detects an agent it prints terse, size-capped text and never prompts. Add `--json` to any command for complete structured output.

## Rules

- Never start a long-running process with `&`, `nohup`, `setsid` or a background shell. Use `devlanes start` for the services in `devlanes.yaml` and `devlanes exec --name NAME -- COMMAND` for anything else.
- `start`, `restart` and `wait` return once units are ready or have failed. Do not poll with `sleep` or `curl` loops.
- Read logs with `--tail`, `--grep` or `--since`. Output is capped at 16 KiB per command, and truncated output ends with the command that fetches the rest. Follow logs (`-f`) only together with `--timeout`.
- A command that needs confirmation exits 2 and names the flag. Rerun it with `--yes` only when the user asked for that action.
- Clean up with `devlanes stop --mine`. It stops only what your session started. Stopping or restarting a unit a human started works, and the result says so; do it only when the task requires it.
- Interrupting a command (Ctrl-C or a tool timeout) detaches from it. Services keep running.

## Lanes and units

A lane reference is `project@N` (lane number), `project@branch`, or `N` / `branch` alone inside the repository. Lane 0 is the main checkout.

A unit is a service, task or `exec` command, referenced as `[lane:]unit`: `api`, `2:api`, `acme@feat/login:web`. Commands that take several units accept globs and tags: `"api-*"`, `+backend`.

Output prints lanes as `acme@2 (feat/billing)`. Paste references back unchanged. A reference that matches nothing or several lanes fails with exit 3 and lists the candidates.

## Commands

| Goal | Command |
|---|---|
| Bring up this worktree's lane | `devlanes start`, or `devlanes start api web` |
| States, URLs and ports | `devlanes status`, `devlanes status --mine` |
| Wait for a unit or lane | `devlanes wait api --timeout 180s` |
| Recent logs | `devlanes logs api --tail 100` |
| Search logs | `devlanes logs api --grep 'ECONNREFUSED' --since 10m` |
| Lines around a log cursor | `devlanes logs api --around CURSOR -C 20` |
| Crashes, exit codes, port conflicts | `devlanes debug errors --since 15m` |
| Reason a unit is not running | `devlanes debug why worker` |
| Restart after a code change | `devlanes restart api` |
| Run a configured task | `devlanes run --list`, then `devlanes run migrate` |
| Ad-hoc long-running command | `devlanes exec --name storybook -- pnpm storybook` |
| URL of a service | `devlanes url print web` |
| Environment of a unit | `devlanes env show api`, or `eval "$(devlanes env export api)"` |
| Tests against the stack | `devlanes with -- pnpm test:e2e` |
| Stop what you started | `devlanes stop --mine` |
| Setup problems | `devlanes doctor` |

`devlanes agent-guide` prints a short version of this page.

## Running tests

`devlanes with [units...] [--profile P] -- COMMAND` starts the lane, waits for ready, runs COMMAND in the foreground and exits with its exit code. COMMAND gets the lane's environment plus `DEVLANES_PUBLIC_URL_<SERVICE>` for every service, so tests need no hardcoded ports. Afterwards it stops what it started. If a unit fails to start, it prints that unit's log tail and exits 1 without running COMMAND. `--keep` leaves the stack running; `--logs-dir DIR` writes every unit's log to DIR.

## Reading output

`devlanes start` with one failing service:

```
acme@2 (feat/billing): 7/8 ready, 1 failed, seed skipped
failed admin-api exit=1
  | Error: Cannot find module './env'
  |     at Module._resolveFilename (node:internal/modules/cjs/loader:1145:15)
  |     at /.../apps/admin-api/src/main.ts:3:1
  more: devlanes logs acme@2:admin-api --tail 200
ready crm-api https://crm.acme-2.localhost
ready user-web https://app.acme-2.localhost
ready +5 (devlanes status acme@2)
```

`devlanes status`:

```
running acme@0 main 8/8
failed acme@2 feat/billing 7/8 failed=admin-api
idle acme@3 fix/login
unassigned acme spike/xyz reason="pool full"
```

The first word of a status line is the state: `ready`, `starting`, `queued` (waiting for a start slot), `running`, `failed`, `stopping`, `stopped`, `exited` (a task or exec that ended), `idle` or `orphaned`. `orphaned` means a previous daemon left the process running and nothing supervises it: run `devlanes stop UNIT --lane LANE`, then start it again. Failures come first, each with its last log lines and the command that shows more. URLs are printed in full.

An error ends with a `hint:` line naming the command that fixes it. Ask the user before running a hint that stops services this session did not start. Examples are `devlanes stop --lane LANE` when `lane clean` refuses a lane that still runs, and `devlanes daemon restart` when the running daemon is another devlanes version (`protocol_mismatch` in JSON), which stops every service the daemon runs.

With `--json`, a command prints one JSON document; lists have the shape `{items, truncated, next_cursor}`. Streams (`start --json`, `logs -f --json`, `debug events -f`) print NDJSON.

Exit codes: 0 success, 1 failure, 2 usage error or missing `--yes`, 3 unknown or ambiguous reference, 4 daemon unreachable, 124 timeout.

## Typical loop

```sh
devlanes start
devlanes debug errors                # only if something failed
devlanes logs api --tail 200
# fix the code, then
devlanes restart api
devlanes with -- pnpm test
devlanes stop --mine
```

## Daemon and sandboxes

The CLI starts the daemon on first use. Inside a sandbox (for example Codex, which sets `CODEX_SANDBOX`) it refuses to and fails with `auto-spawn refused in sandbox`. Ask the user to run `devlanes daemon install` outside the sandbox. It registers a per-user service that starts the daemon now and at login. Then retry. `devlanes doctor` reports the socket and sandbox state.
