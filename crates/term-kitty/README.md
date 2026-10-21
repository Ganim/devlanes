# devlanes-term-kitty

`TerminalLauncher` for kitty, selected by `terminal: kitty`.

## Usage

```rust
use devlanes_term::{LaunchError, LaunchRequest, TerminalLauncher};
use devlanes_term_kitty::Kitty;

fn open(request: &LaunchRequest) -> Result<(), LaunchError> {
    Kitty.launch(request)
}
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in kitty.
terminal: kitty
```

## Behaviour

It runs `kitty --directory <cwd> --title <title> <command>`, through `open -na kitty.app --args` on macOS. On Linux it adds `--single-instance`, which opens the window in a running kitty when there is one; `open -n` always starts a new process, so macOS goes without it.

## License

Apache-2.0
