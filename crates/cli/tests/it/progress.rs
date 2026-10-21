//! When commands drive the terminal's progress bar.

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{Context, Method, Request};
use devlanes_term::{Progress, ProgressMode, Wrap};
use serde_json::{Value, json};

/// A runtime for `args`, with stdout a terminal or not and CI markers injected.
fn runtime(args: &[&str], tty: bool, ci: bool) -> Runtime {
    let cli = Cli::parse_from(args);
    let signals = Signals {
        stdout_tty: tty,
        stdin_tty: tty,
        no_color: true,
        ..Signals::default()
    }
    .with_ci_env(|key| (ci && key == "CI").then(|| "true".to_owned()));
    Runtime::resolve(&cli, signals).expect("runtime")
}

/// Ghostty's environment.
fn ghostty(key: &str) -> Option<String> {
    (key == "TERM_PROGRAM").then(|| "ghostty".to_owned())
}

#[test]
fn only_human_output_on_a_terminal_draws_the_bar() {
    let human = runtime(&["devlanes"], true, false);
    assert_eq!(human.mode, OutputMode::Human);
    assert_eq!(
        human.progress_wrap(ProgressMode::Auto, ghostty),
        Some(Wrap::Bare)
    );
    assert_eq!(human.progress_wrap(ProgressMode::Never, ghostty), None);
    for args in [
        &["devlanes", "--json"][..],
        &["devlanes", "--output", "agent"][..],
        &["devlanes", "--output", "plain"][..],
    ] {
        let other = runtime(args, true, false);
        assert_eq!(
            other.progress_wrap(ProgressMode::Always, ghostty),
            None,
            "{args:?}"
        );
    }
    // Piped, and CI even when a human terminal was forced.
    let piped = runtime(&["devlanes", "--output", "human"], false, false);
    assert_eq!(piped.progress_wrap(ProgressMode::Always, ghostty), None);
    let ci = runtime(&["devlanes", "--output", "human"], true, true);
    assert_eq!(ci.progress_wrap(ProgressMode::Always, ghostty), None);
}

/// A request as the CLI builds it.
fn request(method: Method, params: Value, lane: Option<&str>) -> Request {
    Request {
        id: 1,
        method,
        params,
        context: Context {
            cwd: "/work/acme-1".into(),
            lane: lane.map(str::to_owned),
            ..Context::default()
        },
    }
}

#[test]
fn starting_commands_follow_progress_and_others_do_not() {
    for (method, params) in [
        (Method::Start, json!({})),
        (Method::Restart, json!({"units": ["api"]})),
        (Method::Resume, json!({})),
        (Method::LaneNew, json!({"branch": "feat", "start": true})),
        (Method::Wait, json!({"target": "acme@1:api"})),
    ] {
        let tracker = start_tracker(&request(method, params, None));
        // No events yet: the bar spins until a share is known.
        assert_eq!(
            tracker.map(|t| t.progress()),
            Some(Progress::Indeterminate),
            "{method:?}"
        );
    }
    for (method, params) in [
        (Method::LaneNew, json!({"branch": "feat", "start": false})),
        (Method::Stop, json!({})),
        (Method::Status, json!({})),
        (Method::Logs, json!({})),
        (Method::Wait, json!({})),
    ] {
        assert!(
            start_tracker(&request(method, params, Some("acme@1"))).is_none(),
            "{method:?}"
        );
    }
}
