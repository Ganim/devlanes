//! What commands write to the terminal besides text: tab titles, the
//! notification at the end of a long start, links and section marks, and that
//! none of it reaches agent, JSON, plain, CI or piped output.

use std::time::Duration;

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{Event, Method, Request, RpcError};
use devlanes_term::{Features, Links, Marks, Mode, Settings};
use serde_json::{Value, json};

use crate::render::{lane, start_result, unit};

/// A runtime for `args` with stdout a terminal or not, and CI markers injected.
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

/// The request `args` builds, on plain output so starts stay out of the TUI.
fn request(args: &[&str]) -> Request {
    let mut full = vec!["devlanes"];
    full.extend(args);
    let runtime = runtime(&["devlanes", "--output", "plain"], false, false);
    let cli = Cli::parse_from(full);
    match cli.action_with(&runtime, "/work/acme-1".into(), Default::default) {
        Ok(Action::Rpc(request) | Action::Attach(request) | Action::StartUi { request, .. }) => {
            request
        }
        _ => panic!("{args:?} builds no request"),
    }
}

/// One daemon event.
fn event(name: &str, data: Value) -> Event {
    Event {
        seq: 1,
        event: name.into(),
        data,
    }
}

#[test]
fn long_commands_get_a_title_and_short_ones_none() {
    for (args, expected) in [
        (&["start"][..], "devlanes start"),
        (&["start", "api", "web"][..], "devlanes start api web"),
        (&["restart", "api"][..], "devlanes restart api"),
        (&["lane", "resume"][..], "devlanes lane resume"),
        (
            &["lane", "new", "feat", "--start"][..],
            "devlanes lane new feat",
        ),
        (&["wait", "api"][..], "devlanes wait api"),
        (&["logs", "-f", "api"][..], "devlanes logs -f api"),
        (&["attach", "api"][..], "devlanes attach api"),
        (&["start", "--lane", "acme@1"][..], "devlanes start acme@1"),
        (
            &["start", "a", "b", "c", "d", "e"][..],
            "devlanes start a b c +2",
        ),
    ] {
        let title = command_title(&request(args)).unwrap_or_else(|| panic!("{args:?}"));
        assert_eq!(title.text(), expected, "{args:?}");
    }
    for args in [
        &["status"][..],
        &["logs", "api"][..],
        &["stop"][..],
        &["url", "ls"][..],
        &["doctor"][..],
        &["lane", "new", "feat"][..],
        &["run", "test"][..],
    ] {
        assert!(command_title(&request(args)).is_none(), "{args:?}");
    }
}

#[test]
fn a_start_title_follows_its_events() {
    let mut title = command_title(&request(&["start"])).expect("start title");
    let phase = |phase: &str, state: &str| {
        event(
            "lane.phase",
            json!({"lane": "acme@1", "phase": phase, "state": state, "start_id": 3}),
        )
    };
    title.event(&phase("infra", "started"));
    assert_eq!(title.text(), "devlanes start acme@1 · infra");
    title.event(&phase("infra", "done"));
    let mut services = phase("services", "started");
    services.data["units"] = json!(["api", "web", "worker"]);
    title.event(&services);
    let changed = |name: &str, state: &str| {
        event(
            "unit.changed",
            json!({"unit": {"name": name, "lane": "acme@1", "state": state, "kind": "service",
                            "pid": null, "exit_code": null, "ports": {}, "urls": [],
                            "actor": {"kind": "human"}, "reason": null}}),
        )
    };
    title.event(&changed("api", "ready"));
    assert_eq!(title.text(), "devlanes start acme@1 · 1/3");
    title.event(&changed("web", "failed"));
    assert_eq!(title.text(), "devlanes start acme@1 · 1 failed");

    // The end of a long start; a quick one is not worth a notification.
    assert!(title.finished(false, Duration::from_secs(2)).is_none());
    assert_eq!(
        title.finished(false, Duration::from_secs(30)),
        Some(("start of acme@1 failed".into(), "1 failed".into()))
    );
    let mut ok = command_title(&request(&["start"])).expect("start title");
    ok.event(&services);
    for name in ["api", "web", "worker"] {
        ok.event(&changed(name, "ready"));
    }
    assert_eq!(
        ok.finished(true, Duration::from_secs(30)),
        Some(("acme@1 is up".into(), "3 of 3 ready".into()))
    );
    // A command that follows no start never notifies.
    let logs = command_title(&request(&["logs", "-f", "api"])).expect("logs title");
    assert!(logs.finished(true, Duration::from_secs(600)).is_none());
}

/// Every feature on, whatever the terminal.
fn always() -> Settings {
    Settings {
        title: Mode::Always,
        links: Mode::Always,
        notifications: Mode::Always,
        marks: Mode::Always,
        ..Settings::default()
    }
}

