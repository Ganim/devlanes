# devlanes-term-windows-terminal

`TerminalLauncher` for Windows Terminal, selected by `terminal: windows-terminal`. `bins/devlanes` registers it on Windows, where the TUI's `O` key uses it.

## Usage

```rust
use devlanes_term::{LaunchError, LaunchRequest, TerminalLauncher};
use devlanes_term_windows_terminal::WindowsTerminal;

fn open(request: &LaunchRequest) -> Result<(), LaunchError> {
    WindowsTerminal.launch(request)
}
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in a Windows Terminal tab.
terminal: windows-terminal
```

## Behaviour

It runs `wt.exe -w 0 new-tab --startingDirectory <cwd> [--title <title>] -- <command>`, which opens a tab in the most recently used window, or a new window when none is open. `wt.exe` exits once the running Terminal has the request, so a non-zero status is reported as a launch failure.

wt.exe splits its arguments into subcommands at every `;`, even inside a quoted word, so each `;` in the directory, title and command is passed as `\;`.

The request's `env` is set on `wt.exe`, and Windows Terminal hands its caller's environment to a command given on its command line (checked with Windows Terminal 1.24). That is how the TUI passes `DEVLANES_HOME`, because Windows has no `env` program.

`available()` looks for `wt.exe` on `PATH`, where Windows Terminal installs it as an App Execution Alias in `%LOCALAPPDATA%\Microsoft\WindowsApps`. Turning the alias off in Settings makes the launcher unavailable. Outside Windows it is never available.

## License

Apache-2.0
