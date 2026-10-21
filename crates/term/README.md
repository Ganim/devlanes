# devlanes-term

The `TerminalLauncher` trait behind "open in terminal" in the TUI. Each terminal application has its own `devlanes-term-*` crate implementing it, and `bins/devlanes` registers them.

## Usage

```rust
use devlanes_term::{LaunchRequest, TerminalLauncher};

fn open(launcher: &dyn TerminalLauncher) -> Result<(), devlanes_term::LaunchError> {
    launcher.launch(&LaunchRequest {
        cwd: "/work/acme".into(),
        command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
        title: Some("acme@2:api".into()),
    })
}
```

`launch` blocks while a short helper such as `osascript` or `open` runs, so call it from `spawn_blocking` in async code.

## API

- `TerminalLauncher::name` is the user config value that selects the launcher (`terminal: kitty`).
- `TerminalLauncher::invocation` plans the helper process as data; `launch` runs it.
- `Invocation::run` starts the helper with null stdio and reaps long-lived terminal processes on a background thread.
- `shell_quote`, `shell_join` and `applescript_string` quote commands for terminals that take shell text.

## License

Apache-2.0
