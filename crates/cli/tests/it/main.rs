use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use clap::{CommandFactory, Parser};
use devlanes_cli::*;
use devlanes_proto::{
    Actor, ActorKind, Event, ExecParams, LogLine, LogQuery, Method, Page, RpcError, Selection,
    UnitSummary,
};
use serde_json::{Value, json};

/// Fixed signals keep snapshots independent of the test runner's inherited agent markers.
fn runtime(cli: &Cli, mode: OutputMode) -> Runtime {
    Runtime::resolve(
        cli,
        Signals {
            env_output: Some(mode.as_str().into()),
            stdout_tty: mode == OutputMode::Human,
            stdin_tty: mode == OutputMode::Human,
            ..Signals::default()
        },
    )
    .expect("fixed runtime")
}

/// Test-only environment contents never include the invoking process's actual variables.
fn action(cli: &Cli, runtime: &Runtime) -> Action {
    cli.action_with(runtime, PathBuf::from("/fixture/worktree"), || {
        BTreeMap::from([("FIXTURE".into(), "yes".into())])
    })
    .expect("valid command action")
}

/// Unwraps RPC variants without requiring secret-bearing requests to implement Debug.
fn request(action: Action) -> devlanes_proto::Request {
    match action {
        Action::Rpc(r)
        | Action::RpcThenUi(r)
        | Action::RpcThenOfferStart(r)
        | Action::Attach(r)
        | Action::Open(r)
        | Action::ExportEnv { request: r, .. }
        | Action::Shell(r) => r,
        _ => panic!("expected request action"),
    }
}

#[test]
fn parser_definition_is_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn entire_command_inventory_builds_actions() {
    let cases: &[(&[&str], Option<Method>)] = &[
        (
            &["start", "2:api", "+web", "--profile", "dev", "--attach"],
            Some(Method::Start),
        ),
        (
            &["stop", "acme@feat/billing:api-*", "--mine"],
            Some(Method::Stop),
        ),
        (&["restart", "2:api"], Some(Method::Restart)),
        (
            &["status", "acme@2", "--watch", "--mine", "--timeout", "30s"],
            Some(Method::Status),
        ),
        (
            &[
                "logs",
                "acme@feat/checkout:+api",
                "--tail",
                "9",
                "-f",
                "--timeout",
                "2s",
                "--since",
                "5m",
                "--grep",
                "error",
                "--level",
                "warn",
                "--around",
                "api#2",
                "-C",
                "4",
            ],
            Some(Method::Logs),
        ),
        (&["debug", "errors", "--since", "5m"], Some(Method::Errors)),
        (&["wait", "acme@2", "--timeout", "1m"], Some(Method::Wait)),
        (&["debug", "why", "api"], Some(Method::Why)),
        (&["run", "migrate", "--detach", "--yes"], Some(Method::Run)),
        (&["run", "--list"], Some(Method::TaskList)),
        (
            &[
                "exec",
                "--name",
                "demo",
                "--",
                "echo",
                "--color",
                "hello world",
            ],
            Some(Method::Exec),
        ),
        (&["attach", "api"], Some(Method::Attach)),
        (&["url", "open", "api"], Some(Method::Url)),
        (&["url", "print", "api"], Some(Method::Url)),
        (&["url", "ls"], Some(Method::Hosts)),
        (&["env", "show", "--explain", "api"], Some(Method::Env)),
        (&["env", "export", "api"], Some(Method::Env)),
        (&["env", "shell", "api"], Some(Method::Env)),
        (&["debug", "top", "--json"], Some(Method::Metrics)),
        (&["lane", "resume"], Some(Method::Resume)),
        (&["lane", "ls"], Some(Method::LaneList)),
        (
            &["lane", "new", "feat/checkout", "--start"],
            Some(Method::LaneNew),
        ),
        (
            &["lane", "clean", "acme@2", "--yes"],
            Some(Method::LaneClean),
        ),
        (&["lane", "gc", "--yes"], Some(Method::LaneGc)),
        (&["infra", "up"], Some(Method::InfraUp)),
        (&["infra", "down"], Some(Method::InfraDown)),
        (&["infra", "status"], Some(Method::InfraStatus)),
        (&["remote", "sync"], Some(Method::SyncStatus)),
        (&["config", "check"], Some(Method::ConfigCheck)),
        (&["doctor"], Some(Method::Doctor)),
        (&["daemon", "status"], Some(Method::DaemonStatus)),
        (&["daemon", "stop"], Some(Method::DaemonShutdown)),
        (
            &["daemon", "reset-state", "--yes"],
            Some(Method::DaemonResetState),
        ),
        (
            &["remote", "connect", "box", "--background"],
            Some(Method::Connect),
        ),
        (&["remote", "disconnect", "box"], Some(Method::Disconnect)),
        (&["remote", "ls"], Some(Method::HostList)),
        (
            &["debug", "events", "-f", "--timeout", "30s"],
            Some(Method::Subscribe),
        ),
        (&["certs", "trust", "--hosts"], None),
        (&["certs", "untrust"], None),
        (&["certs", "untrust", "--hosts", "--keep-ca"], None),
        (&["daemon", "restart"], None),
        (&["daemon", "logs", "-f", "--tail", "10"], None),
        (&["daemon", "install"], None),
        (&["daemon", "uninstall"], None),
        (&["daemon", "run"], None),
        (&["ui"], None),
        (&["completions", "zsh"], None),
        (&["tunnel", "--stdio"], None),
        (&["askpass", "Password:"], None),
    ];
    for (args, method) in cases {
        let cli = Cli::try_parse_from(std::iter::once("devlanes").chain(args.iter().copied()))
            .expect("inventory parses");
        // Agents without a terminal cannot attach (agent_mode_bounds_streams_and_terminals).
        let mode = if args[0] == "attach" {
            OutputMode::Plain
        } else {
            OutputMode::Agent
        };
        let action = action(&cli, &runtime(&cli, mode));
        if let Some(method) = method {
            assert_eq!(request(action).method, *method, "{args:?}");
        }
    }
}

