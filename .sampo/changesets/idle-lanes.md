---
cargo/devlanes: minor
cargo/devlanes-daemon: minor
cargo/devlanes-proxy: minor
cargo/devlanes-config: minor
cargo/devlanes-cli: minor
cargo/devlanes-tui: minor
cargo/devlanes-proto: minor
cargo/devlanes-core: minor
cargo/devlanes-runner-pty: minor
cargo/devlanes-runner-windows: minor
cargo/devlanes-sys: minor
cargo/devlanes-sys-windows: minor
cargo/devlanes-infra-docker: minor
cargo/devlanes-infra-compose: minor
cargo/devlanes-resolve: minor
---

Added idle lanes: after `lanes.freeze_after` (default 15m) without activity, devlanes freezes a lane's processes and its own containers, and the next request, connection or command resumes them.

On macOS and Windows, `lanes.throttle_after` (default 5m) first runs an idle lane at background priority. Set `services.<name>.keep_awake: true` on workers that must keep running. `0s` turns either setting off. See `docs/guides/lifecycle.md`.
