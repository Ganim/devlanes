---
cargo/devlanes: patch
cargo/devlanes-proxy: patch
cargo/devlanes-daemon: patch
cargo/devlanes-cli: patch
---

Fixed the URLs of a box whose proxy serves HTTPS on another port than this machine's, such as 8443 on a box that cannot bind 443: `devlanes remote connect` now serves that box's hostnames on the box's port too, and `remote ls` warns when another process holds it.
