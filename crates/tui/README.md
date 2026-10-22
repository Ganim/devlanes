# devlanes-tui

The terminal UI opened by bare `devlanes` and `devlanes ui`. It is a daemon client like the CLI: it talks to the daemon through `devlanes-client` and shows the lane tree, a live terminal for the selected unit and a resource monitor.

## Usage

```rust
use std::sync::Arc;

use devlanes_term_ghostty::Ghostty;
use devlanes_term_terminal_app::TerminalApp;
use devlanes_tui::{Options, Paths};

#[tokio::main]
async fn main() -> Result<(), devlanes_tui::Error> {
    let options = Options {
        launchers: vec![Arc::new(Ghostty), Arc::new(TerminalApp)],
        // The user config's `terminal:` value, matched against each launcher's name.
        terminal: Some("ghostty".into()),
        // No tab progress bar.
        progress: None,
    };
    devlanes_tui::run_with(Paths::discover()?, options).await
}
```

`run_with` connects first, spawning the daemon if needed, and returns `Error::NotATerminal` without a TTY. `run(paths)` is the same with no terminal launchers.

`run_start` opens the TUI on a start that is already running, for `devlanes start` on a terminal. The binary sends the start request with `events: true`, reads its answer with `start::follow_start`, and waits with `start::await_start` until the daemon accepted it (a refusal comes back as an error for the CLI to print). The TUI then selects the started lane, shows the running phase in the header and, when the result arrives, the result line, which stays. Failed services get their reason and log tail in the details pane. `run_start` returns `StartExit::Ready`, `Failed` or `Pending` (quit before the result; the daemon keeps starting).

| key | does |
|---|---|
| `j`/`k`, arrows | move |
| `g` / `G` | first / last row, on every tab |
| `enter`, `h`/`l` | expand or collapse a lane |
| `shift+tab` | switch between the list and the unit's terminal |
| `s` / `x` / `r` | start / stop / restart the selected unit or lane |
| `t` | run a task (asks first) |
| `p` | pick profiles (space toggles, enter starts the lane with the checked ones) |
| `z` | zoom the terminal |
| `pgup`/`pgdn`, wheel | scroll; older lines load from the daemon's logs |
| `c` | copy mode; `y` copies through OSC 52 |
| `o` / `O` | open the unit's URL / open the unit in a new terminal |
| `C` | clean the selected lane; you type its name to confirm |
| `e` | recent errors with their full text |
| `tab`, `1`-`3` | lanes, monitor or daemon (from the list) |
| `?` | all keys |
| `q` | quit |

The daemon tab shows the daemon's status, each doctor check with its fix, and a live tail of `~/.devlanes/logs/daemon.log` (`j`/`k` scroll, `g` jumps to the oldest line, `G` follows again).

| key (daemon tab) | does |
|---|---|
| `R` | restart the daemon; asks first and says how many processes it stops |
| `S` | stop the daemon; asks first |
| `s` | start the daemon after it was stopped |
| `X` | lane gc; asks first |

Units a previous daemon left running show as `orphaned` with their pid, in amber, and their lane counts them. They have no terminal; `x` (or a lane stop, restart or `C` clean) asks the daemon to terminate them, and `s` shows the daemon's refusal.

The terminal pane only takes focus while its stream is live. Browsing the list watches a unit without resizing its PTY; focusing the pane or zooming takes over the size. When a unit restarts, the pane reopens its stream; a terminal that ended keeps focus and drops keys until `shift+tab`.

In terminal focus every key except `shift+tab` goes to the unit, encoded for its current vt100 modes. Tab and `C-a` reach the unit, so shell completion and a tmux prefix work; shift+tab is reserved for focus and apps in the pane don't receive it.

## API

- `run`, `run_with` and `Options` are the entry points for the binary.
- `DaemonApi` is every daemon call the TUI makes; `ClientApi` implements it over `devlanes-client`, tests use a fake.
- `drive` runs the event loop on any ratatui backend with any event stream.
- `app::App::update` turns messages into effects without I/O; `ui::render` draws an `App`.

## Config example

```yaml
# ~/.devlanes/config.yaml: terminal app the O key opens a unit in.
terminal: kitty
```

`O` then opens a kitty window in the lane's worktree, attached to the selected unit. Without `terminal:`, the TUI uses Terminal.app on macOS, else the first launcher that is installed. On a lane row it opens the user's shell instead: `$SHELL` on macOS and Linux, `%ComSpec%` on Windows.

The new terminal's command uses the TUI's `DEVLANES_HOME`. On macOS and Linux it runs as `env DEVLANES_HOME=<home> ...`, because terminals started through `open` or `osascript` get launchd's environment. Windows has no `env` program, so the launcher sets the variable on `wt.exe` or `wezterm`, which pass their environment to the tab. Windows builds offer Windows Terminal (the default) and WezTerm.

While lanes start, the terminal tab's progress bar shows ready services over the services starting, across every starting lane (the selected lane alone would jump while you browse). A failed start shows red for three seconds. Quitting, a panic or `terminal.progress: never` clears it; see `devlanes-term` for where it is drawn.

## Behaviour

Quitting closes connections only. Units, tasks and containers keep running in the daemon.

A failed action marks its row (`! start failed`) and shows the worded daemon error in the details pane until the unit or lane recovers. The header shows errors for a few seconds; `e` keeps the recent ones.

Only the selected unit is parsed client-side. tui-term 0.3 depends on vt100 0.16, so it renders through a newtype over vt100 0.18 (`screen::ScreenView`).

## License

Apache-2.0
