---
cargo/devlanes: patch
cargo/devlanes-metrics-docker: patch
---

Fixed container metrics reading from the default Docker engine instead of the one `DOCKER_HOST` or the current Docker context selects.
