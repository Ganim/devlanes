# devlanes TUI screens

Every TUI state drawn against a fake daemon by `crates/tui/examples/screens/`. Each `.txt` is the screen, then the colors and attributes of its cells. These files are committed: the `screens_are_current` test fails when one is stale, and `AGENTS.md` here says how to update them. The PNG and ANSI files linked below are drawn by `cargo run -p devlanes-tui --example screens` and are not committed.

## lanes-unit

project view, watching a running service: tabs, lane chips, sections and groups

- 200x60: [txt](lanes-unit-200x60.txt) · [png](lanes-unit-200x60.png) · [ansi](lanes-unit-200x60.ansi)
- 120x40: [txt](lanes-unit-120x40.txt) · [png](lanes-unit-120x40.png) · [ansi](lanes-unit-120x40.ansi)
- 80x24: [txt](lanes-unit-80x24.txt) · [png](lanes-unit-80x24.png) · [ansi](lanes-unit-80x24.ansi)

## lanes-typing

the same service while typing into it (Enter): the footer banner says how back

- 200x60: [txt](lanes-typing-200x60.txt) · [png](lanes-typing-200x60.png) · [ansi](lanes-typing-200x60.ansi)
- 120x40: [txt](lanes-typing-120x40.txt) · [png](lanes-typing-120x40.png) · [ansi](lanes-typing-120x40.ansi)
- 80x24: [txt](lanes-typing-80x24.txt) · [png](lanes-typing-80x24.png) · [ansi](lanes-typing-80x24.ansi)

## lanes-zoomed

the pane zoomed (z)

- 200x60: [txt](lanes-zoomed-200x60.txt) · [png](lanes-zoomed-200x60.png) · [ansi](lanes-zoomed-200x60.ansi)
- 120x40: [txt](lanes-zoomed-120x40.txt) · [png](lanes-zoomed-120x40.png) · [ansi](lanes-zoomed-120x40.ansi)
- 80x24: [txt](lanes-zoomed-80x24.txt) · [png](lanes-zoomed-80x24.png) · [ansi](lanes-zoomed-80x24.ansi)

## sections-folded

tasks and infra folded by the user (h), the lane selected through its title (g)

- 200x60: [txt](sections-folded-200x60.txt) · [png](sections-folded-200x60.png) · [ansi](sections-folded-200x60.ansi)
- 120x40: [txt](sections-folded-120x40.txt) · [png](sections-folded-120x40.png) · [ansi](sections-folded-120x40.ansi)
- 80x24: [txt](sections-folded-80x24.txt) · [png](sections-folded-80x24.png) · [ansi](sections-folded-80x24.ansi)

## section-selected

the services section selected: its units with state, url, cpu and memory

- 200x60: [txt](section-selected-200x60.txt) · [png](section-selected-200x60.png) · [ansi](section-selected-200x60.ansi)
- 120x40: [txt](section-selected-120x40.txt) · [png](section-selected-120x40.png) · [ansi](section-selected-120x40.ansi)
- 80x24: [txt](section-selected-80x24.txt) · [png](section-selected-80x24.png) · [ansi](section-selected-80x24.ansi)

## infra-shared

infra: the lane's containers, the project's shared ones (hidden pgbouncer dimmed)

- 200x60: [txt](infra-shared-200x60.txt) · [png](infra-shared-200x60.png) · [ansi](infra-shared-200x60.ansi)
- 120x40: [txt](infra-shared-120x40.txt) · [png](infra-shared-120x40.png) · [ansi](infra-shared-120x40.ansi)
- 80x24: [txt](infra-shared-80x24.txt) · [png](infra-shared-80x24.png) · [ansi](infra-shared-80x24.ansi)

## setup-open

the infra section with the setup group unfolded

