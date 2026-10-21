# devlanes-term-ghostty

`TerminalLauncher` for Ghostty, selected by `terminal: ghostty`.

## Usage

```rust
use devlanes_term::{LaunchError, LaunchRequest, TerminalLauncher};
use devlanes_term_ghostty::Ghostty;

fn open(request: &LaunchRequest) -> Result<(), LaunchError> {
    Ghostty.launch(request)
}
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in Ghostty.
terminal: ghostty
```

## Behaviour

On macOS it runs `open -na Ghostty.app --args --working-directory=<cwd> --title=<title> -e <command>`, because Ghostty's CLI binary cannot open windows there. On Linux it runs `ghostty` with the same arguments.

## License

Apache-2.0
