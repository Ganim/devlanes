---
cargo/devlanes: patch
cargo/devlanes-config: patch
cargo/devlanes-transport: patch
cargo/devlanes-transport-ssh: patch
cargo/devlanes-tui: patch
cargo/devlanes-tunnel: patch
---

Added `remote.start_daemon` to the user config of a box: set to `false`, `devlanes remote connect` no longer starts the box's daemon, and the client reports the stopped daemon and retries until it runs.
