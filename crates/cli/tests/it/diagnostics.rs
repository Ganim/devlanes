//! Spawn failure wording, phase lines that turn into their outcome, and `-v`.

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{Event, Method, RpcError};
use serde_json::{Value, json};

/// A runtime for `mode` with color off; `tty` for stdout and stdin.
fn runtime(args: &[&str], mode: OutputMode, tty: bool, verbose: u8) -> Runtime {
    let cli = Cli::parse_from(args);
    Runtime::resolve(
        &cli,
        Signals {
            env_output: Some(mode.as_str().into()),
            stdout_tty: tty,
            stdin_tty: tty,
            no_color: true,
            env_verbose: verbose,
            ..Signals::default()
        },
    )
    .expect("runtime")
}

/// A start whose `api` could not run `fixture-server`.
fn missing_program() -> Value {
    let reason = json!({
        "code": "spawn_failed",
        "data": {
            "unit": "api", "lane": "acme@0", "program": "fixture-server",
            "message": "I/O failure: No such file or directory (os error 2)",
            "errno": "ENOENT", "reason": "not_found",
            "path_searched": ["/usr/bin", "/bin"],
        },
        "exit_code": 1,
    });
    json!({
        "lane": "acme@0", "branch": "main", "started": [], "already_running": [],
        "whole_lane": true, "ready": false, "timed_out": false,
        "failures": [{
            "lane": "acme@0", "unit": "api", "exit_code": null,
            "log_tail": ["[devlanes] could not start: fixture-server: command not found on PATH (/usr/bin:/bin)"],
            "reason": reason,
        }],
        "skipped_tasks": [], "units": [], "warnings": [],
    })
}

/// The rendered stdout of `data` as the result of `method`.
fn render(runtime: &Runtime, method: Method, data: &Value) -> String {
    runtime
        .reporter()
        .result(&Report::new(method, data))
        .expect("render")
        .stdout
}

#[test]
/// The failure names the program and the cause once, and points at the lane's
/// PATH instead of a log that only repeats it.
fn missing_program_is_named_in_every_mode() {
    let data = missing_program();
    let human = render(
        &runtime(&["devlanes"], OutputMode::Human, true, 0),
        Method::Start,
        &data,
    );
    assert!(
        human.contains("api  could not start: fixture-server: command not found on PATH"),
        "{human}"
    );
    assert!(
        !human.contains("[devlanes]"),
        "the tail repeats the reason: {human}"
    );
    assert!(
        human.contains("hint: check the lane's PATH: devlanes env show api --lane acme@0"),
        "{human}"
    );
    let agent = render(
        &runtime(&["devlanes"], OutputMode::Agent, false, 0),
        Method::Start,
        &data,
    );
    assert_eq!(
        agent,
        "acme@0 (main): not ready, 1 failed\n\
failed api\n  could not start: fixture-server: command not found on PATH\n  \
hint: check the lane's PATH: devlanes env show api --lane acme@0\n"
    );
    // The same error on its own (exec, run) keeps the unit and the hint.
    let error =
        serde_json::from_value::<RpcError>(data["failures"][0]["reason"].clone()).expect("error");
    let rendered = runtime(&["devlanes"], OutputMode::Plain, false, 0)
        .reporter()
        .error(&error, None);
    assert_eq!(
        rendered.stderr,
        "error: api could not start: fixture-server: command not found on PATH\n\
hint: check the lane's PATH: devlanes env show api --lane acme@0\n"
    );
}

#[test]
/// Other causes: a program without the execute bit, a missing directory, and an
/// errno without its own wording.
fn other_spawn_causes() {
    let plain = runtime(&["devlanes"], OutputMode::Plain, false, 0);
    let error = |data: Value| {
        plain
            .reporter()
            .error(&RpcError::new("spawn_failed", data, 1), None)
            .stderr
    };
    let text = error(
        json!({"unit": "api", "program": "./bin/api", "reason": "not_executable", "resolved": "/w/bin/api"}),
    );
    assert!(
        text.contains("api could not start: ./bin/api is not executable"),
        "{text}"
    );
    assert!(text.contains("hint: chmod +x /w/bin/api"), "{text}");
    let text = error(
        json!({"unit": "api", "lane": "acme@0", "program": "x", "reason": "cwd_missing", "cwd": "/w/gone"}),
    );
    assert!(
        text.contains("working directory /w/gone does not exist"),
        "{text}"
    );
    assert!(
        text.contains("devlanes logs acme@0:api --tail 200"),
        "{text}"
    );
    let text = error(
        json!({"unit": "api", "program": "x", "message": "I/O failure: Argument list too long (os error 7)", "errno": "E2BIG"}),
    );
    assert!(
        text.contains("x: I/O failure: Argument list too long (os error 7) (E2BIG)"),
        "{text}"
    );
    // An older daemon's shape still reads.
    let text = error(json!({"unit": "api", "message": "I/O failure: boom"}));
    assert!(
        text.contains("api could not start: I/O failure: boom"),
        "{text}"
    );
    // `with -- COMMAND` keeps its own wording.
    let text = error(json!({"program": "pnpm", "message": "not found"}));
    assert_eq!(text, "error: cannot run pnpm: not found\n");
}

