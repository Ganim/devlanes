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

Install the same release on the client and the box, then run `devlanes daemon restart` on the box. Until both match, `remote connect` fails and tells you to install the same release on both machines.