- 200x60: [txt](setup-open-200x60.txt) · [png](setup-open-200x60.png) · [ansi](setup-open-200x60.ansi)
- 120x40: [txt](setup-open-120x40.txt) · [png](setup-open-120x40.png) · [ansi](setup-open-120x40.ansi)
- 80x24: [txt](setup-open-80x24.txt) · [png](setup-open-80x24.png) · [ansi](setup-open-80x24.ansi)

## lane-overview

the lane selected (its title): facts, services not up, and every lane of the project

- 200x60: [txt](lane-overview-200x60.txt) · [png](lane-overview-200x60.png) · [ansi](lane-overview-200x60.ansi)
- 120x40: [txt](lane-overview-120x40.txt) · [png](lane-overview-120x40.png) · [ansi](lane-overview-120x40.ansi)
- 80x24: [txt](lane-overview-80x24.txt) · [png](lane-overview-80x24.png) · [ansi](lane-overview-80x24.ansi)

## lane-starting

a lane in the middle of its start: phases in the pane, ] to reach it

- 200x60: [txt](lane-starting-200x60.txt) · [png](lane-starting-200x60.png) · [ansi](lane-starting-200x60.ansi)
- 120x40: [txt](lane-starting-120x40.txt) · [png](lane-starting-120x40.png) · [ansi](lane-starting-120x40.ansi)
- 80x24: [txt](lane-starting-80x24.txt) · [png](lane-starting-80x24.png) · [ansi](lane-starting-80x24.ansi)

## lane-stopped

a lane with nothing running: every service listed, s starts it

- 200x60: [txt](lane-stopped-200x60.txt) · [png](lane-stopped-200x60.png) · [ansi](lane-stopped-200x60.ansi)
- 120x40: [txt](lane-stopped-120x40.txt) · [png](lane-stopped-120x40.png) · [ansi](lane-stopped-120x40.ansi)
- 80x24: [txt](lane-stopped-80x24.txt) · [png](lane-stopped-80x24.png) · [ansi](lane-stopped-80x24.ansi)

## lane-missing-worktree

a lane whose worktree directory is gone

- 200x60: [txt](lane-missing-worktree-200x60.txt) · [png](lane-missing-worktree-200x60.png) · [ansi](lane-missing-worktree-200x60.ansi)
- 120x40: [txt](lane-missing-worktree-120x40.txt) · [png](lane-missing-worktree-120x40.png) · [ansi](lane-missing-worktree-120x40.ansi)
- 80x24: [txt](lane-missing-worktree-80x24.txt) · [png](lane-missing-worktree-80x24.png) · [ansi](lane-missing-worktree-80x24.ansi)

## project-shop

another project tab (Shift+3)

- 200x60: [txt](project-shop-200x60.txt) · [png](project-shop-200x60.png) · [ansi](project-shop-200x60.ansi)
- 120x40: [txt](project-shop-120x40.txt) · [png](project-shop-120x40.png) · [ansi](project-shop-120x40.ansi)
- 80x24: [txt](project-shop-80x24.txt) · [png](project-shop-80x24.png) · [ansi](project-shop-80x24.ansi)

## project-config-error

a project whose devlanes.yaml does not load (Shift+2)

- 200x60: [txt](project-config-error-200x60.txt) · [png](project-config-error-200x60.png) · [ansi](project-config-error-200x60.ansi)
- 120x40: [txt](project-config-error-120x40.txt) · [png](project-config-error-120x40.png) · [ansi](project-config-error-120x40.ansi)
- 80x24: [txt](project-config-error-80x24.txt) · [png](project-config-error-80x24.png) · [ansi](project-config-error-80x24.ansi)

## unit-failed

a failed service: failure card above its last output

- 200x60: [txt](unit-failed-200x60.txt) · [png](unit-failed-200x60.png) · [ansi](unit-failed-200x60.ansi)
- 120x40: [txt](unit-failed-120x40.txt) · [png](unit-failed-120x40.png) · [ansi](unit-failed-120x40.ansi)
- 80x24: [txt](unit-failed-80x24.txt) · [png](unit-failed-80x24.png) · [ansi](unit-failed-80x24.ansi)