/// A `lane.phase` event.
fn phase(name: &str, state: &str, reason: Option<&str>) -> Event {
    let mut data = json!({"lane": "acme@0", "phase": name, "state": state, "start_id": 3});
    if let Some(code) = reason {
        data["reason"] = json!({"code": code, "data": {}});
    }
    Event {
        seq: 1,
        event: "lane.phase".into(),
        data,
    }
}

/// What a start prints for `events`, phase lines rewritten as on a terminal.
fn stream(runtime: &Runtime, events: &[Event]) -> String {
    let reporter = runtime.reporter();
    let mut lines = PhaseLines::new(runtime);
    let mut out = String::new();
    for event in events {
        let mut stdout = reporter.event(Method::Start, event).expect("event").stdout;
        lines.rewrite(event, &mut stdout);
        out.push_str(&stdout);
    }
    out
}

#[test]
/// On a terminal each phase prints as it begins, then the line becomes its
/// outcome or goes away; elsewhere only outcomes print.
fn phases_print_as_they_begin() {
    let events = [
        phase("infra", "started", None),
        phase("infra", "done", None),
        phase("setup", "started", None),
        phase("setup", "skipped", Some("no_setup")),
        phase("services", "started", None),
    ];
    let tty = stream(&runtime(&["devlanes"], OutputMode::Human, true, 0), &events);
    assert_eq!(
        tty,
        "… infra: starting containers, waiting until they are healthy\n\
\x1b[1A\x1b[2K✓ infra\n\
… setup: running infra.setup\n\
\x1b[1A\x1b[2K\
… services\n"
    );
    let piped = stream(
        &runtime(&["devlanes"], OutputMode::Human, false, 0),
        &events,
    );
    assert_eq!(piped, "✓ infra\n");
    // With -v the stderr diagnostics interleave, so nothing is erased.
    let verbose = stream(&runtime(&["devlanes"], OutputMode::Human, true, 1), &events);
    assert!(verbose.starts_with("… infra"), "{verbose}");
    assert!(!verbose.contains('\x1b'), "{verbose}");
    let plain = stream(
        &runtime(&["devlanes"], OutputMode::Plain, false, 0),
        &events,
    );
    assert!(!plain.contains('…'), "{plain}");
}

#[test]
/// `-v` counts, `DEVLANES_VERBOSE` sets it too, the higher wins, and requests
/// carry it to the daemon. A verbose start prints instead of opening the TUI.
fn verbose_flag_and_environment() {
    let human = |args: &[&str], env: u8| runtime(args, OutputMode::Human, true, env);
    assert_eq!(human(&["devlanes"], 0).verbose, 0);
    assert_eq!(human(&["devlanes", "-v"], 0).verbose, 1);
    assert_eq!(human(&["devlanes", "-vv", "status"], 0).verbose, 2);
    assert_eq!(human(&["devlanes", "status", "--verbose"], 2).verbose, 2);
    assert_eq!(human(&["devlanes", "-vvv"], 0).verbose, 2);
    let cli = Cli::parse_from(["devlanes", "-v", "status"]);
    let Action::Rpc(request) = cli
        .action(&human(&["devlanes", "-v", "status"], 0))
        .expect("action")
    else {
        panic!("status is an RPC");
    };
    assert_eq!(request.context.verbose, 1);
    let start = Cli::parse_from(["devlanes", "start"]);
    assert!(
        start
            .start_in_ui(&human(&["devlanes", "start"], 0))
            .is_some()
    );
    assert!(
        start
            .start_in_ui(&human(&["devlanes", "start"], 1))
            .is_none()
    );
}

#[test]
/// `debug bundle` runs in the client with the arguments after `--`.
fn debug_bundle_action() {
    let cli = Cli::parse_from([
        "devlanes", "debug", "bundle", "--to", "x.tar.gz", "--", "start", "api",
    ]);
    match cli.action(&runtime(&["devlanes"], OutputMode::Plain, false, 0)) {
        Ok(Action::DebugBundle {
            to,
            log_lines,
            args,
            ..
        }) => {
            assert_eq!(to.as_deref(), Some(std::path::Path::new("x.tar.gz")));
            assert_eq!(log_lines, 500);
            assert_eq!(args, ["start", "api"]);
        }
        _ => panic!("expected a bundle"),
    }
}