#[test]
fn positional_lanes_and_hosts_do_not_collide_with_global_flags() {
    for (args, lane) in [
        (vec!["status", "--lane", "acme@2"], "acme@2"),
        (vec!["status", "acme@3"], "acme@3"),
        (vec!["--lane", "acme@1", "status", "acme@4"], "acme@4"),
        (vec!["lane", "clean", "acme@5", "--yes"], "acme@5"),
    ] {
        let cli = Cli::try_parse_from(std::iter::once("devlanes").chain(args.clone()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"));
        let request = request(action(&cli, &runtime(&cli, OutputMode::Plain)));
        assert_eq!(request.context.lane.as_deref(), Some(lane), "{args:?}");
    }
    let cli = Cli::try_parse_from(["devlanes", "lane", "clean", "acme@5", "--lane", "acme@6"])
        .expect("clean with --lane");
    assert_eq!(
        request(action(&cli, &runtime(&cli, OutputMode::Plain))).params["lane"],
        "acme@5"
    );
    let cli = Cli::try_parse_from(["devlanes", "remote", "connect", "box"]).expect("connect");
    let request = request(action(&cli, &runtime(&cli, OutputMode::Plain)));
    assert_eq!(request.params["host"], "box");
    // Remote is mirroring only: no command runs on a box from here.
    assert!(Cli::try_parse_from(["devlanes", "status", "--host", "box"]).is_err());
}

/// Agents get bounded, terminal-free variants: streams need --timeout, attach points
/// at logs, and `start --attach` returns the start result without the TUI.
#[test]
fn agent_mode_bounds_streams_and_terminals() {
    let agent = |args: &[&str]| {
        let cli = Cli::parse_from(args);
        let runtime = runtime(&cli, OutputMode::Agent);
        cli.action_with(&runtime, PathBuf::from("/fixture"), BTreeMap::new)
    };
    for args in [
        &["devlanes", "status", "--watch"][..],
        &["devlanes", "debug", "events", "-f"],
    ] {
        let error = agent(args).err().expect("refused");
        assert!(error.to_string().contains("--timeout"), "{error}");
        let mut bounded = args.to_vec();
        bounded.extend(["--timeout", "30s"]);
        assert!(agent(&bounded).is_ok(), "{bounded:?}");
    }
    let error = agent(&["devlanes", "attach", "api"])
        .err()
        .expect("refused");
    assert!(
        error
            .to_string()
            .contains("devlanes logs api -f --timeout 30s"),
        "{error}"
    );
    assert!(matches!(
        agent(&["devlanes", "start", "--attach"]),
        Ok(Action::Rpc(_))
    ));
    let cli = Cli::parse_from(["devlanes", "start", "--attach"]);
    assert!(matches!(
        cli.action_with(
            &runtime(&cli, OutputMode::Human),
            PathBuf::new(),
            BTreeMap::new
        ),
        // A person at a terminal: the start runs inside the TUI.
        Ok(Action::StartUi { forced: true, .. })
    ));
}

#[test]
fn profile_repeats_into_an_ordered_list() {
    let cli = Cli::parse_from(["devlanes", "start", "--profile", "web", "--profile", "e2e"]);
    let params = request(action(&cli, &runtime(&cli, OutputMode::Plain))).params;
    assert_eq!(params["profiles"], json!(["web", "e2e"]));
    let cli = Cli::parse_from(["devlanes", "with", "--profile", "e2e", "--", "true"]);
    match action(&cli, &runtime(&cli, OutputMode::Plain)) {
        Action::With(plan) => assert_eq!(plan.start.params["profiles"], json!(["e2e"])),
        _ => panic!("expected with"),
    }
}

#[test]
fn all_at_once_reaches_every_starting_request() {
    for args in [
        vec!["start", "--all-at-once"],
        vec!["restart", "api", "--all-at-once"],
        vec!["lane", "resume", "--all-at-once"],
    ] {
        let cli = Cli::parse_from(std::iter::once("devlanes").chain(args.clone()));
        let request = request(action(&cli, &runtime(&cli, OutputMode::Plain)));
        assert_eq!(request.params["all_at_once"], true, "{args:?}");
    }
    let cli = Cli::parse_from(["devlanes", "start"]);
    assert_eq!(
        request(action(&cli, &runtime(&cli, OutputMode::Plain))).params["all_at_once"],
        false
    );
    let cli = Cli::parse_from(["devlanes", "with", "--all-at-once", "--", "true"]);
    match action(&cli, &runtime(&cli, OutputMode::Plain)) {
        Action::With(plan) => assert_eq!(plan.start.params["all_at_once"], true),
        _ => panic!("expected with"),
    }
}

#[test]
fn trust_and_untrust_carry_the_hosts_flag() {
    // (is trust, hosts, resolver, keep_ca)
    for (args, expected) in [
        (vec!["trust"], (true, false, false, false)),
        (vec!["trust", "--hosts"], (true, true, false, false)),
        (vec!["trust", "--resolver"], (true, false, true, false)),
        (vec!["untrust"], (false, false, false, false)),
        (vec!["untrust", "--hosts"], (false, true, false, false)),
        (
            vec!["untrust", "--hosts", "--keep-ca"],
            (false, true, false, true),
        ),
        (
            vec!["untrust", "--resolver", "--keep-ca"],
            (false, false, true, true),
        ),
        (
            vec!["untrust", "--hosts", "--resolver"],
            (false, true, true, false),
        ),
    ] {
        let cli = Cli::parse_from(["devlanes", "certs"].into_iter().chain(args));
        let found = match action(&cli, &runtime(&cli, OutputMode::Plain)) {
            Action::Trust { hosts, resolver } => (true, hosts, resolver, false),
            Action::Untrust {
                hosts,
                resolver,
                keep_ca,
            } => (false, hosts, resolver, keep_ca),
            _ => panic!("expected a trust action"),
        };
        assert_eq!(found, expected);
    }
    // Keeping the CA only makes sense when something else is being removed.
    assert!(Cli::try_parse_from(["devlanes", "certs", "untrust", "--keep-ca"]).is_err());
}

#[test]
fn output_precedence_and_attribution_are_deterministic() {
    let actor = Actor {
        kind: ActorKind::Agent,
        agent_id: Some("codex".into()),
        session_id: Some("thread".into()),
    };
    let signals = Signals {
        env_output: Some("plain".into()),
        stdout_tty: true,
        stdin_tty: true,
        detected_actor: Some(actor.clone()),
        ..Signals::default()
    };
    let cli = Cli::parse_from(["devlanes", "--json", "--output", "human"]);
    let resolved = Runtime::resolve(&cli, signals.clone()).expect("mode");
    assert_eq!(
        (resolved.mode, resolved.reason),
        (OutputMode::Human, "--output")
    );
    assert_eq!(resolved.actor, actor);
    let cli = Cli::parse_from(["devlanes", "--json"]);
    assert_eq!(
        Runtime::resolve(&cli, signals.clone()).expect("mode").mode,
        OutputMode::Json
    );
    let cli = Cli::parse_from(["devlanes"]);
    assert_eq!(
        Runtime::resolve(&cli, signals.clone()).expect("mode").mode,
        OutputMode::Plain
    );
    let signals = Signals {
        env_output: None,
        ..signals
    };
    assert_eq!(
        Runtime::resolve(&cli, signals.clone()).expect("mode").mode,
        OutputMode::Agent
    );
    let resolved = Runtime::resolve(
        &cli,
        Signals {
            agent_disabled: true,
            ..signals
        },
    )
    .expect("mode");
    assert_eq!(resolved.mode, OutputMode::Human);
    assert_eq!(resolved.actor, Actor::default());
    assert!(
        Runtime::resolve(
            &cli,
            Signals {
                env_output: Some("broken".into()),
                ..Signals::default()
            }
        )
        .is_err()
    );
    let cli = Cli::parse_from(["devlanes", "--output", "plain"]);
    assert!(
        Runtime::resolve(
            &cli,
            Signals {
                env_output: Some("broken".into()),
                ..Signals::default()
            }
        )
        .is_ok()
    );
}

#[test]
fn a_console_without_escape_sequences_gets_plain_output() {
    let signals = Signals {
        stdout_tty: true,
        stdin_tty: true,
        no_escapes: true,
        ..Signals::default()
    };
    let resolved = Runtime::resolve(&Cli::parse_from(["devlanes"]), signals.clone()).expect("mode");
    assert_eq!(
        (resolved.mode, resolved.reason, resolved.color),
        (
            OutputMode::Plain,
            "the console cannot render escape sequences",
            false
        )
    );
    assert!(!resolved.can_open_ui());
    // An explicit choice still wins, without color.
    let cli = Cli::parse_from(["devlanes", "--output", "human"]);
    let resolved = Runtime::resolve(&cli, signals.clone()).expect("mode");
    assert_eq!((resolved.mode, resolved.color), (OutputMode::Human, false));
    let forced = Signals {
        force_color: true,
        ..signals
    };
    let resolved = Runtime::resolve(&Cli::parse_from(["devlanes"]), forced).expect("mode");
    assert!(!resolved.color);
}

#[test]
fn environment_is_captured_only_for_relevant_methods() {
    for (args, capture) in [
        (vec!["start"], true),
        (vec!["restart", "api"], true),
        (vec!["run", "migrate"], true),
        (vec!["exec", "--name", "demo", "--", "echo"], true),
        (vec!["lane", "resume"], true),
        (vec!["run", "--list"], false),
        (vec!["env", "show"], false),
        (vec!["status"], false),
        (vec!["lane", "new", "feature", "--start"], false),
    ] {
        let cli = Cli::parse_from(std::iter::once("devlanes").chain(args));
        let called = std::cell::Cell::new(false);
        let action = cli
            .action_with(
                &runtime(&cli, OutputMode::Plain),
                PathBuf::from("/fixture"),
                || {
                    called.set(true);
                    BTreeMap::new()
                },
            )
            .expect("action");
        assert_eq!(called.get(), capture);
        assert_eq!(request(action).context.env.is_some(), capture);
    }
}

#[test]
fn typed_params_preserve_opaque_refs_argv_and_context() {
    let cli = Cli::parse_from([
        "devlanes",
        "--project",
        "acme",
        "--lane",
        "feat/checkout",
        "--yes",
        "start",
        "2:api-*",
        "+web",
        "--env",
        "X=first",
        "--env",
        "X=last=part",
    ]);
    let req = request(action(&cli, &runtime(&cli, OutputMode::Agent)));
    let params: Selection = serde_json::from_value(req.params).expect("typed selection");
    assert_eq!(params.units, ["2:api-*", "+web"]);
    assert_eq!(params.env["X"], "last=part");
    assert_eq!(params.timeout_ms, Some(180_000));
    assert_eq!(req.context.lane.as_deref(), Some("feat/checkout"));
    assert_eq!(req.context.project.as_deref(), Some("acme"));
    assert!(req.context.yes);
    let cli = Cli::parse_from([
        "devlanes",
        "exec",
        "--name",
        "demo",
        "--no-pty",
        "--",
        "echo",
        "$(unsafe)",
        "hello world",
    ]);
    let params: ExecParams =
        serde_json::from_value(request(action(&cli, &runtime(&cli, OutputMode::Plain))).params)
            .expect("exec params");
    assert_eq!(params.command, ["echo", "$(unsafe)", "hello world"]);
    assert!(!params.pty);
    let cli = Cli::parse_from(["devlanes", "logs", "acme@feat/checkout:+api"]);
    let params: LogQuery =
        serde_json::from_value(request(action(&cli, &runtime(&cli, OutputMode::Agent))).params)
            .expect("log query");
    assert_eq!(params.tail, Some(100));
    assert_eq!(params.max_bytes, Some(16 * 1024));
    assert_eq!(params.units, ["acme@feat/checkout:+api"]);
    assert!(!params.styled, "agents get no colors");
    let cli = Cli::parse_from(["devlanes", "logs", "api", "--head", "5"]);
    let params: LogQuery =
        serde_json::from_value(request(action(&cli, &runtime(&cli, OutputMode::Agent))).params)
            .expect("log query");
    assert_eq!((params.head, params.tail), (Some(5), None));
    for conflicting in [["--tail", "5"], ["-f", "--timeout=1s"]] {
        let args = ["devlanes", "logs", "api", "--head", "5"]
            .into_iter()
            .chain(conflicting);
        assert!(Cli::try_parse_from(args).is_err(), "{conflicting:?}");
    }
}

#[test]
fn defaults_follow_guards_and_interrupt_policy() {
    let cli = Cli::parse_from(["devlanes"]);
    assert!(matches!(
        action(&cli, &runtime(&cli, OutputMode::Human)),
        Action::Ui
    ));
    assert_eq!(
        request(action(&cli, &runtime(&cli, OutputMode::Agent))).method,
        Method::Status
    );
    assert_eq!(
        request(action(&cli, &runtime(&cli, OutputMode::Plain))).method,
        Method::Status
    );
    let cli = Cli::parse_from(["devlanes", "logs", "api", "-f"]);
    assert!(
        cli.action_with(
            &runtime(&cli, OutputMode::Agent),
            PathBuf::new(),
            BTreeMap::new
        )
        .is_err()
    );
    let cli = Cli::parse_from(["devlanes", "run", "migrate"]);
    assert_eq!(
        cli.interrupt_policy(&runtime(&cli, OutputMode::Agent)),
        InterruptPolicy::StopTask
    );
    assert_eq!(
        cli.interrupt_policy(&runtime(&cli, OutputMode::Human)),
        InterruptPolicy::ConfirmTaskStop
    );
    assert_eq!(
        cli.interrupt_policy(&runtime(&cli, OutputMode::Plain)),
        InterruptPolicy::Detach
    );
    let cli = Cli::parse_from(["devlanes", "start"]);
    assert_eq!(
        cli.interrupt_policy(&runtime(&cli, OutputMode::Agent)),
        InterruptPolicy::Detach
    );
    assert!(!runtime(&cli, OutputMode::Agent).can_prompt());
    assert!(
        runtime(&cli, OutputMode::Agent)
            .require_confirmation(false)
            .is_err()
    );
    assert!(
        runtime(&cli, OutputMode::Agent)
            .require_confirmation(true)
            .is_ok()
    );
    let cli = Cli::parse_from(["devlanes", "url", "open", "api"]);
    assert!(matches!(
        action(&cli, &runtime(&cli, OutputMode::Agent)),
        Action::Rpc(_)
    ));
}

#[test]
fn malformed_inputs_are_usage_errors() {
    // `restart`, `run` and `logs` alone are not malformed: they offer a choice
    // (tests/it/choose.rs).
    for args in [
        vec!["exec", "--name", "x"],
        vec!["run", "task", "--list"],
        vec!["start", "--env", "missing-equals"],
        vec!["--timeout", "0"],
        vec!["--timeout", "18446744073709551615h"],
        vec!["--budget", "12"],
    ] {
        let error =
            Cli::try_parse_from(std::iter::once("devlanes").chain(args)).expect_err("invalid args");
        assert_eq!(error.exit_code(), 2);
    }
    let cli = Cli::parse_from(["devlanes", "stop", "--env", "X=1"]);
    assert!(
        cli.action_with(
            &runtime(&cli, OutputMode::Plain),
            PathBuf::new(),
            BTreeMap::new
        )
        .is_err()
    );
}

/// Fake API responses use stable identifiers and no captured client environment.
fn unit(name: &str, state: &str) -> UnitSummary {
    UnitSummary {
        name: name.into(),
        lane: "acme@2".into(),
        state: state.into(),
        pid: Some(42),
        exit_code: (state == "failed").then_some(1),
        ports: BTreeMap::new(),
        urls: if state == "ready" {
            vec!["https://api.acme-2.localhost".into()]
        } else {
            vec![]
        },
        actor: Actor::default(),
        reason: None,
        kind: "service".into(),
        autostart: true,
        ..UnitSummary::default()
    }
}

#[test]
fn reporter_mode_snapshots_preserve_json_and_prioritize_agent_failures() {
    let cli = Cli::parse_from(["devlanes", "status"]);
    let mut failed = serde_json::to_value(unit("worker", "failed")).expect("serialize");
    failed["log_tail"] = json!(["\u{1b}[31mCannot find module\u{1b}[0m", "at app/main.ts:4"]);
    let data = json!({"items": [unit("api", "ready"), unit("web", "idle")], "truncated": false, "next_cursor": null});
    let mut data = data;
    data["items"].as_array_mut().expect("items").push(failed);
    let mut outputs = BTreeMap::new();
    for mode in [
        OutputMode::Human,
        OutputMode::Plain,
        OutputMode::Agent,
        OutputMode::Json,
    ] {
        let result = runtime(&cli, mode)
            .reporter()
            .result(&Report::new(Method::Status, &data))
            .expect("render");
        assert_eq!(result.exit_code, 1);
        if mode == OutputMode::Json {
            assert_eq!(
                serde_json::from_str::<Value>(&result.stdout).expect("JSON"),
                data
            );
        }
        outputs.insert(mode.as_str(), result.stdout);
    }
    insta::assert_json_snapshot!("reporter_modes", outputs);
}

#[test]
fn log_shaping_snapshot_and_utf8_budget() {
    let cli = Cli::parse_from(["devlanes", "logs", "acme@2:api", "--budget", "1024"]);
    let texts = [
        "\x1b[31merror: failure\x1b[0m".into(),
        "repeat".into(),
        "repeat".into(),
        " at /node_modules/pkg/index.js:3".into(),
        " at /node_modules/pkg/index.js:4".into(),
        "界".repeat(400),
    ];
    let lines: Vec<_> = texts
        .into_iter()
        .enumerate()
        .map(|(i, text)| LogLine {
            unit: "acme@2:api".into(),
            seq: i as u64,
            timestamp_ms: 1,
            text,
            level: None,
            styled: None,
        })
        .collect();
    let data = serde_json::to_value(Page::complete(lines)).expect("serialize");
    let rendered = runtime(&cli, OutputMode::Agent)
        .reporter()
        .result(&Report::new(Method::Logs, &data).with_more(Hint::command([
            "logs",
            "acme@2:api",
            "--output",
            "json",
            "--tail",
            "200",
        ])))
        .expect("render");
    assert!(rendered.stdout.len() <= 1024);
    assert!(!rendered.stdout.contains('\x1b'));
    assert!(rendered.stdout.contains("(x2)"));
    assert!(rendered.stdout.contains("2 node_modules frames folded"));
    assert!(rendered.stdout.contains("truncated; more: devlanes logs"));
    insta::assert_snapshot!("agent_logs", rendered.stdout);
}

#[test]
fn errors_are_stderr_in_every_mode_and_keep_exit_status() {
    let cli = Cli::parse_from(["devlanes", "start"]);
    let error = RpcError::new(
        "port_in_use",
        json!({"port": 20000, "pid": 42, "context": ["api could not bind"]}),
        3,
    );
    for mode in [
        OutputMode::Human,
        OutputMode::Plain,
        OutputMode::Agent,
        OutputMode::Json,
    ] {
        let result = runtime(&cli, mode)
            .reporter()
            .error(&error, Some(&Hint::command(["debug", "why", "acme@2:api"])));
        assert!(result.stdout.is_empty());
        assert_eq!(result.exit_code, 3);
        assert_eq!(
            result.stderr,
            "error: port 20000 is taken by pid 42\n  api could not bind\nhint: devlanes debug why acme@2:api\n"
        );
    }
}

#[test]
fn ndjson_events_and_agent_progress() {
    let cli = Cli::parse_from(["devlanes", "start", "--json"]);
    let event = Event {
        seq: 7,
        event: "unit.ready".into(),
        data: json!({"unit": "api"}),
    };
    let reporter = runtime(&cli, OutputMode::Json).reporter();
    let rendered = reporter.event(Method::Start, &event).expect("event");
    assert_eq!(rendered.stdout.lines().count(), 1);
    insta::assert_snapshot!(rendered.stdout, @"{\"seq\":7,\"event\":\"unit.ready\",\"data\":{\"unit\":\"api\"}}\n");
    let cli = Cli::parse_from(["devlanes", "start"]);
    let reporter = runtime(&cli, OutputMode::Agent).reporter();
    assert!(
        reporter
            .event(Method::Start, &event)
            .expect("event")
            .stdout
            .is_empty()
    );
    assert!(
        !reporter
            .event(Method::Subscribe, &event)
            .expect("event")
            .stdout
            .is_empty()
    );
}

#[test]
fn shell_exports_and_hints_are_injection_safe() {
    let values = BTreeMap::from([
        ("EMPTY".into(), "".into()),
        ("MESSAGE".into(), "it's $(touch /bad)".into()),
    ]);
    insta::assert_snapshot!(export_environment(&values, ExportShell::Posix).script, @r#"
    export EMPTY=''
    export MESSAGE='it'"'"'s $(touch /bad)'
    "#);
    // A name a POSIX shell cannot take is skipped, the rest still exported.
    let exported = export_environment(
        &BTreeMap::from([
            ("X;bad".into(), "y".into()),
            ("NEXT-PUBLIC".into(), "z".into()),
            ("OK".into(), "1".into()),
        ]),
        ExportShell::Posix,
    );
    assert_eq!(exported.script, "export OK=1\n");
    assert_eq!(exported.skipped, ["NEXT-PUBLIC", "X;bad"]);
    assert_eq!(
        Hint::logs("acme@2", "api-*", 200).0,
        "devlanes logs 'acme@2:api-*' --tail 200"
    );
}

#[test]
fn powershell_exports_are_ascii_literals_or_base64() {
    let values = BTreeMap::from([
        ("EMPTY".into(), "".into()),
        ("QUOTES".into(), "it's ''$(Write-Output bad)'' `n".into()),
        ("PORT".into(), "3000".into()),
        // Printable ASCII only: control characters and anything else go to Base64.
        ("LINES".into(), "a\nb".into()),
        ("TAB".into(), "a\tb".into()),
        ("SMART".into(), "a\u{2019}; calc".into()),
        ("JAPAN".into(), "日本".into()),
        // Names `$env:NAME` cannot spell go through `Set-Item -LiteralPath`.
        ("NEXT-PUBLIC".into(), "z".into()),
        ("EMPTY-DASH".into(), "".into()),
        ("1ST".into(), "x".into()),
        ("A}B".into(), "x".into()),
        ("CAFÉ".into(), "x".into()),
        // Names and values Windows cannot hold are skipped.
        ("A=B".into(), "x".into()),
        ("NUL".into(), "a\0b".into()),
    ]);
    let exported = export_environment(&values, ExportShell::Pwsh);
    insta::assert_snapshot!(exported.script, @r"
    Set-Item -LiteralPath ('Env:1ST') -Value ('x')
    Set-Item -LiteralPath ('Env:A}B') -Value ('x')
    Set-Item -LiteralPath ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('RW52OkNBRsOJ'))) -Value ('x')
    $env:EMPTY = ''
    Set-Item -LiteralPath ('Env:EMPTY-DASH') -Value ('')
    $env:JAPAN = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('5pel5pys'))
    $env:LINES = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('YQpi'))
    Set-Item -LiteralPath ('Env:NEXT-PUBLIC') -Value ('z')
    $env:PORT = '3000'
    $env:QUOTES = 'it''s ''''$(Write-Output bad)'''' `n'
    $env:SMART = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('YeKAmTsgY2FsYw=='))
    $env:TAB = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('YQli'))
    ");
    assert!(exported.script.is_ascii());
    assert_eq!(exported.skipped, ["A=B", "NUL"]);
    // POSIX still skips every name `export` rejects.
    assert_eq!(
        export_environment(&values, ExportShell::Posix).skipped,
        [
            "1ST",
            "A=B",
            "A}B",
            "CAFÉ",
            "EMPTY-DASH",
            "NEXT-PUBLIC",
            "NUL"
        ]
    );
}

