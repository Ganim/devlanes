---
cargo/devlanes: minor
cargo/devlanes-cli: minor
cargo/devlanes-config: minor
cargo/devlanes-core: minor
cargo/devlanes-daemon: minor
cargo/devlanes-proto: minor
cargo/devlanes-resolve: minor
cargo/devlanes-tui: minor
---

Removed the fnox integration: the `fnox` config key and `devlanes secrets unlock`.

Configs with a `fnox:` key fail to load. Run the tool in the command instead, for example `fnox exec -- npm run dev`. `${{ env.X }}` now reads the client environment, then `.env`.
