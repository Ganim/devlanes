# devlanes-term-kitty

`TerminalLauncher` for kitty, selected by `terminal: kitty`.

## Usage

```rust
use devlanes_term::TerminalLauncher;
use devlanes_term_kitty::Kitty;

// `request` is a `devlanes_term::LaunchRequest`.
Kitty.launch(&request)?;
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in kitty.
terminal: kitty
```

## Behaviour

It runs `kitty --single-instance --directory <cwd> --title <title> <command>`, through `open -na kitty.app --args` on macOS. `--single-instance` opens the window in a running kitty when there is one.

## License

Apache-2.0
