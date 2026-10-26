# devlanes-tui

## 0.4.2 ... 0.4.3 — 2026-10-26

Version bump only, to follow other packages.

## 0.4.1 — 2026-10-26

### Patch changes

- [e7fccbd](https://github.com/devlanes-run/devlanes/commit/e7fccbd548210a7d6a802badcaad39c8c8d47043) Added mouse selection to the daemon log in the TUI: dragging over it copies the text on release.
- [e7fccbd](https://github.com/devlanes-run/devlanes/commit/e7fccbd548210a7d6a802badcaad39c8c8d47043) Fixed the TUI reporting `started` when a service it started could not run, for example a command missing from PATH; the service now shows `start failed` with the reason.
- [b8ce96e](https://github.com/devlanes-run/devlanes/commit/b8ce96e40a27becff2afdb4f4c934e7bb358375d) Changed the TUI's first-run hint row into a welcome popup with the main features, the first keys and a link to the docs, so the footer is always a single row of keys.

## 0.4.0 — 2026-10-25

### Minor changes

- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Removed the fnox integration: the `fnox` config key and `devlanes secrets unlock`.
  
  Configs with a `fnox:` key fail to load. Run the tool in the command instead, for example `fnox exec -- npm run dev`. `${{ env.X }}` now reads the client environment, then `.env`.

### Patch changes

- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the TUI daemon log's copy mode to work like a unit pane: move the cursor with `h j k l`, mark with `v`, copy the text as shown with `y` or the whole entries it touches with `Y`.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the TUI to show the lanes beside the project name when there is only one project, which removes the separate lane row.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added colors to the level tags in the TUI daemon log.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed the TUI monitor not showing the history recorded while the TUI was closed.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed TUI lane chips to show the lane's branch, with `@N` only when two lanes share a name. An extra lane on the same folder shows its own name.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the mouse wheel in the TUI monitor to scroll the unit table without moving the selection.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Fixed the TUI daemon log scrolling into blank space when the log is short.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Changed the TUI to color project and lane names by state (green running, yellow starting or needing attention, red failed, dim idle) instead of showing state dots. The dots stay with `NO_COLOR` and the plain theme.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added a per-core CPU view to the TUI monitor. `m` puts memory in the large graph box and CPU in a small box with a square per core, and `c` switches back.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added the `infra.hosts` URLs of a container to `devlanes status` and the TUI container pane.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added `t` and `i` in the TUI lane tree to jump to the tasks and infra sections.
- [771b030](https://github.com/devlanes-run/devlanes/commit/771b030878fefcde77a4d3b35bd007c17c9b9b30) Added WSL support: devlanes detects WSL and adapts to it.
  
  `devlanes url open` and `o` in the TUI open URLs in the Windows browser. A new `windows` target for `tls.trust` adds the CA to the Windows user store. Starting in a worktree under `/mnt/<drive>` warns, `doctor` gets a `wsl` group, and `daemon install` without systemd fails with the `wsl.conf` fix. See `docs/guides/wsl.md`.

