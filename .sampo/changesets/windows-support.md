---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
cargo/devlanes-metrics-windows: patch
cargo/devlanes-resolver-nrpt: patch
cargo/devlanes-runner-windows: patch
cargo/devlanes-shell-windows: patch
cargo/devlanes-sys-windows: patch
cargo/devlanes-trust-windows: patch
---

Added native support for Windows 11 24H2 or later, on x64 and arm64.

Install with `irm https://devlanes.run/install.ps1 | iex`. `*.localhost` resolves without a hosts file, `daemon install` starts the daemon at logon, and `certs trust --resolver` writes an NRPT rule for custom domains. See `docs/guides/windows.md`.
