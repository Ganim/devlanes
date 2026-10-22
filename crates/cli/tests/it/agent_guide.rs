//! The Codex copy of the agent guide in `agent-plugin/` names commands that exist.

use clap::Parser;
use devlanes_cli::Cli;

/// Catches guide commands that a CLI change renamed or removed.
#[test]
fn every_guide_command_parses() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../agent-plugin/codex/AGENTS.md"
    );
    let guide = std::fs::read_to_string(path).expect("read the Codex guide");
    let commands: Vec<&str> = guide
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("devlanes "))
        .collect();
    assert!(commands.len() > 5, "found {commands:?}");
    for command in commands {
        let args = command.split_whitespace();
        if let Err(error) = Cli::try_parse_from(args) {
            panic!("`{command}` does not parse: {error}");
        }
    }
}