#[test]
fn only_human_output_on_a_terminal_gets_terminal_features() {
    let iterm = |key: &str| (key == "TERM_PROGRAM").then(|| "iTerm.app".to_owned());
    let human = runtime(&["devlanes"], true, false).detect_terminal(&Settings::default(), iterm);
    assert!(human.title && human.links.is_some() && human.marks.is_some());
    assert!(human.notifications.is_some());
    for (args, tty, ci) in [
        (&["devlanes", "--json"][..], true, false),
        (&["devlanes", "--output", "agent"][..], true, false),
        (&["devlanes", "--output", "plain"][..], true, false),
        (&["devlanes", "--output", "human"][..], false, false),
        (&["devlanes", "--output", "human"][..], true, true),
    ] {
        let features = runtime(args, tty, ci).detect_terminal(&always(), iterm);
        assert_eq!(features, Features::default(), "{args:?} tty={tty} ci={ci}");
    }
}

/// `runtime` with links and marks on, as the binary sets them.
fn featured(mut runtime: Runtime) -> Runtime {
    runtime.terminal = Features {
        links: Some(Links::new(true)),
        marks: Some(Marks::Osc133),
        ..Features::default()
    };
    runtime
}

/// `data` rendered for `method` by `runtime`.
fn rendered(runtime: &Runtime, method: Method, data: &Value) -> String {
    runtime
        .reporter()
        .result(&Report::new(method, data))
        .expect("render")
        .stdout
}

#[test]
fn human_output_links_urls_and_marks_sections() {
    let human = featured(runtime(&["devlanes"], true, false));
    let start = rendered(&human, Method::Start, &start_result());
    assert!(
        start.contains("\x1b]8;;https://api.acme-2.localhost:8443\x1b\\https://api.acme-2.localhost:8443\x1b]8;;\x1b\\"),
        "{start:?}"
    );
    // The failed unit's row starts a section the terminal can jump to.
    assert!(
        start.contains("\x1b]133;A\x1b\\✗\x1b]133;C\x1b\\  admin"),
        "{start:?}"
    );
    let status = json!({"items": [lane(Some(2), "feat", "running", vec![unit("api", "ready")])],
                        "truncated": false, "next_cursor": null});
    let status = rendered(&human, Method::Status, &status);
    assert!(
        status.contains("\x1b]8;;https://acme-2.localhost:8443\x1b\\"),
        "{status:?}"
    );
    let hosts = json!({
        "items": [{"host": "api.acme.localhost", "project": "acme", "lane": "acme@0", "unit": "api",
                   "port": 20001, "fallback": false, "state": "ready"}],
        "proxy": {"https": {"bound": true, "port": 443}, "http": {"bound": true, "port": 80}},
        "truncated": false, "next_cursor": null,
    });
    let hosts = rendered(&human, Method::Hosts, &hosts);
    assert!(
        hosts.contains("\x1b]8;;https://api.acme.localhost\x1b\\api.acme.localhost"),
        "{hosts:?}"
    );
    let logs = json!({"lane": "acme@0", "units": [
        {"unit": "api", "lines": [], "truncated": false},
        {"unit": "web", "lines": [], "truncated": false},
    ]});
    let logs = rendered(&human, Method::Logs, &logs);
    assert_eq!(logs.matches("\x1b]133;A\x1b\\== ").count(), 2, "{logs:?}");

    // The same runtime set to plain output prints none of it, and neither
    // does human output without the features.
    let mut plain = featured(runtime(&["devlanes", "--output", "plain"], true, false));
    plain.terminal.links = Some(Links::new(false));
    let bare = runtime(&["devlanes"], true, false);
    for (name, runtime) in [("plain", &plain), ("no features", &bare)] {
        for (method, data) in [
            (Method::Start, start_result()),
            (
                Method::Logs,
                json!({"lane": "acme@0", "units": [{"unit": "api", "lines": [], "truncated": false}]}),
            ),
        ] {
            let text = rendered(runtime, method, &data);
            assert!(!text.contains("\x1b]"), "{name} {method:?}: {text:?}");
        }
    }
}

#[test]
fn config_errors_link_the_file_at_its_line() {
    let error = RpcError::new(
        "config_error",
        json!({"message": "unknown field `prot`", "file": "/work/acme/devlanes.yaml",
               "line": 5, "column": 12}),
        1,
    );
    let human = featured(runtime(&["devlanes"], true, false));
    let stderr = human.reporter().error(&error, None).stderr;
    assert!(
        stderr.contains(
            "\x1b]8;;file:///work/acme/devlanes.yaml#5:12\x1b\\/work/acme/devlanes.yaml\x1b]8;;\x1b\\ is invalid at line 5, column 12"
        ),
        "{stderr:?}"
    );
    let plain = featured(runtime(&["devlanes", "--output", "plain"], false, false));
    assert!(!plain.reporter().error(&error, None).stderr.contains("\x1b"));
}
