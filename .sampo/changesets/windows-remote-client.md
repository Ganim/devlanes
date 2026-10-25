---
cargo/devlanes: patch
cargo/devlanes-daemon: patch
cargo/devlanes-proto: patch
cargo/devlanes-sync: patch
cargo/devlanes-transport-ssh: patch
---

Added `devlanes remote connect` on Windows, using the OpenSSH client in System32 or on `PATH`. Boxes stay Linux or macOS.

Passphrase prompts are relayed to the terminal. Names Windows can't hold are skipped and listed as sync issues. Running the OpenSSH Authentication Agent service avoids a prompt on every reconnect. See `docs/guides/remote-setup.md`.