## unit-orphan

a process an earlier daemon left running

- 200x60: [txt](unit-orphan-200x60.txt) · [png](unit-orphan-200x60.png) · [ansi](unit-orphan-200x60.ansi)
- 120x40: [txt](unit-orphan-120x40.txt) · [png](unit-orphan-120x40.png) · [ansi](unit-orphan-120x40.ansi)
- 80x24: [txt](unit-orphan-80x24.txt) · [png](unit-orphan-80x24.png) · [ansi](unit-orphan-80x24.ansi)

## unit-stopped

a service that is not running (autostart off); hidden services are dimmed

- 200x60: [txt](unit-stopped-200x60.txt) · [png](unit-stopped-200x60.png) · [ansi](unit-stopped-200x60.ansi)
- 120x40: [txt](unit-stopped-120x40.txt) · [png](unit-stopped-120x40.png) · [ansi](unit-stopped-120x40.ansi)
- 80x24: [txt](unit-stopped-80x24.txt) · [png](unit-stopped-80x24.png) · [ansi](unit-stopped-80x24.ansi)

## task-never-run

a task from task.list that never ran

- 200x60: [txt](task-never-run-200x60.txt) · [png](task-never-run-200x60.png) · [ansi](task-never-run-200x60.ansi)
- 120x40: [txt](task-never-run-120x40.txt) · [png](task-never-run-120x40.png) · [ansi](task-never-run-120x40.ansi)
- 80x24: [txt](task-never-run-80x24.txt) · [png](task-never-run-80x24.png) · [ansi](task-never-run-80x24.ansi)

## unit-logs

stored logs of a unit after scrolling past its screen (PgUp)

- 200x60: [txt](unit-logs-200x60.txt) · [png](unit-logs-200x60.png) · [ansi](unit-logs-200x60.ansi)
- 120x40: [txt](unit-logs-120x40.txt) · [png](unit-logs-120x40.png) · [ansi](unit-logs-120x40.ansi)
- 80x24: [txt](unit-logs-80x24.txt) · [png](unit-logs-80x24.png) · [ansi](unit-logs-80x24.ansi)

## filter

the list filtered with /wrk while typing the filter

- 200x60: [txt](filter-200x60.txt) · [png](filter-200x60.png) · [ansi](filter-200x60.ansi)
- 120x40: [txt](filter-120x40.txt) · [png](filter-120x40.png) · [ansi](filter-120x40.ansi)
- 80x24: [txt](filter-80x24.txt) · [png](filter-80x24.png) · [ansi](filter-80x24.ansi)

## start-in-progress

devlanes start running inside the TUI: phase in the top bar

- 200x60: [txt](start-in-progress-200x60.txt) · [png](start-in-progress-200x60.png) · [ansi](start-in-progress-200x60.ansi)
- 120x40: [txt](start-in-progress-120x40.txt) · [png](start-in-progress-120x40.png) · [ansi](start-in-progress-120x40.ansi)
- 80x24: [txt](start-in-progress-80x24.txt) · [png](start-in-progress-80x24.png) · [ansi](start-in-progress-80x24.ansi)

## dialog-clean

clean confirmation (C)

- 200x60: [txt](dialog-clean-200x60.txt) · [png](dialog-clean-200x60.png) · [ansi](dialog-clean-200x60.ansi)
- 120x40: [txt](dialog-clean-120x40.txt) · [png](dialog-clean-120x40.png) · [ansi](dialog-clean-120x40.ansi)
- 80x24: [txt](dialog-clean-80x24.txt) · [png](dialog-clean-80x24.png) · [ansi](dialog-clean-80x24.ansi)

## dialog-profiles

profile picker (p)