#[test]
fn env_export_takes_a_shell_defaulting_to_posix() {
    for (args, expected) in [
        (
            &["devlanes", "env", "export", "api"][..],
            ExportShell::Posix,
        ),
        (
            &["devlanes", "env", "export", "api", "--shell", "posix"][..],
            ExportShell::Posix,
        ),
        (
            &["devlanes", "env", "export", "--shell", "pwsh"][..],
            ExportShell::Pwsh,
        ),
    ] {
        let cli = Cli::try_parse_from(args).expect("parses");
        match action(&cli, &runtime(&cli, OutputMode::Plain)) {
            Action::ExportEnv { request, shell } => {
                assert_eq!(shell, expected, "{args:?}");
                assert_eq!(request.method, Method::Env);
            }
            _ => panic!("expected an env export action for {args:?}"),
        }
    }
    assert!(Cli::try_parse_from(["devlanes", "env", "export", "--shell", "fish"]).is_err());
}

#[test]
fn confirmation_hints_repeat_the_refused_command_with_the_flag() {
    for (args, hint) in [
        (vec!["lane", "clean", "1"], "devlanes lane clean 1 --yes"),
        (
            vec!["daemon", "reset-state"],
            "devlanes daemon reset-state --yes",
        ),
        (vec!["run", "reset-db"], "devlanes run reset-db --yes"),
        (vec!["lane", "gc", "--yes"], "devlanes lane gc --yes"),
        (
            vec!["with", "--", "pnpm", "test", "--yes"],
            "devlanes with --yes -- pnpm test --yes",
        ),
        (
            vec!["--lane", "acme@feat/x", "infra", "down"],
            "devlanes --lane acme@feat/x infra down --yes",
        ),
    ] {
        assert_eq!(Hint::rerun_with(&args, "--yes").0, hint, "{args:?}");
    }
}

