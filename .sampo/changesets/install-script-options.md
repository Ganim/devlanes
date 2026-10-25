---
cargo/devlanes: patch
---

Added `--channel stable|next`, `--version` and `--dir` to `install.sh`, and `-Channel`, `-Version` and `-Dir` to `install.ps1`.

`curl -fsSL https://devlanes.run/install.sh | sh -s -- --channel next` installs the latest build of `main`; on Windows, `iex "& {$(irm https://devlanes.run/install.ps1)} -Channel next"`. The `DEVLANES_VERSION` and `DEVLANES_INSTALL_DIR` variables still work.
