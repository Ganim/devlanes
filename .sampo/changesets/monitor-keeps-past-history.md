---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
cargo/devlanes-tui: patch
---

Fixed the monitor (`M`) erasing the graph history of units, lanes and containers once they stopped or were cleaned.

They stay in the table as stopped until their samples leave the graph. A stopped lane no longer counts toward the monitor's current CPU and memory, and `status` no longer shows the last reading of a unit that failed.