#[test]
fn quiet_color_and_human_ownership_note() {
    let cli = Cli::parse_from(["devlanes", "stop", "--quiet"]);
    let data =
        serde_json::to_value(Page::complete(vec![unit("api", "stopped")])).expect("serialize");
    assert!(
        runtime(&cli, OutputMode::Plain)
            .reporter()
            .result(&Report::new(Method::Stop, &data))
            .expect("render")
            .stdout
            .is_empty()
    );
    let cli = Cli::parse_from(["devlanes", "stop"]);
    let agent_caller = Runtime::resolve(
        &cli,
        Signals {
            detected_actor: Some(Actor {
                kind: ActorKind::Agent,
                agent_id: Some("codex".into()),
                session_id: Some("thread".into()),
            }),
            ..Signals::default()
        },
    )
    .expect("runtime");
    assert_eq!(agent_caller.mode, OutputMode::Agent);
    let rendered = agent_caller
        .reporter()
        .result(&Report::new(Method::Stop, &data))
        .expect("render");
    assert_eq!(rendered.stderr, "note: affected units started by a human\n");
    // DEVLANES_OUTPUT=agent with DEVLANES_AGENT=0: a human stopping their own units.
    let rendered = runtime(&cli, OutputMode::Agent)
        .reporter()
        .result(&Report::new(Method::Stop, &data))
        .expect("render");
    assert!(rendered.stderr.is_empty(), "{}", rendered.stderr);
    // An agent restarting its own units gets no note either.
    let mut own = unit("api", "ready");
    own.actor = agent_caller.actor.clone();
    let restart = json!({"lane": "acme@2", "ready": true, "units": [own]});
    assert!(
        agent_caller
            .reporter()
            .result(&Report::new(Method::Restart, &restart))
            .expect("render")
            .stderr
            .is_empty()
    );
    let signals = Signals {
        stdout_tty: true,
        force_color: true,
        no_color: true,
        ..Signals::default()
    };
    assert!(!Runtime::resolve(&cli, signals).expect("runtime").color);
}

