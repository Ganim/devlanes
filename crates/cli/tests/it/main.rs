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
        | Action::Attach(r)
        | Action::Open(r)
        | Action::ExportEnv(r)
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
            &["status", "acme@2", "--watch", "--mine"],
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
        (&["errors", "--since", "5m"], Some(Method::Errors)),
        (&["wait", "acme@2", "--timeout", "1m"], Some(Method::Wait)),
        (&["why", "api"], Some(Method::Why)),
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
        (&["open", "api"], Some(Method::Url)),
        (&["url", "api"], Some(Method::Url)),
        (&["env", "--explain", "api"], Some(Method::Env)),
        (&["env", "api", "--export"], Some(Method::Env)),
        (&["shell", "api"], Some(Method::Env)),
        (&["top", "--json"], Some(Method::Metrics)),
        (&["resume"], Some(Method::Resume)),
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
        (&["sync", "status"], Some(Method::SyncStatus)),
        (&["config", "check"], Some(Method::ConfigCheck)),
        (&["schema"], Some(Method::Schema)),
        (&["doctor"], Some(Method::Doctor)),
        (&["hosts"], Some(Method::Hosts)),
        (&["daemon", "status"], Some(Method::DaemonStatus)),
        (&["daemon", "stop"], Some(Method::DaemonShutdown)),
        (
            &["daemon", "reset-state", "--yes"],
            Some(Method::DaemonResetState),
        ),
        (&["connect", "box", "--background"], Some(Method::Connect)),
        (&["disconnect", "box"], Some(Method::Disconnect)),
        (&["host", "ls"], Some(Method::HostList)),
        (&["events", "-f"], Some(Method::Subscribe)),
        (&["trust", "--hosts"], None),
        (&["untrust"], None),
        (&["unlock"], None),
        (&["daemon", "restart"], None),
        (&["daemon", "logs", "-f", "--tail", "10"], None),
        (&["daemon", "install"], None),
        (&["daemon", "uninstall"], None),
        (&["daemon", "run"], None),
        (&["ui"], None),
        (&["agent-guide"], None),
        (&["completions", "zsh"], None),
        (&["tunnel", "--stdio"], None),
        (&["askpass", "Password:"], None),
    ];
    for (args, method) in cases {
        let cli = Cli::try_parse_from(std::iter::once("devlanes").chain(args.iter().copied()))
            .expect("inventory parses");
        let action = action(&cli, &runtime(&cli, OutputMode::Agent));
        if let Some(method) = method {
            assert_eq!(request(action).method, *method, "{args:?}");
        }
    }
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
fn environment_is_captured_only_for_relevant_methods() {
    for (args, capture) in [
        (vec!["start"], true),
        (vec!["restart", "api"], true),
        (vec!["run", "migrate"], true),
        (vec!["exec", "--name", "demo", "--", "echo"], true),
        (vec!["resume"], true),
        (vec!["run", "--list"], false),
        (vec!["env"], false),
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
        "--host",
        "box",
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
    assert_eq!(req.context.host.as_deref(), Some("box"));
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
    let cli = Cli::parse_from(["devlanes", "open", "api"]);
    assert!(matches!(
        action(&cli, &runtime(&cli, OutputMode::Agent)),
        Action::Rpc(_)
    ));
}

#[test]
fn malformed_inputs_are_usage_errors() {
    for args in [
        vec!["restart"],
        vec!["exec", "--name", "x"],
        vec!["run"],
        vec!["run", "task", "--list"],
        vec!["start", "--env", "missing-equals"],
        vec!["--timeout", "0"],
        vec!["--timeout", "18446744073709551615h"],
        vec!["--budget", "12"],
        vec!["logs"],
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
            .error(&error, Some(&Hint::command(["why", "acme@2:api"])));
        assert!(result.stdout.is_empty());
        assert_eq!(result.exit_code, 3);
        assert_eq!(
            result.stderr,
            "error: port 20000 is in use by PID 42\n  api could not bind\nhint: devlanes why acme@2:api\n"
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
    insta::assert_snapshot!(export_environment(&values).expect("exports"), @r#"
    export EMPTY=''
    export MESSAGE='it'"'"'s $(touch /bad)'
    "#);
    assert!(export_environment(&BTreeMap::from([("X;bad".into(), "y".into())])).is_err());
    assert_eq!(
        Hint::logs("acme@2", "api-*", 200).0,
        "devlanes logs 'acme@2:api-*' --tail 200"
    );
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
    let rendered = runtime(&cli, OutputMode::Agent)
        .reporter()
        .result(&Report::new(Method::Stop, &data))
        .expect("render");
    assert_eq!(rendered.stderr, "note: affected units started by a human\n");
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
        let event = Event {
            seq,
            event: "log".into(),
            data: serde_json::to_value(LogLine {
                unit: "api".into(),
                seq,
                timestamp_ms: 1,
                text: "long line of process output".into(),
                level: None,
            })
            .expect("line"),
        };
        combined.push_str(&stream.event(&event).expect("stream event").stdout);
    }
    assert!(stream.exhausted());
    assert!(combined.len() <= 256, "{}", combined.len());
    assert_eq!(combined.matches("truncated;").count(), 1);
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
mod ci;
