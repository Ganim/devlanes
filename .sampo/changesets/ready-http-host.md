---
cargo/devlanes: patch
cargo/devlanes-config: patch
cargo/devlanes-resolve: patch
cargo/devlanes-daemon: patch
---

Added `ready.http.host` to set the Host header an http readiness check sends, for apps that answer their health route only on their own hostname, such as `host: ${{ self.hosts[0] }}`.
