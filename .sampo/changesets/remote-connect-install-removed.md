---
cargo/devlanes: minor
cargo/devlanes-cli: minor
cargo/devlanes-config: minor
cargo/devlanes-daemon: minor
cargo/devlanes-transport: patch
cargo/devlanes-transport-ssh: minor
cargo/devlanes-tui: patch
cargo/devlanes-tunnel: minor
cargo/devlanes-version: minor
---

Removed `devlanes remote connect --install`; a box without devlanes now fails with `devlanes is not installed on <box>` and a link to the remote setup guide.

Install devlanes on the box with the usual install methods (`install.sh`, Homebrew, npm or cargo binstall). The new `remote.start_daemon` user config key on a box set to `false` stops connections from starting its daemon; clients then report the stopped daemon and retry. A connection to a mirrored port that fails on the box now shows as the port's `last_error` in `remote ls` and as an `open_failed` warning.
