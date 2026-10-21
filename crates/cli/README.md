# devlanes-cli

Command parsing, request building, and output rendering for the devlanes binary.
It builds `devlanes-proto` requests and delegates execution to the binary's handlers or injected drivers; it never talks to the daemon itself.

## Usage

```rust
use devlanes_cli::{clap::Parser, Action, Cli, Runtime, Signals};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::try_parse_from(["devlanes", "status", "--json"])?;
    let runtime = Runtime::resolve(&cli, Signals::detect())?;
    let action = cli.action(&runtime)?;

    assert!(matches!(action, Action::Rpc(_)));
    Ok(())
}
```

The caller executes the returned action and writes the reporter's separate stdout and stderr output.

## API

- `Cli` / `Action`: parse arguments and describe RPC or local work.
- `Runtime` / `Signals`: resolve output mode and caller attribution, with injectable detection signals.
- `Reporter`: human, plain, agent, and JSON rendering for results and events.
- `run_with` / `WithDriver`: start a lane, run a foreground command, collect artifacts, and clean up owned resources.
- `CiProvider`: phase groups, failure annotations, and step-summary appends.

## Behaviour

Output mode: the first matching setting wins: `--output`, `--json`, `DEVLANES_OUTPUT`, agent detection, CI detection, then terminal detection.
`DEVLANES_AGENT=0` disables agent detection. CI defaults to plain output and disables prompts.

Progress bar: in human output on a terminal, `start`, `restart`, `lane resume`, `lane new --start`, `wait` and the start phase of `with` drive the terminal tab's progress bar. `start_tracker` picks the lanes and units a request follows, and `Runtime::progress_wrap` decides whether to draw at all; the binary follows the events and writes it. A failed start stays red for `ERROR_HOLD` before the command exits, and the bar is cleared on every exit and on Ctrl-C.

## License

Apache-2.0
