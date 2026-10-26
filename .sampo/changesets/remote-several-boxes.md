---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
cargo/devlanes-cli: patch
---

Fixed `devlanes remote connect` with several boxes: a hostname two boxes serve stays with the box connected first and the other box's lane is skipped with a warning naming it, a port another box's mirror holds is reported with that box and unit, and a box whose `ports.range` overlaps this machine's or another box's gets a warning.
