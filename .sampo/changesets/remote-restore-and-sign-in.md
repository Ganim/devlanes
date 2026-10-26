---
cargo/devlanes: minor
cargo/devlanes-cli: patch
cargo/devlanes-daemon: minor
cargo/devlanes-state: minor
cargo/devlanes-transport: minor
cargo/devlanes-transport-ssh: minor
cargo/devlanes-proxy: minor
cargo/devlanes-pages: minor
---

Changed `devlanes remote connect` connections to come back after a daemon restart until `devlanes remote disconnect`, and to wait for `devlanes remote connect` when a reconnect needs a password or new credentials instead of failing.

While a box waits, `remote ls` and `status --json` show `needs_auth` with the command to run, its ports stay bound and its hostnames show a sign-in page (DL205). The state database gains a column, so a daemon of an earlier release cannot open it after the upgrade.