#[test]
fn follow_budget_is_shared_across_events() {
    let cli = Cli::parse_from([
        "devlanes",
        "logs",
        "api",
        "-f",
        "--timeout",
        "1s",
        "--budget",
        "256",
    ]);
    let reporter = runtime(&cli, OutputMode::Agent).reporter();
    let mut stream = ReporterStream::new(
        reporter.as_ref(),
        Method::Logs,
        Hint::command(["logs", "api", "--cursor", "api#1", "--output", "json"]),
    );
    let mut combined = String::new();
    for seq in 0..100 {
        let event = log_event(seq, "long line of process output");
        combined.push_str(&stream.event(&event).expect("stream event").stdout);
    }
    assert!(stream.exhausted());
    assert!(combined.len() <= 256, "{}", combined.len());
    assert_eq!(combined.matches("truncated;").count(), 1);
}

/// A followed line exactly as the daemon sends it: `{unit, line: LogLine}`.
fn log_event(seq: u64, text: &str) -> Event {
    let line = LogLine {
        unit: "api".into(),
        seq,
        timestamp_ms: 1,
        text: text.into(),
        level: None,
        styled: None,
    };
    Event {
        seq,
        event: devlanes_proto::events::LOG.into(),
        data: json!({"unit": "api", "line": line}),
    }
}