- 200x60: [txt](dialog-profiles-200x60.txt) · [png](dialog-profiles-200x60.png) · [ansi](dialog-profiles-200x60.ansi)
- 120x40: [txt](dialog-profiles-120x40.txt) · [png](dialog-profiles-120x40.png) · [ansi](dialog-profiles-120x40.ansi)
- 80x24: [txt](dialog-profiles-80x24.txt) · [png](dialog-profiles-80x24.png) · [ansi](dialog-profiles-80x24.ansi)

## dialog-run-task

a task whose config asks first (s on seed)

- 200x60: [txt](dialog-run-task-200x60.txt) · [png](dialog-run-task-200x60.png) · [ansi](dialog-run-task-200x60.ansi)
- 120x40: [txt](dialog-run-task-120x40.txt) · [png](dialog-run-task-120x40.png) · [ansi](dialog-run-task-120x40.ansi)
- 80x24: [txt](dialog-run-task-80x24.txt) · [png](dialog-run-task-80x24.png) · [ansi](dialog-run-task-80x24.ansi)

## dialog-help

every key (?), this screen's first

- 200x60: [txt](dialog-help-200x60.txt) · [png](dialog-help-200x60.png) · [ansi](dialog-help-200x60.ansi)
- 120x40: [txt](dialog-help-120x40.txt) · [png](dialog-help-120x40.png) · [ansi](dialog-help-120x40.ansi)
- 80x24: [txt](dialog-help-80x24.txt) · [png](dialog-help-80x24.png) · [ansi](dialog-help-80x24.ansi)

## welcome

the first launch in a home: banner, features, first keys and the docs link

- 200x60: [txt](welcome-200x60.txt) · [png](welcome-200x60.png) · [ansi](welcome-200x60.ansi)
- 120x40: [txt](welcome-120x40.txt) · [png](welcome-120x40.png) · [ansi](welcome-120x40.ansi)
- 80x24: [txt](welcome-80x24.txt) · [png](welcome-80x24.png) · [ansi](welcome-80x24.ansi)

## theme-dark

true color on a dark background

- 200x60: [txt](theme-dark-200x60.txt) · [png](theme-dark-200x60.png) · [ansi](theme-dark-200x60.ansi)
- 120x40: [txt](theme-dark-120x40.txt) · [png](theme-dark-120x40.png) · [ansi](theme-dark-120x40.ansi)
- 80x24: [txt](theme-dark-80x24.txt) · [png](theme-dark-80x24.png) · [ansi](theme-dark-80x24.ansi)

## theme-light

true color on a light background

- 200x60: [txt](theme-light-200x60.txt) · [png](theme-light-200x60.png) · [ansi](theme-light-200x60.ansi)
- 120x40: [txt](theme-light-120x40.txt) · [png](theme-light-120x40.png) · [ansi](theme-light-120x40.ansi)
- 80x24: [txt](theme-light-80x24.txt) · [png](theme-light-80x24.png) · [ansi](theme-light-80x24.ansi)

## theme-no-color

NO_COLOR: shapes, words, bold and reverse video

- 200x60: [txt](theme-no-color-200x60.txt) · [png](theme-no-color-200x60.png) · [ansi](theme-no-color-200x60.ansi)
- 120x40: [txt](theme-no-color-120x40.txt) · [png](theme-no-color-120x40.png) · [ansi](theme-no-color-120x40.ansi)
- 80x24: [txt](theme-no-color-80x24.txt) · [png](theme-no-color-80x24.png) · [ansi](theme-no-color-80x24.ansi)

## glyphs-ascii

ASCII glyphs (ui.glyphs: ascii, or a locale without UTF-8)

- 200x60: [txt](glyphs-ascii-200x60.txt) · [png](glyphs-ascii-200x60.png) · [ansi](glyphs-ascii-200x60.ansi)
- 120x40: [txt](glyphs-ascii-120x40.txt) · [png](glyphs-ascii-120x40.png) · [ansi](glyphs-ascii-120x40.ansi)
- 80x24: [txt](glyphs-ascii-80x24.txt) · [png](glyphs-ascii-80x24.png) · [ansi](glyphs-ascii-80x24.ansi)

