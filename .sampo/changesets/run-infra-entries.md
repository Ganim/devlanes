---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
cargo/devlanes-tui: patch
---

Added `devlanes run infra.setup.<name>` and `devlanes run infra.teardown.<name>` to run one setup or teardown entry. In the TUI, `s` and `r` on a setup step now run that entry instead of failing with "no lane or unit named setup.<name>" (a teardown step asks first), and steps are listed by their entry name, such as `databases` under `setup`.
