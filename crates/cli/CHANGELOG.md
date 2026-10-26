# devlanes-cli

## 0.4.3 — 2026-10-26

### Patch changes

- [6b60828](https://github.com/devlanes-run/devlanes/commit/6b608289d00c02d7f6181fad56def7e8546e3977) Added `devlanes config get`, `set` and `unset` to read and change the user config.yaml.
- [488f5be](https://github.com/devlanes-run/devlanes/commit/488f5bee3ef717386e6ea4ece47870af144ce613) Changed http readiness timeouts to name the request that failed, such as `GET /-/health (Host: 127.0.0.1:20008) answered 404`, and to suggest `ready.http.host` alongside `path` and `status`.

## 0.4.2 — 2026-10-26

### Patch changes

- [9a6e93f](https://github.com/devlanes-run/devlanes/commit/9a6e93f88aa5ab73e833c4d16a7e5a654d448a88) Changed the stop timeout warning in `devlanes doctor` and the TUI's daemon page to a shorter line, with its units listed on a separate line.
- [b3a2ee0](https://github.com/devlanes-run/devlanes/commit/b3a2ee03f66aa8c7515008f37031468fbc92c2ae) Added `devlanes infra logs <container> [-f]` and Docker logs in the TUI pane when an infra container is selected, read from Docker and followed across container restarts.

## 0.4.1 — 2026-10-26

### Patch changes

- [181698e](https://github.com/devlanes-run/devlanes/commit/181698ebbe25612c1149bb1c157857ff6acadf85) Changed `daemon status` and `doctor` on Windows to label the daemon's named pipe as `pipe` instead of `socket`.
- [181698e](https://github.com/devlanes-run/devlanes/commit/181698ebbe25612c1149bb1c157857ff6acadf85) Added `devlanes daemon start`, which starts the daemon in the background unless it already runs and prints its status.

## 0.4.0 — 2026-10-25

### Minor changes

- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Removed the fnox integration: the `fnox` config key and `devlanes secrets unlock`.
  
  Configs with a `fnox:` key fail to load. Run the tool in the command instead, for example `fnox exec -- npm run dev`. `${{ env.X }}` now reads the client environment, then `.env`.

### Patch changes

- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added `devlanes env export --shell pwsh` for Windows PowerShell 5.1 and PowerShell 7. The default, `posix`, is unchanged.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed `devlanes with` to skip the peak table when it is empty, and on an interactive terminal; CI output, pipes and the step summary still get it.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed `devlanes stop --logs-dir` printing output with `--quiet`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added the `infra.hosts` URLs of a container to `devlanes status` and the TUI container pane.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added a prompt to `devlanes lane new` that asks whether to start the new lane when run without `--start` on a terminal.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added WSL support: devlanes detects WSL and adapts to it.
  
  `devlanes url open` and `o` in the TUI open URLs in the Windows browser. A new `windows` target for `tls.trust` adds the CA to the Windows user store. Starting in a worktree under `/mnt/<drive>` warns, `doctor` gets a `wsl` group, and `daemon install` without systemd fails with the `wsl.conf` fix. See `docs/guides/wsl.md`.

