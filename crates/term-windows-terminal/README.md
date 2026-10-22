# devlanes-term-windows-terminal

`TerminalLauncher` for Windows Terminal, selected by `terminal: windows-terminal`. `bins/devlanes` registers it on every platform; it is available on Windows and inside WSL, where the TUI's `O` key uses it.

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

It runs `wt.exe -w 0 new-tab --inheritEnvironment --startingDirectory <cwd> [--title <title>] -- <command>`, which opens a tab in the most recently used window, or a new window when none is open. `wt.exe` exits once the running Terminal has the request, so a non-zero status is reported as a launch failure.

wt.exe splits its arguments into subcommands at every `;`, even inside a quoted word, so each `;` in the directory, title and command is passed as `\;`.

Requires Windows Terminal 1.18 or later. The request's `env` is set on `wt.exe`, and since 1.18 a tab that `wt.exe` opens with `--inheritEnvironment` gets `wt.exe`'s environment, also in an existing window (checked with 1.24). Older versions do not have the flag. Passing the flag makes a profile's `compatibility.reloadEnvironmentVariables` irrelevant. That is how the TUI passes `DEVLANES_HOME`, because Windows has no `env` program. Before 1.25 the tab could miss the environment when `wt.exe` starts Windows Terminal itself (microsoft/terminal#16226).

`available()` looks for `wt.exe` on `PATH`, where Windows Terminal installs it as an App Execution Alias in `%LOCALAPPDATA%\Microsoft\WindowsApps`. Turning the alias off in Settings makes the launcher unavailable.

## Inside WSL

On Linux it is available only inside WSL with interop on and `wt.exe` on `PATH` (WSL appends the Windows `PATH` unless `appendWindowsPath=false`). The tab runs the command in the same distro:

```text
wt.exe -w 0 new-tab [--title <title>] -- wsl.exe [-d <distro>] --cd <linux cwd> --exec [env K=V...] <command>
```

`--exec` keeps the distro's shell from splitting the words again, the `--` keeps wt from reading `-d` and `--cd` as its own options, and `-d` comes from `WSL_DISTRO_NAME` (the default distro without it). The request's `env` goes through `env`, because Windows does not pass its environment into WSL. Elsewhere (macOS, Linux outside WSL) `invocation` returns `Unsupported`.

## License

Apache-2.0