## monitor

the monitor (M): grouped by lane, sorted by cpu, api-search selected

- 200x60: [txt](monitor-200x60.txt) · [png](monitor-200x60.png) · [ansi](monitor-200x60.ansi)
- 120x40: [txt](monitor-120x40.txt) · [png](monitor-120x40.png) · [ansi](monitor-120x40.ansi)
- 80x24: [txt](monitor-80x24.txt) · [png](monitor-80x24.png) · [ansi](monitor-80x24.ansi)

## monitor-sorted-filtered

flat (e), sorted by memory (>), filtered with /acme

- 200x60: [txt](monitor-sorted-filtered-200x60.txt) · [png](monitor-sorted-filtered-200x60.png) · [ansi](monitor-sorted-filtered-200x60.ansi)
- 120x40: [txt](monitor-sorted-filtered-120x40.txt) · [png](monitor-sorted-filtered-120x40.png) · [ansi](monitor-sorted-filtered-120x40.ansi)
- 80x24: [txt](monitor-sorted-filtered-80x24.txt) · [png](monitor-sorted-filtered-80x24.png) · [ansi](monitor-sorted-filtered-80x24.ansi)

## monitor-detail

the detail graphs zoomed (z) on api-search

- 200x60: [txt](monitor-detail-200x60.txt) · [png](monitor-detail-200x60.png) · [ansi](monitor-detail-200x60.ansi)
- 120x40: [txt](monitor-detail-120x40.txt) · [png](monitor-detail-120x40.png) · [ansi](monitor-detail-120x40.ansi)
- 80x24: [txt](monitor-detail-80x24.txt) · [png](monitor-detail-80x24.png) · [ansi](monitor-detail-80x24.ansi)

## daemon

the daemon page (D): facts, doctor checks and the daemon log

- 200x60: [txt](daemon-200x60.txt) · [png](daemon-200x60.png) · [ansi](daemon-200x60.ansi)
- 120x40: [txt](daemon-120x40.txt) · [png](daemon-120x40.png) · [ansi](daemon-120x40.ansi)
- 80x24: [txt](daemon-80x24.txt) · [png](daemon-80x24.png) · [ansi](daemon-80x24.ansi)

## daemon-targets

the target picker on the daemon page (T): gate checked

- 200x60: [txt](daemon-targets-200x60.txt) · [png](daemon-targets-200x60.png) · [ansi](daemon-targets-200x60.ansi)
- 120x40: [txt](daemon-targets-120x40.txt) · [png](daemon-targets-120x40.png) · [ansi](daemon-targets-120x40.ansi)
- 80x24: [txt](daemon-targets-80x24.txt) · [png](daemon-targets-80x24.png) · [ansi](daemon-targets-80x24.ansi)

## daemon-targets-applied

the daemon log narrowed to the gate target (T, space, enter)

- 200x60: [txt](daemon-targets-applied-200x60.txt) · [png](daemon-targets-applied-200x60.png) · [ansi](daemon-targets-applied-200x60.ansi)
- 120x40: [txt](daemon-targets-applied-120x40.txt) · [png](daemon-targets-applied-120x40.png) · [ansi](daemon-targets-applied-120x40.ansi)
- 80x24: [txt](daemon-targets-applied-80x24.txt) · [png](daemon-targets-applied-80x24.png) · [ansi](daemon-targets-applied-80x24.ansi)

## daemon-search

a search on the daemon log (/ wrk-export, enter): matches marked

