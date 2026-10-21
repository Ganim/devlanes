# devlanes-term-iterm2

`TerminalLauncher` for iTerm2, selected by `terminal: iterm2`. macOS only; elsewhere it returns `LaunchError::Unsupported`.

## Usage

```rust
use devlanes_term::{LaunchError, LaunchRequest, TerminalLauncher};
use devlanes_term_iterm2::Iterm2;

fn open(request: &LaunchRequest) -> Result<(), LaunchError> {
    Iterm2.launch(request)
}
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in iTerm2.
terminal: iterm2
```

## Behaviour

It runs `osascript`, opens a window with the default profile, sets the session name to the title and types `sh -c 'cd <cwd> && exec <command>'` into the login shell, so fish or nushell work too.

## License

Apache-2.0
