---
cargo/devlanes: patch
cargo/devlanes-tui: patch
---

Fixed the TUI reporting `started` when a service it started could not run, for example a command missing from PATH; the service now shows `start failed` with the reason.
