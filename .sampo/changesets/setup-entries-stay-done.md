---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
---

Fixed `infra.setup` entries with a `container` running again at the next start after `devlanes run infra.setup.<name>`, a TUI run of the entry or a task step ran them, even when the container had not restarted.

Starting a service in a running lane is also faster: devlanes plans only the services it starts and checks every setup entry's container with one Docker Compose lookup.
