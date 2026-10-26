---
cargo/devlanes: patch
cargo/devlanes-proxy: patch
cargo/devlanes-pages: patch
cargo/devlanes-config: patch
cargo/devlanes-daemon: patch
---

Changed the proxy's error pages: each names an error code documented at devlanes.run/docs/errors, lists commands to diagnose it, and comes as Markdown to clients that do not ask for HTML. Requests to a starting service or a reconnecting box now wait for it, refused connections are retried for a few seconds, and forwarded responses carry `Via: 1.1 devlanes.run` unless `proxy.via: false`.
