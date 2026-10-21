//! When commands drive the terminal's progress bar.

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{Context, Event, Method, Request};
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

/// The request `args` builds in `runtime`.
fn built(args: &[&str], runtime: &Runtime) -> Request {
    let cli = Cli::parse_from(args);
    match cli.action_with(runtime, "/work/acme-1".into(), Default::default) {
        Ok(Action::Rpc(request)) => request,
        _ => panic!("{args:?} builds no request"),
    }
}

#[test]
fn starts_ask_for_their_lane_events() {
    let human = runtime(&["devlanes"], true, false);
    let agent = runtime(&["devlanes", "--output", "agent"], false, false);
    let piped = runtime(&["devlanes", "--output", "plain"], false, false);
    let events = |args: &[&str], runtime: &Runtime| built(args, runtime).params["events"] == true;
    for args in [
        &["devlanes", "start"][..],
        &["devlanes", "restart", "api"][..],
        &["devlanes", "lane", "resume"][..],
        &["devlanes", "lane", "new", "feat", "--start"][..],
    ] {
        assert!(events(args, &human), "{args:?}");
        assert!(events(args, &piped), "{args:?}");
        // Agent output prints no progress, so the events would be wasted.
        assert!(!events(args, &agent), "{args:?}");
    }
    assert!(!events(&["devlanes", "lane", "new", "feat"], &human));
    // `wait` streams them only for the bar.
    assert!(events(&["devlanes", "wait", "api"], &human));
    assert!(!events(&["devlanes", "wait", "api"], &piped));
}

/// What a human terminal shows for each of `events` while `method` runs.
fn shown(method: Method, events: &[(&str, Value)]) -> String {
    let reporter = runtime(&["devlanes"], true, false).reporter();
    events
        .iter()
        .map(|(name, data)| {
            let event = Event {
                seq: 1,
                event: (*name).into(),
                data: data.clone(),
            };
            reporter.event(method, &event).expect("event").stdout
        })
        .collect()
}

#[test]
fn a_start_prints_its_phase_lines_in_human_mode() {
    let phase = |phase: &str, state: &str| json!({"lane": "acme@1", "phase": phase, "state": state, "start_id": 4});
    let start = [
        ("lane.phase", phase("infra", "started")),
        ("lane.phase", phase("infra", "done")),
        ("lane.phase", phase("setup", "skipped")),
        ("lane.phase", phase("tasks", "started")),
        (
            "task",
            json!({"lane": "acme@1", "task": "migrate", "state": "done", "exit_code": 0}),
        ),
        ("lane.phase", phase("tasks", "done")),
        ("lane.phase", {
            let mut started = phase("services", "started");
            started["units"] = json!(["api"]);
            started
        }),
        (
            "unit.changed",
            json!({"unit": {"name": "api", "lane": "acme@1", "state": "ready"}}),
        ),
        ("lane.phase", phase("services", "done")),
    ];
    // The services line is the result's; task phases speak through their tasks.
    assert_eq!(
        shown(Method::Start, &start),
        "\u{2713} infra\n! setup skipped\n\u{2713} migrate\n"
    );
    // `wait` streams the same events for the bar and prints none of them.
    assert_eq!(shown(Method::Wait, &start), "");
}