- 200x60: [txt](daemon-search-200x60.txt) · [png](daemon-search-200x60.png) · [ansi](daemon-search-200x60.ansi)
- 120x40: [txt](daemon-search-120x40.txt) · [png](daemon-search-120x40.png) · [ansi](daemon-search-120x40.ansi)
- 80x24: [txt](daemon-search-80x24.txt) · [png](daemon-search-80x24.png) · [ansi](daemon-search-80x24.ansi)

## daemon-copy

copy mode on the daemon log (c, v, k): two lines selected

- 200x60: [txt](daemon-copy-200x60.txt) · [png](daemon-copy-200x60.png) · [ansi](daemon-copy-200x60.ansi)
- 120x40: [txt](daemon-copy-120x40.txt) · [png](daemon-copy-120x40.png) · [ansi](daemon-copy-120x40.ansi)
- 80x24: [txt](daemon-copy-80x24.txt) · [png](daemon-copy-80x24.png) · [ansi](daemon-copy-80x24.ansi)

## unit-start-failed

a start that failed: badge on the row, full error in the pane

- 200x60: [txt](unit-start-failed-200x60.txt) · [png](unit-start-failed-200x60.png) · [ansi](unit-start-failed-200x60.ansi)
- 120x40: [txt](unit-start-failed-120x40.txt) · [png](unit-start-failed-120x40.png) · [ansi](unit-start-failed-120x40.ansi)
- 80x24: [txt](unit-start-failed-80x24.txt) · [png](unit-start-failed-80x24.png) · [ansi](unit-start-failed-80x24.ansi)

## dialog-errors

the error list (e): the newest error in full, the header cuts it

- 200x60: [txt](dialog-errors-200x60.txt) · [png](dialog-errors-200x60.png) · [ansi](dialog-errors-200x60.ansi)
- 120x40: [txt](dialog-errors-120x40.txt) · [png](dialog-errors-120x40.png) · [ansi](dialog-errors-120x40.ansi)
- 80x24: [txt](dialog-errors-80x24.txt) · [png](dialog-errors-80x24.png) · [ansi](dialog-errors-80x24.ansi)

## update-badge

a newer release in the header

- 200x60: [txt](update-badge-200x60.txt) · [png](update-badge-200x60.png) · [ansi](update-badge-200x60.ansi)
- 120x40: [txt](update-badge-120x40.txt) · [png](update-badge-120x40.png) · [ansi](update-badge-120x40.ansi)
- 80x24: [txt](update-badge-80x24.txt) · [png](update-badge-80x24.png) · [ansi](update-badge-80x24.ansi)

## menu

the Esc menu: banner over lane lines, version, options, help, quit

- 200x60: [txt](menu-200x60.txt) · [png](menu-200x60.png) · [ansi](menu-200x60.ansi)
- 120x40: [txt](menu-120x40.txt) · [png](menu-120x40.png) · [ansi](menu-120x40.ansi)
- 80x24: [txt](menu-80x24.txt) · [png](menu-80x24.png) · [ansi](menu-80x24.ansi)

## menu-light

the Esc menu in true color on a light background, quit selected

- 200x60: [txt](menu-light-200x60.txt) · [png](menu-light-200x60.png) · [ansi](menu-light-200x60.ansi)
- 120x40: [txt](menu-light-120x40.txt) · [png](menu-light-120x40.png) · [ansi](menu-light-120x40.ansi)
- 80x24: [txt](menu-light-80x24.txt) · [png](menu-light-80x24.png) · [ansi](menu-light-80x24.ansi)

## menu-ascii

the Esc menu with ASCII glyphs

- 200x60: [txt](menu-ascii-200x60.txt) · [png](menu-ascii-200x60.png) · [ansi](menu-ascii-200x60.ansi)
- 120x40: [txt](menu-ascii-120x40.txt) · [png](menu-ascii-120x40.png) · [ansi](menu-ascii-120x40.ansi)
- 80x24: [txt](menu-ascii-80x24.txt) · [png](menu-ascii-80x24.png) · [ansi](menu-ascii-80x24.ansi)

## options

