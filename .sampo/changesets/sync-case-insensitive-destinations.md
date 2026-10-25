---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
cargo/devlanes-sync: patch
---

Fixed remote sync overwriting files on case-insensitive folders when the box has names that differ only by case. Those names are skipped and listed as sync issues.
