---
cargo/devlanes: minor
cargo/devlanes-daemon: minor
cargo/devlanes-state: minor
cargo/devlanes-proto: minor
cargo/devlanes-config: minor
cargo/devlanes-cli: minor
cargo/devlanes-tui: minor
---

Changed a plain `devlanes start` (and `s` on a lane in the TUI) to bring back the services the lane ran before, after a reboot, a daemon crash or `daemon stop`, including ones started by name.

`devlanes stop` without units now also forgets the lane's profiles, so the next plain start uses the `default` profile. A plain start names the other lanes that ran so `devlanes lane resume` can bring them back, and `devlanes with` no longer changes what a lane brings back. Set `start.restore: false` in `~/.devlanes/config.yaml` to always start the `default` profile.