#[test]
fn foreground_run_output_streams_within_the_budget_and_keeps_the_hidden_tail() {
    let cli = Cli::parse_from(["devlanes", "run", "test", "--budget", "512"]);
    let agent = runtime(&cli, OutputMode::Agent).reporter();
    // A task's progress events stay silent for agents; its output does not.
    let progress = Event {
        seq: 1,
        event: devlanes_proto::events::TASK.into(),
        data: json!({"lane": "acme@2", "task": "test", "state": "started"}),
    };
    assert!(
        agent
            .event(Method::Run, &progress)
            .expect("event")
            .stdout
            .is_empty()
    );
    let mut stream = ReporterStream::new(
        agent.as_ref(),
        Method::Run,
        Hint::command(["logs", "test", "--tail", "500"]),
    )
    .reserve(128);
    let mut printed = String::new();
    for seq in 0..60 {
        printed.push_str(
            &stream
                .event(&log_event(seq, &format!("case {seq} ok")))
                .expect("event")
                .stdout,
        );
    }
    assert!(stream.exhausted());
    assert!(printed.len() <= 512 - 128, "{}", printed.len());
    assert!(printed.starts_with("api#0 case 0 ok\n"));
    assert!(printed.contains("truncated; more: devlanes logs test --tail 500"));
    let tail = stream.hidden_tail();
    assert_eq!(tail.len(), 20);
    assert_eq!(tail.last().map(String::as_str), Some("case 59 ok"));
    // The summary shows the hidden end of a failed task's output.
    let data = json!({"task": "test", "lane": "acme@2", "unit": "test", "interactive": false,
        "exit_code": 1, "state": "failed", "log_tail": tail});
    let summary = agent
        .result(&Report::new(Method::Run, &data))
        .expect("summary");
    assert!(
        summary
            .stdout
            .starts_with("failed acme@2:test exit=1\n  | case 40 ok\n"),
        "{}",
        summary.stdout
    );
    assert_eq!(summary.exit_code, 1);
    // Plain mode streams every line.
    let plain = runtime(&cli, OutputMode::Plain).reporter();
    assert_eq!(
        plain
            .event(Method::Run, &log_event(3, "listening"))
            .expect("event")
            .stdout,
        "api#3 listening\n"
    );
}

#[test]
fn followed_log_events_render_their_line() {
    let cli = Cli::parse_from(["devlanes", "logs", "api", "-f", "--timeout", "1s"]);
    for mode in [OutputMode::Human, OutputMode::Plain, OutputMode::Agent] {
        let reporter = runtime(&cli, mode).reporter();
        let mut stream = ReporterStream::new(
            reporter.as_ref(),
            Method::Logs,
            Hint::command(["logs", "api"]),
        );
        let rendered = stream.event(&log_event(3, "listening")).expect("event");
        assert_eq!(rendered.stdout, "api#3 listening\n", "{mode:?}");
    }
    let json = runtime(&cli, OutputMode::Json)
        .reporter()
        .event(Method::Logs, &log_event(3, "listening"))
        .expect("json");
    let value: Value = serde_json::from_str(&json.stdout).expect("ndjson");
    assert_eq!(value["data"]["line"]["text"], "listening");
}

