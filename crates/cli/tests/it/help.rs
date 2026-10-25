//! `--help` text: snapshots of the main pages and a check that nothing visible lacks help.

use clap::CommandFactory;
use devlanes_cli::Cli;

/// The built command tree, so globals and binary names are filled in.
fn built() -> clap::Command {
    let mut command = Cli::command();
    command.build();
    command
}

/// `devlanes <path…> --help` as plain text.
fn long_help(path: &[&str]) -> String {
    let mut command = built();
    let mut current = &mut command;
    for name in path {
        current = current
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("no subcommand {name}"));
    }
    current.render_long_help().to_string()
}

#[test]
fn main_help_pages() {
    for path in [
        &[][..],
        &["start"],
        &["logs"],
        &["with"],
        &["lane"],
        &["debug"],
        &["env"],
        &["url"],
        &["certs"],
        &["infra"],
        &["config"],
        &["remote"],
        &["daemon"],
    ] {
        let name = if path.is_empty() {
            "help_devlanes".to_owned()
        } else {
            format!("help_{}", path.join("_"))
        };
        insta::assert_snapshot!(name, long_help(path));
    }
}

/// Collects `path: problem` for every visible command without `about` and every
/// visible argument without `help`.
fn missing(command: &clap::Command, path: &str, problems: &mut Vec<String>) {
    for arg in command.get_arguments() {
        let id = arg.get_id().as_str();
        if arg.is_hide_set() || matches!(id, "help" | "version") {
            continue;
        }
        if arg.get_help().is_none() {
            problems.push(format!("{path}: argument {id} has no help"));
        }
    }
    for sub in command.get_subcommands() {
        if sub.is_hide_set() || sub.get_name() == "help" {
            continue;
        }
        let sub_path = format!("{path} {}", sub.get_name());
        if sub.get_about().is_none() {
            problems.push(format!("{sub_path}: no about"));
        }
        missing(sub, &sub_path, problems);
    }
}

#[test]
fn every_visible_command_and_argument_has_help() {
    let mut problems = Vec::new();
    missing(&built(), "devlanes", &mut problems);
    assert!(
        problems.is_empty(),
        "missing help:\n{}",
        problems.join("\n")
    );
}

#[test]
fn hidden_commands_stay_hidden() {
    let help = long_help(&[]);
    for hidden in ["tunnel", "askpass"] {
        assert!(
            !help.contains(&format!("  {hidden} ")),
            "{hidden} is listed"
        );
    }
    assert!(!long_help(&["daemon"]).contains("  run "));
    assert!(!help.contains("schema"), "schema is no longer a command");
}

/// Same shape as clap's own lines ("Print help"): capital first letter, no period.
#[test]
fn abouts_match_clap_style() {
    fn check(command: &clap::Command, problems: &mut Vec<String>) {
        for sub in command.get_subcommands().filter(|s| s.get_name() != "help") {
            if let Some(about) = sub.get_about().map(ToString::to_string) {
                let first = about.chars().next().unwrap_or_default();
                if first.is_lowercase() || about.ends_with('.') || about.contains('\u{2014}') {
                    problems.push(format!("{}: {about}", sub.get_name()));
                }
            }
            check(sub, problems);
        }
    }
    let mut problems = Vec::new();
    check(&built(), &mut problems);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
