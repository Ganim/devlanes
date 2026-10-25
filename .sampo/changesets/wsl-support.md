---
cargo/devlanes: patch
cargo/devlanes-cli: patch
cargo/devlanes-config: patch
cargo/devlanes-daemon: patch
cargo/devlanes-sys: patch
cargo/devlanes-trust: patch
cargo/devlanes-trust-linux: patch
cargo/devlanes-tui: patch
---

Added WSL support: devlanes detects WSL and adapts to it.

`devlanes url open` and `o` in the TUI open URLs in the Windows browser. A new `windows` target for `tls.trust` adds the CA to the Windows user store. Starting in a worktree under `/mnt/<drive>` warns, `doctor` gets a `wsl` group, and `daemon install` without systemd fails with the `wsl.conf` fix. See `docs/guides/wsl.md`.