#[test]
fn failure_tail_and_long_lines_have_explicit_limits() {
    let cli = Cli::parse_from(["devlanes", "start"]);
    let reporter = runtime(&cli, OutputMode::Agent).reporter();
    let mut data = serde_json::to_value(unit("api", "failed")).expect("unit");
    data["log_tail"] = json!((0..50).map(|i| format!("failure {i}")).collect::<Vec<_>>());
    let rendered = reporter
        .result(&Report::new(Method::Start, &data))
        .expect("render");
    assert_eq!(rendered.stdout.matches("  | ").count(), 20);
    assert!(rendered.stdout.contains("failure 49"));
    assert!(!rendered.stdout.contains("failure 29"));
    let line = LogLine {
        unit: "api".into(),
        seq: 1,
        timestamp_ms: 1,
        text: "界".repeat(500),
        level: None,
        styled: None,
    };
    let text = line.text(true, false);
    assert_eq!(
        text[0]
            .strip_prefix("api#1 ")
            .expect("cursor")
            .chars()
            .count(),
        300
    );
    assert_eq!(
        line.text(false, false)[0]
            .strip_prefix("api#1 ")
            .expect("cursor")
            .chars()
            .count(),
        500
    );
}

#[test]
fn exit_codes_and_truncation_cursor_survive_rendering() {
    let cli = Cli::parse_from(["devlanes", "status"]);
    let reporter = runtime(&cli, OutputMode::Agent).reporter();
    for code in [
        exit::FAILED,
        exit::USAGE,
        exit::NOT_FOUND,
        exit::DAEMON,
        exit::TIMEOUT,
    ] {
        assert_eq!(
            reporter
                .error(&RpcError::new("failure", Value::Null, code), None)
                .exit_code,
            code
        );
    }
    let data = json!({"items": [{"unit":"api", "seq":1, "timestamp_ms":1, "text":"line", "level":null}], "truncated":true, "next_cursor":"api#2"});
    let rendered = reporter
        .result(&Report::new(Method::Logs, &data))
        .expect("render");
    assert!(rendered.stdout.contains("--cursor 'api#2'"));
    let json = runtime(&cli, OutputMode::Json)
        .reporter()
        .result(&Report::new(Method::Logs, &data))
        .expect("JSON");
    assert_eq!(
        serde_json::from_str::<Value>(&json.stdout).expect("parse"),
        data
    );
}

/// Each method decides its status from its own result shape.
#[test]
fn exit_codes_follow_the_method() {
    let cli = Cli::parse_from(["devlanes", "status"]);
    let reporter = runtime(&cli, OutputMode::Json).reporter();
    let code = |method, data: Value| {
        reporter
            .result(&Report::new(method, &data))
            .expect("render")
            .exit_code
    };
    let start = |ready: bool, timed_out: bool, failures: Value| {
        json!({"lane": "acme@2", "ready": ready, "timed_out": timed_out, "failures": failures,
               "units": [{"name": "api", "state": "ready"}]})
    };
    assert_eq!(code(Method::Start, start(true, false, json!([]))), exit::OK);
    assert_eq!(
        code(Method::Start, start(false, true, json!([]))),
        exit::TIMEOUT
    );
    let failure = json!([{"unit": "seed", "exit_code": 3, "log_tail": []}]);
    assert_eq!(
        code(Method::Restart, start(true, false, failure)),
        exit::FAILED
    );
    assert_eq!(
        code(Method::Start, start(false, false, json!([]))),
        exit::FAILED
    );
    assert_eq!(
        code(
            Method::LaneNew,
            json!({"lane": "acme@3", "start": start(false, true, json!([]))})
        ),
        exit::TIMEOUT
    );
    assert_eq!(code(Method::LaneNew, json!({"lane": "acme@3"})), exit::OK);
    // A unit stopped by SIGTERM keeps its 143; that is a normal stop.
    let stopped = json!({"items": [{"lane": "acme@2", "state": "stopped",
        "units": [{"name": "api", "state": "stopped", "exit_code": 143}]}]});
    assert_eq!(code(Method::Stop, stopped.clone()), exit::OK);
    assert_eq!(code(Method::Status, stopped), exit::OK);
    let failed = json!({"items": [{"lane": "acme@2", "state": "running",
        "units": [{"name": "api", "state": "failed", "exit_code": 1}]}]});
    assert_eq!(code(Method::Status, failed.clone()), exit::FAILED);
    assert_eq!(code(Method::LaneList, failed), exit::FAILED);
    assert_eq!(
        code(
            Method::LaneList,
            json!({"items": [{"lane": "acme@2", "state": "failed"}]})
        ),
        exit::FAILED
    );
    // `wait` ends with 0 only when ready; a unit that exited 3 gives 1, not 3.
    assert_eq!(code(Method::Wait, json!({"ready": true})), exit::OK);
    assert_eq!(
        code(
            Method::Wait,
            json!({"ready": false, "state": "exited", "exit_code": 3})
        ),
        exit::FAILED
    );
    assert_eq!(
        code(Method::Wait, json!({"ready": false, "state": "idle"})),
        exit::FAILED
    );
    // A foreground run passes the task's own status through.
    assert_eq!(
        code(Method::Run, json!({"task": "test", "exit_code": 3})),
        3
    );
}

/// `doctor --json` adds the client's output mode and its reason to the daemon's checks.
#[test]
fn doctor_json_names_the_output_mode() {
    let cli = Cli::parse_from(["devlanes", "--json", "doctor"]);
    let runtime = Runtime::resolve(&cli, Signals::default()).expect("runtime");
    let rendered = runtime
        .reporter()
        .result(&Report::new(Method::Doctor, &json!({"projects": 1})))
        .expect("doctor");
    let data: Value = serde_json::from_str(&rendered.stdout).expect("json");
    assert_eq!(data["projects"], 1);
    assert_eq!(data["output"]["mode"], "json");
    assert_eq!(data["output"]["reason"], "--json");
}

