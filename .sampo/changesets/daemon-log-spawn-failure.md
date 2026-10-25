---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
---

Changed the daemon log entry for a service that could not start to a warning that names the cause without listing every PATH directory; the service's own log still lists them.
