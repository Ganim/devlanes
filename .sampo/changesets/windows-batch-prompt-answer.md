---
cargo/devlanes: patch
cargo/devlanes-runner-windows: patch
---

Fixed `devlanes stop` on Windows sometimes waiting out the stop timeout for a service run through a `.cmd` shim such as `npm.cmd`, when its `Terminate batch job (Y/N)?` prompt missed the answer.
