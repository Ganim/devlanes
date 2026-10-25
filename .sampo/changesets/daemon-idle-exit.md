---
cargo/devlanes: patch
cargo/devlanes-config: patch
cargo/devlanes-daemon: patch
---

Added an idle exit for daemons that a client started on demand, and the `daemon.exit_when_idle` user config key to turn it off.

Such a daemon now exits about 30 seconds after the last TUI left, when nothing ran or was starting for that whole time. Containers keep running. Daemons installed with `devlanes daemon install` never exit when idle. Set `daemon.exit_when_idle: false` to keep the old behavior.