options (Esc, o): the general tab, tui.theme with its values

- 200x60: [txt](options-200x60.txt) · [png](options-200x60.png) · [ansi](options-200x60.ansi)
- 120x40: [txt](options-120x40.txt) · [png](options-120x40.png) · [ansi](options-120x40.ansi)
- 80x24: [txt](options-80x24.txt) · [png](options-80x24.png) · [ansi](options-80x24.ansi)

## options-error

a typed value the config refuses: proxy.https_port equal to http_port

- 200x60: [txt](options-error-200x60.txt) · [png](options-error-200x60.png) · [ansi](options-error-200x60.ansi)
- 120x40: [txt](options-error-120x40.txt) · [png](options-error-120x40.png) · [ansi](options-error-120x40.ansi)
- 80x24: [txt](options-error-80x24.txt) · [png](options-error-80x24.png) · [ansi](options-error-80x24.ansi)

## options-preview

tui.theme in edit mode (Enter, →): light previews before saving

- 200x60: [txt](options-preview-200x60.txt) · [png](options-preview-200x60.png) · [ansi](options-preview-200x60.ansi)
- 120x40: [txt](options-preview-120x40.txt) · [png](options-preview-120x40.png) · [ansi](options-preview-120x40.ansi)
- 80x24: [txt](options-preview-80x24.txt) · [png](options-preview-80x24.png) · [ansi](options-preview-80x24.ansi)

## options-unsaved

two changes kept with Enter and not saved yet: the count in the title, s or [save] writes them

- 200x60: [txt](options-unsaved-200x60.txt) · [png](options-unsaved-200x60.png) · [ansi](options-unsaved-200x60.ansi)
- 120x40: [txt](options-unsaved-120x40.txt) · [png](options-unsaved-120x40.png) · [ansi](options-unsaved-120x40.ansi)
- 80x24: [txt](options-unsaved-80x24.txt) · [png](options-unsaved-80x24.png) · [ansi](options-unsaved-80x24.ansi)

## options-discard

Esc with unsaved changes: save changes, discard or keep editing

- 200x60: [txt](options-discard-200x60.txt) · [png](options-discard-200x60.png) · [ansi](options-discard-200x60.ansi)
- 120x40: [txt](options-discard-120x40.txt) · [png](options-discard-120x40.png) · [ansi](options-discard-120x40.ansi)
- 80x24: [txt](options-discard-80x24.txt) · [png](options-discard-80x24.png) · [ansi](options-discard-80x24.ansi)

## options-restart

after s: what was saved and the daemon restart it needs, R to restart

- 200x60: [txt](options-restart-200x60.txt) · [png](options-restart-200x60.png) · [ansi](options-restart-200x60.ansi)
- 120x40: [txt](options-restart-120x40.txt) · [png](options-restart-120x40.png) · [ansi](options-restart-120x40.ansi)
- 80x24: [txt](options-restart-80x24.txt) · [png](options-restart-80x24.png) · [ansi](options-restart-80x24.ansi)

## options-editing

typing a duration on the storage tab (4, Enter)

- 200x60: [txt](options-editing-200x60.txt) · [png](options-editing-200x60.png) · [ansi](options-editing-200x60.ansi)
- 120x40: [txt](options-editing-120x40.txt) · [png](options-editing-120x40.png) · [ansi](options-editing-120x40.ansi)
- 80x24: [txt](options-editing-80x24.txt) · [png](options-editing-80x24.png) · [ansi](options-editing-80x24.ansi)

## menu-narrow

the Esc menu without room for the banner

- 56x16: [txt](menu-narrow-56x16.txt) · [png](menu-narrow-56x16.png) · [ansi](menu-narrow-56x16.ansi)

## small-terminal

below the minimum size

- 36x7: [txt](small-terminal-36x7.txt) · [png](small-terminal-36x7.png) · [ansi](small-terminal-36x7.ansi)
