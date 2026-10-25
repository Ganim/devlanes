---
cargo/devlanes: patch
cargo/devlanes-client: patch
---

Fixed an empty `HOME` being used as the home directory; devlanes now reports it as missing, like an unset one.
