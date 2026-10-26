---
cargo/devlanes: patch
cargo/devlanes-proxy: patch
cargo/devlanes-daemon: patch
cargo/devlanes-cli: patch
---

Fixed opening the URLs of a box whose proxy serves HTTPS on another port than this machine's, such as 8443 on a box that cannot bind 443: the client now serves that box's hostnames on that port too, and `devlanes remote ls` warns when it cannot.
