//! Which starts run inside the TUI, and the ways to turn that off.

use std::{collections::BTreeMap, path::PathBuf};

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::Method;

/// A runtime for `args` with the given terminals, CI marker and `DEVLANES_NO_UI`.
fn runtime(cli: &Cli, stdout: bool, stdin: bool, ci: bool, no_ui: bool) -> Runtime {
    let signals = Signals {
        stdout_tty: stdout,
        stdin_tty: stdin,
        no_color: true,
        no_ui,
        ..Signals::default()
    }
    .with_ci_env(|key| (ci && key == "CI").then(|| "true".to_owned()));
    Runtime::resolve(cli, signals).expect("runtime")
}

/// What the command turns into on a human terminal, with `no_ui` set or not.
fn opens(args: &[&str], no_ui: bool) -> Option<bool> {
    let cli = Cli::parse_from(args);
    let runtime = runtime(&cli, true, true, false, no_ui);
    let action = cli
        .action_with(&runtime, PathBuf::from("/work/acme"), BTreeMap::new)
        .expect("action");
    match action {
        Action::StartUi { request, forced } => {
            assert_eq!(request.params["events"], true, "{args:?}");
            Some(forced)
        }
        _ => None,
    }
}

#[test]
fn starts_on_a_terminal_run_in_the_tui() {
    for args in [
        &["devlanes", "start"][..],
        &["devlanes", "start", "api", "--profile", "e2e"][..],
        &["devlanes", "restart", "api"][..],
        &["devlanes", "lane", "resume"][..],
        &["devlanes", "lane", "new", "feat", "--start"][..],
    ] {
        assert_eq!(opens(args, false), Some(false), "{args:?}");
    }
    // `--attach` asks for it explicitly, past DEVLANES_NO_UI and the config.
    assert_eq!(opens(&["devlanes", "start", "--attach"], false), Some(true));
    assert_eq!(opens(&["devlanes", "start", "--attach"], true), Some(true));
}

#[test]
fn other_commands_and_opt_outs_keep_the_cli() {
    for args in [
        &["devlanes", "start", "--no-ui"][..],
        &["devlanes", "restart", "api", "--no-ui"][..],
        &["devlanes", "lane", "resume", "--no-ui"][..],
        &["devlanes", "lane", "new", "feat", "--start", "--no-ui"][..],
        &["devlanes", "lane", "new", "feat"][..],
        &["devlanes", "wait", "api"][..],
        &["devlanes", "stop"][..],
        &["devlanes", "run", "migrate"][..],
        &["devlanes", "with", "--", "true"][..],
    ] {
        assert_eq!(opens(args, false), None, "{args:?}");
    }
    assert_eq!(opens(&["devlanes", "start"], true), None, "DEVLANES_NO_UI");
    assert!(Cli::try_parse_from(["devlanes", "start", "--attach", "--no-ui"]).is_err());
}

#[test]
fn only_a_person_at_a_terminal_gets_the_tui() {
    let start = |args: &[&str], stdout, stdin, ci| {
        let cli = Cli::parse_from(args);
        let runtime = runtime(&cli, stdout, stdin, ci, false);
        cli.start_in_ui(&runtime)
    };
    assert_eq!(
        start(&["devlanes", "start"], true, true, false),
        Some(false)
    );
    assert_eq!(
        start(&["devlanes", "start"], false, true, false),
        None,
        "piped stdout"
    );
    assert_eq!(
        start(&["devlanes", "start"], true, false, false),
        None,
        "piped stdin"
    );
    assert_eq!(start(&["devlanes", "start"], true, true, true), None, "CI");
    for mode in ["agent", "plain", "json"] {
        let args = ["devlanes", "--output", mode, "start", "--attach"];
        assert_eq!(start(&args, true, true, false), None, "{mode}");
    }
    // Agents keep the plain start request: no TUI, no prompt.
    let cli = Cli::parse_from(["devlanes", "--output", "agent", "start"]);
    let runtime = runtime(&cli, true, true, false, false);
    let action = cli
        .action_with(&runtime, PathBuf::from("/work/acme"), BTreeMap::new)
        .expect("action");
    assert!(matches!(action, Action::Rpc(request) if request.method == Method::Start));
}
