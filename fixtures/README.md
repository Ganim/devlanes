# Fixtures

Build `fixture-server` with `cargo build -p devlanes-testing --bin fixture-server`, then add the repository's `target/debug` directory to PATH. Native fixtures need Rust and Git, without Node or Python.

| Directory | Purpose | Status |
| --- | --- | --- |
| `two-services` | Independent API/web routing and worktree isolation | Present |
| `many-services` | 40 native services for terminal and concurrency checks | Present |
| `compose-pg-redis` | Shared infrastructure, per-lane databases | Planned for infrastructure milestone |
| `full` | Setup, migration, services and seed ordering | Planned for tasks milestone |
| `vite` | Real framework/HMR behavior | Planned, requires Node and ignored tests |
| `broken` | Invalid configuration diagnostics | Added with config coverage |

## Server contract

`PORT` defaults to 3000; `--port` overrides it. Port zero requests an OS-assigned port. `LISTENING <address>` is printed only after the listener binds. HTTP paths return JSON with the request path, headers and PID; `/ws` echoes text/binary frames; `/sse` emits numbered events every 100 ms.

| Flag | Behavior |
| --- | --- |
| `--ignore-sigterm` | Consume SIGTERM without exiting; SIGINT still exits |
| `--spawn-grandchild` | Start a child which starts a grandchild |
| `--setsid-grandchild` | Start the same tree with the grandchild in a new session |
| `--log-line <text>` | Print a line at startup; repeatable |
| `--exit-after <duration>` | Exit after e.g. `500ms` or `10s`; inherited by descendants |
| `--bind-other-port <port>` | Bind this port instead of the configured port |
| `--bind-ipv6-only` | Listen on `[::1]` with IPv6-only enabled |
| `--print-env` | Explicitly print the entire environment as an `ENV` JSON line |

Descendants announce `child/grandchild pid=<pid> sid=<sid>` and intentionally survive a parent's termination. Supervisor tests must clean up their own descendants. Always supply a deadline as a failure fallback. Use a cleared environment for `--print-env` tests to keep real credentials out of test output.
