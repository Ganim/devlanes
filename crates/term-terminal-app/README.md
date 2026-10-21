# devlanes-term-terminal-app

`TerminalLauncher` for macOS Terminal.app, selected by `terminal: terminal`. The TUI also falls back to it when no terminal is configured. Off macOS it returns `LaunchError::Unsupported`.

## Usage

```rust
use devlanes_term::{LaunchError, LaunchRequest, TerminalLauncher};
use devlanes_term_terminal_app::TerminalApp;

fn open(request: &LaunchRequest) -> Result<(), LaunchError> {
    TerminalApp.launch(request)
}
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in Terminal.app, also the default.
terminal: terminal
```

## Behaviour

It runs `osascript` with `do script "sh -c 'cd <cwd> && exec <command>'"` and sets the window's custom title.

## License

Apache-2.0
