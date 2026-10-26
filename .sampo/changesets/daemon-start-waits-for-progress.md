---
cargo/devlanes: patch
cargo/devlanes-client: patch
cargo/devlanes-daemon: patch
cargo/devlanes-cli: patch
cargo/devlanes-tui: patch
---

Fixed the first `devlanes` command on a busy machine, or one run while another started the daemon, failing with "daemon is unreachable" while the daemon was still starting.

The command now waits as long as the daemon makes progress. When one startup step hangs for 30 seconds it fails with "the daemon has not finished starting", names the step and hints `devlanes daemon logs`.
