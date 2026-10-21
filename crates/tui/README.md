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
    };
    devlanes_tui::run_with(Paths::discover()?, options).await
}
```

`run_with` connects first, spawning the daemon if needed, and returns `Error::NotATerminal` without a TTY. `run(paths)` is the same with no terminal launchers.

| key | does |
|---|---|
| `j`/`k`, arrows | move |
| `enter`, `h`/`l` | expand or collapse a lane |
| `shift+tab` | switch between the list and the unit's terminal |
| `s` / `x` / `r` | start / stop / restart the selected unit or lane |
| `t` | run a task (asks first) |
| `p` | start the lane with a profile |
| `z` | zoom the terminal |
| `pgup`/`pgdn`, wheel | scroll; older lines load from the daemon's logs |
| `c` | copy mode; `y` copies through OSC 52 |
| `o` / `O` | open the unit's URL / open the unit in a new terminal |
| `e` | recent errors with their full text |
| `tab` | lanes or monitor (from the list) |
| `?` | all keys |
| `q` | quit |

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

`O` then opens a kitty window in the lane's worktree, attached to the selected unit. Without `terminal:`, the TUI uses Terminal.app.

## Behaviour

Quitting closes connections only. Units, tasks and containers keep running in the daemon.

A failed action marks its row (`! start failed`) and shows the worded daemon error in the details pane until the unit or lane recovers. The header shows errors for a few seconds; `e` keeps the recent ones.

Only the selected unit is parsed client-side. tui-term 0.3 depends on vt100 0.16, so it renders through a newtype over vt100 0.18 (`screen::ScreenView`).

## License

Apache-2.0
