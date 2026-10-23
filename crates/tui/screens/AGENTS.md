# Committed TUI screens

The `.txt` files and `index.md` in this directory are generated and committed on purpose. Each `.txt` is one TUI state at one terminal size, drawn against the fake daemon in `crates/tui/examples/screens/`: the screen first, then the styles of its cells under `--- styles (row: columns style) ---`. They exist so a pull request shows in its diff what changed on screen.

If your change modified these files, that is expected, even when the change was not meant to touch the TUI: a renamed field, a new error message or a proto change can move what the TUI draws. Do not revert them and do not leave them out of the commit. CI runs `screens_are_current` (`crates/tui/tests/it/screens.rs`), which fails while the committed files differ from what the TUI draws.

1. Regenerate: `DEVLANES_BLESS=1 cargo nextest run -p devlanes-tui -E 'test(screens_are_current)'`. It also deletes the `.txt` of states that are no longer drawn.
2. Read `git diff crates/tui/screens`. Every change there should follow from your code change. If a screen changed in a way you did not intend, fix the code instead of committing the screen.
3. Commit the files together with the change, and mention visible TUI changes in the pull request.

Never edit these files by hand. To add or change a state, edit `scenes()` in `crates/tui/examples/screens/scenes.rs` or the fake data in `data.rs`, then regenerate.

The output must not depend on the machine, the date or the release. The fake pins the clock (`data::NOW_MS`), the version (`data::VERSION`, also set as `App::version`), pids, ports and paths. `ui::ago` reads the wall clock, so the fake data leaves `finished_at_ms` unset; keep it that way or pass the time in first. If the test passes on one OS and fails on the other, look for something read from the environment.

`cargo run -p devlanes-tui --example screens` writes the same files plus a PNG and an ANSI dump of each state for local review. Those are gitignored (`.gitignore` here) and never committed; PNGs depend on the system font, so they differ between macOS and Linux.
