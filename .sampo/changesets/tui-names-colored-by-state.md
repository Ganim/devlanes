---
cargo/devlanes: patch
cargo/devlanes-tui: patch
---

Changed the TUI to color project and lane names by state (green running, yellow starting or needing attention, red failed, dim idle) instead of showing state dots. The dots stay with `NO_COLOR` and the plain theme.
