# devlanes-term-ghostty

`TerminalLauncher` for Ghostty, selected by `terminal: ghostty`.

## Usage

```rust
use devlanes_term::TerminalLauncher;
use devlanes_term_ghostty::Ghostty;

// `request` is a `devlanes_term::LaunchRequest`.
Ghostty.launch(&request)?;
```

On macOS it runs `open -na Ghostty.app --args --working-directory=<cwd> --title=<title> -e <command>`, because Ghostty's CLI binary cannot open windows there. On Linux it runs `ghostty` with the same arguments.

## License

Apache-2.0
