//! `devlanes agent-guide` and the Codex snippet in `agent-plugin/` are one text.

use clap::Parser;
use devlanes_cli::{AGENT_GUIDE, Cli};

#[test]
fn codex_snippet_matches_the_agent_guide() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../agent-plugin/codex/AGENTS.md"
    );
    let snippet = std::fs::read_to_string(path).expect("read the Codex snippet");
    assert_eq!(
        snippet, AGENT_GUIDE,
        "copy crates/cli/agent-guide.md to agent-plugin/codex/AGENTS.md"
    );
}

/// Catches guide commands that a CLI change renamed or removed.
#[test]
fn every_guide_command_parses() {
    let commands: Vec<&str> = AGENT_GUIDE
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
