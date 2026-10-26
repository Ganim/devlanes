# devlanes

## 0.4.4 — 2026-10-26

### Patch changes

- [b3a2ee0](https://github.com/devlanes-run/devlanes/commit/b3a2ee03f66aa8c7515008f37031468fbc92c2ae) Added `devlanes run infra.setup.<name>` and `devlanes run infra.teardown.<name>` to run one setup or teardown entry. In the TUI, `s` and `r` on a setup step now run that entry instead of failing with "no lane or unit named setup.<name>" (a teardown step asks first), and steps are listed by their entry name, such as `databases` under `setup`.
- [9a6e93f](https://github.com/devlanes-run/devlanes/commit/9a6e93f88aa5ab73e833c4d16a7e5a654d448a88) Added scrolling and text selection to the checks box on the TUI's daemon page; there `Tab` moves between the checks and the log instead of switching projects.
- [057966d](https://github.com/devlanes-run/devlanes/commit/057966d225932708635cc616c5e38afd80dce095) Changed the TUI monitor (`M`) to keep each project's shared infra on top and its lanes in lane order, and fixed its devlanes graphs showing less memory and CPU than used while shared containers run and drawing separate stripes for the time before it was open.
- [9a6e93f](https://github.com/devlanes-run/devlanes/commit/9a6e93f88aa5ab73e833c4d16a7e5a654d448a88) Changed the stop timeout warning in `devlanes doctor` and the TUI's daemon page to a shorter line, with its units listed on a separate line.
- [b3a2ee0](https://github.com/devlanes-run/devlanes/commit/b3a2ee03f66aa8c7515008f37031468fbc92c2ae) Added the TUI error list: `e`, or a click on the error in the header, shows the whole error the header cuts, steps back through earlier ones and copies them; a failed run of a task that has not run yet now shows on the task's row instead of the lane.
- [057966d](https://github.com/devlanes-run/devlanes/commit/057966d225932708635cc616c5e38afd80dce095) Added the daemon's own CPU and memory to the TUI: a row pinned under the monitor's table (`M`) with its graphs, and a `usage` line on the daemon page (`D`).
- [057966d](https://github.com/devlanes-run/devlanes/commit/057966d225932708635cc616c5e38afd80dce095) Changed the memory graph on the TUI monitor (`M`) to scale to the host's memory, draw the machine's used memory as a line and color only the seconds that were under memory pressure.
- [b3a2ee0](https://github.com/devlanes-run/devlanes/commit/b3a2ee03f66aa8c7515008f37031468fbc92c2ae) Added `devlanes infra logs <container> [-f]` and Docker logs in the TUI pane when an infra container is selected, read from Docker and followed across container restarts.
- [b3a2ee0](https://github.com/devlanes-run/devlanes/commit/b3a2ee03f66aa8c7515008f37031468fbc92c2ae) Changed the daemon log to record failed start, stop, restart, run and other requests that change lanes, with the error code and data.

## 0.4.3 — 2026-10-26

### Patch changes

- [546ea51](https://github.com/devlanes-run/devlanes/commit/546ea511256266e54b0b56fd05ecbab7953c1051) Added PyPI packages: `uv tool install devlanes`, `pipx install devlanes` or `pip install devlanes` installs the release build, and the update notice and `devlanes doctor` give the matching upgrade command.

## 0.4.2 — 2026-10-26

### Patch changes

- [f81274b](https://github.com/devlanes-run/devlanes/commit/f81274b9c8094c87886180d81da6ed5a88187ec0) Changed the proxy's error pages: each names an error code documented at devlanes.run/docs/errors, lists commands to diagnose it, and comes as Markdown to clients that do not ask for HTML. Requests to a starting service or a reconnecting box now wait for it, refused connections are retried for a few seconds, and forwarded responses carry `Via: 1.1 devlanes.run` unless `proxy.via: false`.

## 0.4.1 — 2026-10-26

### Patch changes

- [6545d4b](https://github.com/devlanes-run/devlanes/commit/6545d4bc38c1ea672a383bf1eb0c5e0077bf8adb) Fixed `git worktree move` failing with "Permission denied" on Windows while the devlanes daemon watched the repository.
- [e7fccbd](https://github.com/devlanes-run/devlanes/commit/e7fccbd548210a7d6a802badcaad39c8c8d47043) Added mouse selection to the daemon log in the TUI: dragging over it copies the text on release.
- [e7fccbd](https://github.com/devlanes-run/devlanes/commit/e7fccbd548210a7d6a802badcaad39c8c8d47043) Fixed the TUI reporting `started` when a service it started could not run, for example a command missing from PATH; the service now shows `start failed` with the reason.
- [181698e](https://github.com/devlanes-run/devlanes/commit/181698ebbe25612c1149bb1c157857ff6acadf85) Changed `daemon status` and `doctor` on Windows to label the daemon's named pipe as `pipe` instead of `socket`.
- [b8ce96e](https://github.com/devlanes-run/devlanes/commit/b8ce96e40a27becff2afdb4f4c934e7bb358375d) Changed the TUI's first-run hint row into a welcome popup with the main features, the first keys and a link to the docs, so the footer is always a single row of keys.
- [e7fccbd](https://github.com/devlanes-run/devlanes/commit/e7fccbd548210a7d6a802badcaad39c8c8d47043) Changed the daemon log entry for a service that could not start to a warning that names the cause without listing every PATH directory; the service's own log still lists them.
- [181698e](https://github.com/devlanes-run/devlanes/commit/181698ebbe25612c1149bb1c157857ff6acadf85) Added `devlanes daemon start`, which starts the daemon in the background unless it already runs and prints its status.

## 0.4.0 — 2026-10-25

### Minor changes

- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Removed the fnox integration: the `fnox` config key and `devlanes secrets unlock`.
  
  Configs with a `fnox:` key fail to load. Run the tool in the command instead, for example `fnox exec -- npm run dev`. `${{ env.X }}` now reads the client environment, then `.env`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Removed the `DEVLANES_PUBLIC_URL` variable that services received.
  
  Apps that read it get nothing now. Set it in the service's `env` instead:
  
  ```yaml
  env:
    DEVLANES_PUBLIC_URL: ${{ self.public_url }}
  ```
  
  `DEVLANES_PUBLIC_URL_<SERVICE>` in `with`, `exec` and `env show` is unchanged.

### Patch changes

- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added `devlanes env export --shell pwsh` for Windows PowerShell 5.1 and PowerShell 7. The default, `posix`, is unchanged.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed `devlanes with` to skip the peak table when it is empty, and on an interactive terminal; CI output, pipes and the step summary still get it.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the TUI daemon log's copy mode to work like a unit pane: move the cursor with `h j k l`, mark with `v`, copy the text as shown with `y` or the whole entries it touches with `Y`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed a worktree whose `.git` entry went missing being registered as the main checkout.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the TUI to show the lanes beside the project name when there is only one project, which removes the separate lane row.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed `devlanes stop --logs-dir` printing output with `--quiet`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added colors to the level tags in the TUI daemon log.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed a freshly started daemon exiting when the client that spawned it stopped waiting for it.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added native support for Windows 11 24H2 or later, on x64 and arm64.
  
  Install with `irm https://devlanes.run/install.ps1 | iex`. `*.localhost` resolves without a hosts file, `daemon install` starts the daemon at logon, and `certs trust --resolver` writes an NRPT rule for custom domains. See `docs/guides/windows.md`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed the TUI monitor not showing the history recorded while the TUI was closed.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed TUI lane chips to show the lane's branch, with `@N` only when two lanes share a name. An extra lane on the same folder shows its own name.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the mouse wheel in the TUI monitor to scroll the unit table without moving the selection.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed container metrics reading from the default Docker engine instead of the one `DOCKER_HOST` or the current Docker context selects.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed an empty `HOME` being used as the home directory; devlanes now reports it as missing, like an unset one.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed daemon startup waiting for a sleeping Docker engine (for example Docker Desktop's Resource Saver) to wake up.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed the TUI daemon log scrolling into blank space when the log is short.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the TUI to color project and lane names by state (green running, yellow starting or needing attention, red failed, dim idle) instead of showing state dots. The dots stay with `NO_COLOR` and the plain theme.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added an idle exit for daemons that a client started on demand, and the `daemon.exit_when_idle` user config key to turn it off.
  
  Such a daemon now exits about 30 seconds after the last TUI left, when nothing ran or was starting for that whole time. Containers keep running. Daemons installed with `devlanes daemon install` never exit when idle. Set `daemon.exit_when_idle: false` to keep the old behavior.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed remote sync overwriting files on case-insensitive folders when the box has names that differ only by case. Those names are skipped and listed as sync issues.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added a per-core CPU view to the TUI monitor. `m` puts memory in the large graph box and CPU in a small box with a square per core, and `c` switches back.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added `devlanes remote connect` on Windows, using the OpenSSH client in System32 or on `PATH`. Boxes stay Linux or macOS.
  
  Passphrase prompts are relayed to the terminal. Names Windows can't hold are skipped and listed as sync issues. Running the OpenSSH Authentication Agent service avoids a prompt on every reconnect. See `docs/guides/remote-setup.md`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added the `infra.hosts` URLs of a container to `devlanes status` and the TUI container pane.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added a prompt to `devlanes lane new` that asks whether to start the new lane when run without `--start` on a terminal.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added `t` and `i` in the TUI lane tree to jump to the tasks and infra sections.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added WSL support: devlanes detects WSL and adapts to it.
  
  `devlanes url open` and `o` in the TUI open URLs in the Windows browser. A new `windows` target for `tls.trust` adds the CA to the Windows user store. Starting in a worktree under `/mnt/<drive>` warns, `doctor` gets a `wsl` group, and `daemon install` without systemd fails with the `wsl.conf` fix. See `docs/guides/wsl.md`.
- [a71dc4a](https://github.com/devlanes-run/devlanes/commit/a71dc4a8b7e9cce1d45d369fe5ea3f3b73ee4181) Added `--channel stable|next`, `--version` and `--dir` to `install.sh`, and `-Channel`, `-Version` and `-Dir` to `install.ps1`.
  
  `curl -fsSL https://devlanes.run/install.sh | sh -s -- --channel next` installs the latest build of `main`; on Windows, `iex "& {$(irm https://devlanes.run/install.ps1)} -Channel next"`. The `DEVLANES_VERSION` and `DEVLANES_INSTALL_DIR` variables still work.
- [07ba115](https://github.com/devlanes-run/devlanes/commit/07ba115e2da3825ad16d6b8d1f8b1e5879f919e4) Changed the agent plugin's hook, which blocks dev servers started in the background inside a devlanes project, to run in devlanes for Claude Code's Bash and PowerShell tools, and added it for Codex.
  
  The hook now needs devlanes of the plugin's version on the agent's `PATH`; with an older or missing devlanes every call passes. Codex runs it after you trust it in `/hooks`.

