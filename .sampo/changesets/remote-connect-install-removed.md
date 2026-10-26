---
cargo/devlanes: minor
cargo/devlanes-cli: minor
cargo/devlanes-daemon: minor
cargo/devlanes-transport-ssh: minor
cargo/devlanes-version: minor
---

Removed `devlanes remote connect --install`; a box without devlanes now fails with `devlanes is not installed on <box>` and a link to the remote setup guide.

Install devlanes on the box yourself with `install.sh`, Homebrew, npm or cargo binstall, so that it is on the `PATH` of a non-interactive ssh session or in `~/.devlanes/bin` (`install.sh --dir ~/.devlanes/bin`). See `docs/guides/remote-setup.md`.
