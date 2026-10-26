---
cargo/devlanes: minor
cargo/devlanes-cli: patch
cargo/devlanes-daemon: minor
cargo/devlanes-proto: minor
cargo/devlanes-transport: minor
cargo/devlanes-transport-ssh: minor
cargo/devlanes-tunnel: minor
---

Changed the remote protocol to version 4, so `devlanes remote connect` works with boxes whose login shell prints to stdout, and `devlanes status --json` on a box lists each connected client with a stable `id` and its host `name`.

Install the same release on the client and the box, then run `devlanes daemon restart` on the box. A client and a box on different sides of this change cannot connect; a newer client says the box may run an older release.