/// A unit a previous daemon left running: shown with its pid and the stop command,
/// not a failure for status or stop, never ready for wait, and refused starts say so.
#[test]
fn orphaned_units_render_and_exit() {
    let orphan = UnitSummary {
        name: "api".into(),
        lane: "acme@2".into(),
        state: devlanes_proto::ORPHANED.into(),
        pid: Some(4242),
        started_at_ms: Some(1_000),
        orphaned_since_ms: Some(2_000),
        ..UnitSummary::default()
    };
    let lane = devlanes_proto::LaneSummary {
        reference: "acme@2".into(),
        project: "acme".into(),
        index: Some(2),
        state: "running".into(),
        units: vec![orphan.clone()],
        ..devlanes_proto::LaneSummary::default()
    };
    let status = json!({"items": [lane], "truncated": false, "next_cursor": null});
    let cli = Cli::parse_from(["devlanes", "status"]);
    let stop_hint = "devlanes stop api --lane acme@2";
    for mode in [OutputMode::Human, OutputMode::Plain, OutputMode::Agent] {
        let rendered = runtime(&cli, mode)
            .reporter()
            .result(&Report::new(Method::Status, &status))
            .expect("status");
        assert_eq!(rendered.exit_code, exit::OK, "{mode:?}");
        assert!(rendered.stdout.contains("orphaned"), "{}", rendered.stdout);
        assert!(
            rendered
                .stdout
                .contains("left running by a previous daemon (pid 4242)"),
            "{}",
            rendered.stdout
        );
        assert!(rendered.stdout.contains(stop_hint), "{}", rendered.stdout);
    }
    let json = runtime(&cli, OutputMode::Json)
        .reporter()
        .result(&Report::new(Method::Status, &status))
        .expect("json");
    assert_eq!(json.exit_code, exit::OK);
    let parsed: Value = serde_json::from_str(&json.stdout).expect("json");
    assert_eq!(parsed["items"][0]["units"][0]["state"], "orphaned");
    assert_eq!(parsed["items"][0]["units"][0]["pid"], 4242);

    let stopped = UnitSummary {
        state: "stopped".into(),
        ..orphan
    };
    let rendered = runtime(&cli, OutputMode::Agent)
        .reporter()
        .result(&Report::new(Method::Stop, &json!({"items": [stopped]})))
        .expect("stop");
    assert_eq!(rendered.exit_code, exit::OK);
    assert!(
        rendered
            .stdout
            .contains("stopped acme@2:api (left running by a previous daemon)"),
        "{}",
        rendered.stdout
    );

    let wait =
        json!({"lane": "acme@2", "unit": "api", "state": "orphaned", "ready": false, "pid": 4242});
    let rendered = runtime(&cli, OutputMode::Agent)
        .reporter()
        .result(&Report::new(Method::Wait, &wait))
        .expect("wait");
    assert_eq!(rendered.exit_code, exit::FAILED);
    assert!(rendered.stdout.contains(stop_hint), "{}", rendered.stdout);

    let refused = RpcError::new(
        "unit_orphaned",
        json!({"lane": "acme@2", "unit": "api", "pid": 4242,
               "message": "api in acme@2 was left running by a previous daemon (pid 4242)"}),
        1,
    );
    let rendered = runtime(&cli, OutputMode::Agent)
        .reporter()
        .error(&refused, None);
    assert_eq!(rendered.exit_code, exit::FAILED);
    assert!(
        rendered
            .stderr
            .contains("api in acme@2 was left running by a previous daemon (pid 4242)"),
        "{}",
        rendered.stderr
    );
    assert!(rendered.stderr.contains(stop_hint), "{}", rendered.stderr);
    // The daemon's own shape: no message, so the CLI words it from the fields.
    let refused = RpcError::new(
        "unit_orphaned",
        json!({"lane": "acme@2", "unit": "api", "pid": 4242,
               "command": "devlanes stop api --lane acme@2"}),
        1,
    );
    let rendered = runtime(&cli, OutputMode::Plain)
        .reporter()
        .error(&refused, None);
    assert_eq!(rendered.exit_code, exit::FAILED);
    assert!(
        rendered.stderr.contains("(pid 4242)"),
        "{}",
        rendered.stderr
    );
    assert!(rendered.stderr.contains(stop_hint), "{}", rendered.stderr);
}

struct FakeCompletion(AtomicUsize);
impl CompletionSource for FakeCompletion {
    /// Simulates an existing daemon without giving completion any spawn capability.
    fn existing_daemon_candidates(&self, kind: CompletionKind, _prefix: &str) -> Vec<String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        if kind == CompletionKind::Unit {
            vec!["api".into(), "web".into()]
        } else {
            vec![]
        }
    }
}

#[test]
fn completion_tree_passes_clap_debug_asserts() {
    Cli::command().debug_assert();
    completion_command(Arc::new(FakeCompletion(AtomicUsize::new(0)))).debug_assert();
}

/// Candidates clap_complete's engine offers for `args` (the last one is completed).
fn complete(args: &[&str]) -> Vec<String> {
    let mut command = completion_command(Arc::new(FakeCompletion(AtomicUsize::new(0))));
    let args: Vec<std::ffi::OsString> = std::iter::once("devlanes")
        .chain(args.iter().copied())
        .map(Into::into)
        .collect();
    clap_complete::engine::complete(&mut command, args.clone(), args.len() - 1, None)
        .expect("complete")
        .into_iter()
        .map(|candidate| candidate.get_value().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn dynamic_completion_offers_commands_and_daemon_units() {
    let commands = complete(&["st"]);
    assert!(commands.contains(&"start".to_owned()), "{commands:?}");
    assert!(commands.contains(&"status".to_owned()), "{commands:?}");
    assert!(commands.contains(&"stop".to_owned()), "{commands:?}");
    let units = complete(&["logs", ""]);
    assert!(units.contains(&"api".to_owned()), "{units:?}");
    assert!(units.contains(&"web".to_owned()), "{units:?}");
    assert_eq!(complete(&["debug", "why", "a"]), ["api"]);
}

#[test]
fn completion_is_lazy_and_filters_existing_candidates() {
    let source = Arc::new(FakeCompletion(AtomicUsize::new(0)));
    let command = completion_command(source.clone());
    assert_eq!(source.0.load(Ordering::Relaxed), 0);
    let restart = command.find_subcommand("restart").expect("restart");
    let arg = restart
        .get_arguments()
        .find(|arg| arg.get_id() == "units")
        .expect("units");
    let completer = arg
        .get::<clap_complete::ArgValueCompleter>()
        .expect("completer");
    assert_eq!(completer.complete(std::ffi::OsStr::new("a")).len(), 1);
    assert_eq!(source.0.load(Ordering::Relaxed), 1);
    let mut buffer = Vec::new();
    write_completions(Shell::Bash, &mut buffer).expect("registration");
    assert!(!buffer.is_empty());
    assert!(
        String::from_utf8(buffer)
            .expect("UTF-8")
            .contains("COMPLETE=")
    );
}
mod agent_guide;
mod choose;
mod ci;
mod codes;
mod diagnostics;
mod help;
mod progress;
#[cfg(windows)]
mod pwsh_export;
mod reference_docs;
mod render;
mod start_ui;
mod terminal;
