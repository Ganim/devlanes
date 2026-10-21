# devlanes-term-terminal-app

`TerminalLauncher` for macOS Terminal.app, selected by `terminal: terminal`. The TUI also falls back to it when no terminal is configured. Off macOS it returns `LaunchError::Unsupported`.

## Usage

```rust
use devlanes_term::TerminalLauncher;
use devlanes_term_terminal_app::TerminalApp;

// `request` is a `devlanes_term::LaunchRequest`.
TerminalApp.launch(&request)?;
```

It runs `osascript` with `do script "cd <cwd> && <command>"` and sets the window's custom title.

## License

Apache-2.0
