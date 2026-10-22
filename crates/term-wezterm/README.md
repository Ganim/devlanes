# devlanes-term-wezterm

`TerminalLauncher` for WezTerm, selected by `terminal: wezterm`.

## Usage

```rust
use devlanes_term::{LaunchError, LaunchRequest, TerminalLauncher};
use devlanes_term_wezterm::Wezterm;

fn open(request: &LaunchRequest) -> Result<(), LaunchError> {
    Wezterm.launch(request)
}
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in WezTerm.
terminal: wezterm
```

## Behaviour

It runs `wezterm start --cwd <cwd> -- <command>`, through `open -na WezTerm.app --args` on macOS. `wezterm start` has no title flag, so the request's title is ignored.

On Linux and Windows the request's `env` is set on the `wezterm` process, which hands it to the command. On Windows it adds `--always-new-process`, because a running WezTerm GUI would otherwise open the window with its own environment. On macOS `open` starts the app with launchd's environment, so `env` is ignored there.

## License

Apache-2.0
