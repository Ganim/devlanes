//! `docs/reference/cli.md`, generated from the clap command tree. The test fails
//! when the file is stale; `DEVLANES_BLESS=1` rewrites it.

use std::path::Path;

use clap::{Arg, Command, CommandFactory};
use devlanes_cli::Cli;

#[test]
fn cli_reference_is_current() {
    let mut command = Cli::command();
    command.build();
    let generated = render(&command);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/cli.md");
    if std::env::var("DEVLANES_BLESS").as_deref() == Ok("1") {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create docs/reference");
        }
        std::fs::write(&path, generated).expect("write the reference");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is stale; regenerate it with DEVLANES_BLESS=1 cargo nextest run -p devlanes-cli -E 'test(cli_reference_is_current)'",
        path.display()
    );
}

/// The whole page: the root help, global options once, then every command.
fn render(root: &Command) -> String {
    let mut page = String::from(
        "# CLI reference\n\n\
         <!-- Generated from the clap definitions in crates/cli by \
         crates/cli/tests/it/reference_docs.rs. Edit the help text in crates/cli, then run \
         DEVLANES_BLESS=1 cargo nextest run -p devlanes-cli. -->\n\n\
         Every `devlanes` command and option with its help text; `devlanes <command> --help` \
         prints the same.\n\n",
    );
    if let Some(about) = root.get_long_about().or(root.get_about()) {
        page.push_str(&format!("{}\n\n", about.to_string().trim()));
    }
    page.push_str(&format!("```text\n{}\n```\n\n", usage(root)));
    if let Some(after) = root.get_after_long_help().or(root.get_after_help()) {
        page.push_str(&format!("```text\n{}\n```\n\n", after.to_string().trim()));
    }
    page.push_str(
        "## Global options\n\nAccepted by every command, before or after the command name.\n\n",
    );
    for arg in root
        .get_arguments()
        .filter(|arg| shown(arg) && arg.is_global_set())
    {
        page.push_str(&argument(arg));
    }
    page.push_str("\n## Commands\n");
    for sub in visible(root) {
        command(&mut page, sub, "devlanes");
    }
    page
}

/// One `###` section per command, depth first in definition order.
fn command(page: &mut String, command: &Command, parent: &str) {
    let path = format!("{parent} {}", command.get_name());
    page.push_str(&format!("\n### {path}\n\n"));
    if let Some(about) = command.get_long_about().or(command.get_about()) {
        page.push_str(&format!("{}\n\n", about.to_string().trim()));
    }
    page.push_str(&format!("```text\n{}\n```\n", usage(command)));
    let arguments: Vec<&Arg> = command
        .get_arguments()
        .filter(|arg| shown(arg) && !arg.is_global_set())
        .collect();
    if !arguments.is_empty() {
        page.push('\n');
        for arg in arguments {
            page.push_str(&argument(arg));
        }
    }
    if let Some(after) = command.get_after_long_help().or(command.get_after_help()) {
        page.push_str(&format!("\n```text\n{}\n```\n", after.to_string().trim()));
    }
    for sub in visible(command) {
        self::command(page, sub, &path);
    }
}

/// Subcommands a user can see, without clap's `help`.
fn visible(command: &Command) -> impl Iterator<Item = &Command> {
    command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

/// Arguments a user can see, without `--help` and `--version`.
fn shown(arg: &Arg) -> bool {
    !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version")
}

/// The usage line without its `Usage: ` prefix.
fn usage(command: &Command) -> String {
    let mut command = command.clone();
    let text = command.render_usage().to_string();
    text.trim()
        .strip_prefix("Usage: ")
        .unwrap_or(text.trim())
        .to_owned()
}

/// A list item: the flag or positional, its help, possible values and default.
fn argument(arg: &Arg) -> String {
    let takes_value = arg.get_action().takes_values();
    let mut name = match (arg.get_short(), arg.get_long()) {
        (None, None) => {
            let value = value_name(arg);
            if arg.is_required_set() {
                format!("<{value}>")
            } else {
                format!("[{value}]")
            }
        }
        (short, long) => {
            let mut flags = Vec::new();
            if let Some(short) = short {
                flags.push(format!("-{short}"));
            }
            if let Some(long) = long {
                flags.push(format!("--{long}"));
            }
            let mut text = flags.join(", ");
            if takes_value {
                text.push_str(&format!(" <{}>", value_name(arg)));
            }
            text
        }
    };
    if arg
        .get_num_args()
        .is_some_and(|range| range.max_values() > 1)
    {
        name.push_str("...");
    }
    let help = arg
        .get_long_help()
        .or(arg.get_help())
        .map(|help| help.to_string())
        .unwrap_or_default();
    let mut item = format!("- `{name}`: {}", indent(help.trim()));
    if takes_value {
        let values: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|value| !value.is_hide_set())
            .map(|value| match value.get_help() {
                Some(help) => format!("`{}` ({})", value.get_name(), help),
                None => format!("`{}`", value.get_name()),
            })
            .collect();
        if !values.is_empty() {
            item.push_str(&format!("\n  Values: {}.", values.join(", ")));
        }
        let defaults: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|value| format!("`{}`", value.to_string_lossy()))
            .collect();
        if !defaults.is_empty() {
            item.push_str(&format!("\n  Default: {}.", defaults.join(", ")));
        }
    }
    item.push('\n');
    item
}

/// The value placeholder: clap's value name, or the argument id in capitals.
fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase())
}

/// Continuation lines indented under their list item; blank lines kept blank.
fn indent(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_owned()
}
