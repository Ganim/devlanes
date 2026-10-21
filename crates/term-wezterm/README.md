# devlanes-term-wezterm

`TerminalLauncher` for WezTerm, selected by `terminal: wezterm`.

## Usage

```rust
use devlanes_term::TerminalLauncher;
use devlanes_term_wezterm::Wezterm;

// `request` is a `devlanes_term::LaunchRequest`.
Wezterm.launch(&request)?;
```

## Config example

```yaml
# ~/.devlanes/config.yaml: open units from the TUI's O key in WezTerm.
terminal: wezterm
```

## Behaviour

It runs `wezterm start --cwd <cwd> -- <command>`, through `open -na WezTerm.app --args` on macOS. `wezterm start` has no title flag, so the request's title is ignored.

## License

Apache-2.0
