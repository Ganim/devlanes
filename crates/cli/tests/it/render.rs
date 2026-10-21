//! Every daemon result shape in every text mode, from fixed fake responses.
//! Agent-mode output doubles as the examples agents learn from, so its wording is
//! part of the contract.

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{Event, Method, RpcError};
use serde_json::{Value, json};

/// Runtime for `mode` with color off, so snapshots stay readable.
fn runtime(mode: OutputMode) -> Runtime {
    let cli = Cli::parse_from(["devlanes"]);
    Runtime::resolve(
        &cli,
        Signals {
            env_output: Some(mode.as_str().into()),
            stdout_tty: mode == OutputMode::Human,
            stdin_tty: mode == OutputMode::Human,
            no_color: true,
            ..Signals::default()
        },
    )
    .expect("runtime")
}

/// A unit summary as the daemon sends it.
fn unit(name: &str, state: &str) -> Value {
    json!({
        "name": name, "lane": "acme@2", "state": state, "pid": 4100,
        "exit_code": if state == "failed" { json!(1) } else { Value::Null },
        "ports": if state == "ready" { json!({"http": 20101}) } else { json!({}) },
        "urls": if state == "ready" && name != "worker" {
            json!([format!("https://{name}.acme-2.localhost")])
        } else {
            json!([])
        },
        "actor": {"kind": "human"}, "reason": null, "kind": "service",
        "autostart": true, "hidden": false, "tags": [],
        "started_at_ms": 1_700_000_000_000_u64,
        "cpu_percent": if state == "ready" { json!(4.5) } else { Value::Null },
        "memory_bytes": if state == "ready" { json!(150_000_000) } else { Value::Null },
    })
}

/// A start result: two ready services with URLs, two without, one failure, one skipped task.
fn start_result() -> Value {
    json!({
        "lane": "acme@2", "branch": "feat/billing",
        "started": ["acme@2:api", "acme@2:web", "acme@2:admin", "acme@2:worker", "acme@2:mailer"],
        "already_running": [], "whole_lane": true, "ready": false, "timed_out": false,
        "failures": [{
            "lane": "acme@2", "unit": "admin", "exit_code": 1,
            "log_tail": ["Error: Cannot find module './env'", "    at main.ts:3:1"], "reason": null,
        }],
        "skipped_tasks": ["seed"],
        "units": [unit("api", "ready"), unit("web", "ready"), unit("admin", "failed"),
                  unit("worker", "ready"), unit("mailer", "ready")],
    })
}

/// A lane summary.
fn lane(index: Option<u32>, branch: &str, state: &str, units: Vec<Value>) -> Value {
    json!({
        "reference": index.map_or("acme@spike".into(), |i| format!("acme@{i}")),
        "project": "acme", "index": index, "branch": branch,
        "path": format!("/work/acme-lanes/{}", branch.replace('/', "-")),
        "state": state, "config_changed": false, "units": units, "profile": null,
        "moves_to": null,
        "domain": index.map(|i| if i == 0 { "acme.localhost".to_owned() } else { format!("acme-{i}.localhost") }),
        "port_block": index.map(|i| [20000 + i * 100, 20099 + i * 100]),
        "reason": null, "needs_env": false, "containers": [],
    })
}

/// A log page.
fn lines(unit: &str, texts: &[&str]) -> Vec<Value> {
    texts
        .iter()
        .enumerate()
        .map(|(seq, text)| json!({"unit": unit, "seq": seq + 1, "timestamp_ms": 1000 + seq, "text": text, "level": null}))
        .collect()
}

/// Fixed daemon answers, one per command shape.
fn fixtures() -> Vec<(&'static str, Method, Value)> {
    let queued_reason = json!({"code": "queued", "exit_code": 0, "data": {
        "position": 6, "queued": 40, "in_flight": 4, "window": 4, "hold": "window_full", "cpu_percent": 93.2,
    }});
    let mut queued_unit = unit("search", "queued");
    queued_unit["reason"] = queued_reason.clone();
    let mut status_lane = lane(
        Some(2),
        "feat/billing",
        "failed",
        vec![
            unit("api", "ready"),
            unit("admin", "failed"),
            unit("worker", "idle"),
            queued_unit.clone(),
        ],
    );
    status_lane["units"][1]["reason"] =
        json!({"code": "exited", "data": {"code": 1, "signal": null}, "exit_code": 1});
    let mut start_queued = start_result();
    start_queued["timed_out"] = json!(true);
    start_queued["units"]
        .as_array_mut()
        .expect("units")
        .push(queued_unit);
    let mut moved = lane(Some(3), "fix/login", "idle", vec![unit("api", "idle")]);
    moved["moves_to"] = json!(0);
    moved["config_changed"] = json!(true);
    let mut env_lane = lane(Some(4), "feat/search", "idle", vec![]);
    env_lane["needs_env"] = json!(true);
    let mut unassigned = lane(None, "spike/xyz", "unassigned", vec![]);
    unassigned["reason"] =
        json!({"code": "unassigned", "data": {"reason": "pool_exhausted"}, "exit_code": 1});
    let mut missing = lane(Some(5), "old/branch", "idle", vec![]);
    missing["reason"] =
        json!({"code": "worktree_missing", "data": {"path": "/work/gone"}, "exit_code": 1});
    let page = |items: Vec<Value>| json!({"items": items, "truncated": false, "next_cursor": null});
    vec![
        ("start", Method::Start, start_result()),
        ("start_queued", Method::Start, start_queued),
        (
            "restart_ready",
            Method::Restart,
            json!({
                "lane": "acme@2", "branch": "feat/billing", "started": ["acme@2:api"],
                "already_running": [], "whole_lane": false, "ready": true, "timed_out": false,
                "failures": [], "skipped_tasks": [], "units": [unit("api", "ready")],
            }),
        ),
        (
            "resume",
            Method::Resume,
            page(vec![
                json!({
                    "lane": "acme@0", "branch": "main", "started": ["acme@0:api"], "already_running": [],
                    "whole_lane": true, "ready": true, "timed_out": false, "failures": [],
                    "skipped_tasks": [], "units": [unit("api", "ready")],
                }),
                json!({"lane": "acme@3", "error": {"code": "secrets_locked", "data": {"profile": "dev"}, "exit_code": 1}}),
            ]),
        ),
        (
            "stop",
            Method::Stop,
            page(vec![unit("api", "stopped"), unit("web", "exited")]),
        ),
        (
            "status",
            Method::Status,
            page(vec![
                lane(
                    Some(0),
                    "main",
                    "running",
                    vec![unit("api", "ready"), unit("web", "ready")],
                ),
                status_lane.clone(),
                moved,
                env_lane,
                missing,
                unassigned,
            ]),
        ),
        ("status_one_lane", Method::Status, page(vec![status_lane])),
        (
            "lane_new",
            Method::LaneNew,
            json!({"lane": lane(Some(6), "feat/checkout", "idle", vec![])}),
        ),
        (
            "lane_clean",
            Method::LaneClean,
            json!({
                "lane": "acme@2", "stopped": ["api", "web"],
                "tasks": [{"task": "drop-db", "exit_code": 0}],
                "teardown": [{"entry": "vhost", "exit_code": 3, "ignored": false}],
                "infra": {"compose": "acme-2"},
            }),
        ),
        (
            "lane_gc",
            Method::LaneGc,
            json!({"cleaned": ["acme@5"], "offsets_removed": 3}),
        ),
        ("exec", Method::Exec, {
            let mut exec = unit("probe", "ready");
            exec["kind"] = json!("exec");
            exec["urls"] = json!([]);
            exec["ports"] = json!({});
            exec
        }),
        (
            "wait_ready",
            Method::Wait,
            json!({"lane": "acme@2", "unit": "api", "state": "ready", "ready": true}),
        ),
        (
            "wait_failed",
            Method::Wait,
            json!({
                "lane": "acme@2", "unit": "admin", "state": "failed", "ready": false, "failed": ["admin"],
                "reason": {"code": "exited", "data": {"code": 1}, "exit_code": 1}, "exit_code": 1,
                "log_tail": ["Error: Cannot find module './env'"],
            }),
        ),
        (
            "wait_idle",
            Method::Wait,
            json!({"lane": "acme@2", "unit": null, "state": "idle", "ready": false}),
        ),
        (
            "errors",
            Method::Errors,
            page(vec![
                json!({"lane": "acme@2", "unit": "api", "state": "ready", "exit_code": null, "crashes": 2,
                   "reason": null, "error_lines": ["warn: slow query"], "port_conflicts": []}),
                json!({"lane": "acme@2", "unit": "admin", "state": "failed", "exit_code": 1, "crashes": 1,
                   "reason": {"code": "exited", "data": {"code": 1}, "exit_code": 1},
                   "error_lines": ["Error: Cannot find module './env'"],
                   "port_conflicts": [{"port": 20102, "pid": 911}]}),
            ]),
        ),
        ("errors_none", Method::Errors, page(vec![])),
        (
            "why",
            Method::Why,
            json!({
                "lane": "acme@2", "unit": "admin", "state": "idle",
                "reasons": [
                    {"code": "not_in_profile", "data": {"profile": "e2e"}},
                    {"code": "needs_env", "data": {"lane": "acme@2"}},
                    {"code": "secrets_locked", "data": {"profile": "dev"}},
                    {"code": "config_error", "data": {"message": "unknown field", "file": "devlanes.yaml", "line": 4, "column": 3}},
                    {"code": "port_in_use", "data": {"port": 20102, "pid": 911}},
                    {"code": "waiting_after_services", "data": {}},
                    {"code": "ready_timeout", "data": {}},
                    {"code": "exited", "data": {"code": 1, "signal": null}},
                    {"code": "stopped", "data": {"exit_code": 0, "signal": null, "requested": true}},
                    {"code": "autostart_off", "data": {}},
                    {"code": "unassigned", "data": {"reason": "lanes_max", "max": 4}},
                    {"code": "no_config", "data": {}},
                    {"code": "sudo_denied", "data": {"hosts": ["admin.acme-2.localhost"], "message": "sudo said no"}},
                {"code": "queued", "data": queued_reason["data"].clone()},
                ],
            }),
        ),
        (
            "env",
            Method::Env,
            json!({"lane": "acme@2", "unit": "api", "env": {"PORT": "20101", "DATABASE_URL": "postgres://localhost:20110/acme", "MULTI": "a\nb"}}),
        ),
        (
            "env_explain",
            Method::Env,
            json!({"lane": "acme@2", "unit": "api", "items": [
            {"key": "PORT", "value": "20101", "layer": "devlanes", "redacted": false},
            {"key": "API_TOKEN", "value": null, "layer": "secrets", "redacted": true},
            {"key": "NODE_ENV", "value": "development", "layer": "service", "redacted": false},
        ], "truncated": false, "next_cursor": null}),
        ),
        (
            "url",
            Method::Url,
            json!({"lane": "acme@2", "unit": "web", "url": "https://web.acme-2.localhost", "urls": ["http://127.0.0.1:20101", "https://web.acme-2.localhost"]}),
        ),
        (
            "hosts",
            Method::Hosts,
            json!({
                "items": [
                    {"host": "api.acme-2.localhost", "project": "acme", "lane": "acme@2", "unit": "api", "port": 20101, "fallback": false, "state": "ready"},
                    {"host": "acme-2.localhost", "project": "acme", "lane": "acme@2", "unit": "web", "port": 20100, "fallback": true, "state": "idle"},
                ],
                "proxy": {"http": {"bound": true, "port": 80, "fallback_reason": null},
                          "https": {"bound": true, "port": 8443, "fallback_reason": "port 443 needs root"}},
                "truncated": false, "next_cursor": null,
            }),
        ),
        (
            "logs",
            Method::Logs,
            page(lines("api", &["listening on :20101", "GET / 200"])),
        ),
        (
            "logs_merged",
            Method::Logs,
            page({
                let mut merged = lines("api", &["GET / 200"]);
                merged.extend(lines("web", &["compiled in 120ms"]));
                merged
            }),
        ),
        (
            "logs_all",
            Method::Logs,
            json!({"lane": "acme@2", "units": [
                {"unit": "api", "lines": lines("api", &["listening on :20101"]), "truncated": false},
                {"unit": "web", "lines": lines("web", &["compiled", "compiled"]), "truncated": true},
            ]}),
        ),
        (
            "run_ok",
            Method::Run,
            json!({"task": "migrate", "lane": "acme@2", "unit": "migrate", "interactive": false, "exit_code": 0, "state": "exited"}),
        ),
        (
            "run_failed",
            Method::Run,
            json!({"task": "test", "lane": "acme@2", "unit": "test", "interactive": false, "exit_code": 2, "state": "failed"}),
        ),
        (
            "run_detached",
            Method::Run,
            json!({"task": "seed", "lane": "acme@2", "unit": "seed", "interactive": false, "detached": true}),
        ),
        (
            "run_interactive",
            Method::Run,
            json!({"task": "console", "lane": "acme@2", "unit": "console", "interactive": true, "detached": false}),
        ),
        (
            "task_list",
            Method::TaskList,
            json!({
                "items": [
                    {"name": "migrate", "when": "lane.start", "after": null, "profiles": null, "steps": null, "confirm": false, "interactive": false, "run_script": {"source": "package.json", "name": "db:migrate"}},
                    {"name": "seed", "when": "lane.create", "after": "services", "profiles": ["dev"], "steps": null, "confirm": true, "interactive": false, "run_script": null},
                    {"name": "reset", "when": null, "after": null, "profiles": null, "steps": ["drop", "migrate"], "confirm": false, "interactive": false, "run_script": null},
                ],
                "sources": [{"source": "package.json", "runner": "npm"}],
                "truncated": false, "next_cursor": null,
            }),
        ),
        (
            "metrics",
            Method::Metrics,
            json!({"units": [
                {"lane": "acme@2", "unit": "api", "cpu_percent": 12.5, "memory_bytes": 314_572_800, "history": []},
                {"lane": "acme@2", "unit": "web", "cpu_percent": 0.4, "memory_bytes": 52_428_800, "history": []},
                {"lane": "acme@0", "unit": "api", "cpu_percent": null, "memory_bytes": null, "history": []},
            ]}),
        ),
        (
            "metrics_peaks",
            Method::Metrics,
            json!({"lane": "acme@2", "peaks": [
                {"unit": "api", "peak_memory_bytes": 1_073_741_824, "peak_cpu_percent": 125.5},
            ]}),
        ),
        (
            "events",
            Method::Subscribe,
            page(vec![
                json!({"seq": 1, "event": "lane.phase", "data": {"lane": "acme@2", "phase": "infra", "state": "done"}}),
                json!({"seq": 2, "event": "unit.changed", "data": {"unit": unit("api", "ready")}}),
                json!({"seq": 3, "event": "task", "data": {"lane": "acme@2", "task": "migrate", "state": "failed", "exit_code": 1}}),
                json!({"seq": 4, "event": "remote.state", "data": {"host": "box", "state": "connected"}}),
            ]),
        ),
        (
            "infra_up",
            Method::InfraUp,
            json!({"lane": "acme@2", "infra": {}, "endpoints": {
                "postgres": {"port": 20110, "ports": {"5432": 20110}},
                "redis": {"port": 20111, "ports": {"6379": 20111}},
            }}),
        ),
        (
            "infra_status",
            Method::InfraStatus,
            json!({"lane": "acme@2", "infra": {
                "shared": {"project": "acme", "services": [{"service": "postgres", "state": "running", "health": "healthy"}]},
            }}),
        ),
        (
            "doctor",
            Method::Doctor,
            json!({
                "version": "0.1.0", "pid": 4242, "home": "/home/u/.devlanes", "db_path": "/home/u/.devlanes/state.db",
                "schema_version": 5, "supported_schema_version": 5, "socket": "/home/u/.devlanes/run/daemon.sock",
                "sandbox": {"codex_sandbox": false, "app_sandbox": false},
                "infra": {"docker": {"reachable": false, "reason": "docker_unavailable"}, "compose": {"error": "docker_unavailable"}},
                "proxy": {"http": {"bound": true, "port": 80, "fallback_reason": null}, "https": {"bound": false, "reason": "no_ca"}},
                "ca": {"present": true, "path": "/home/u/.devlanes/ca/ca.pem", "trusted": "unknown"},
                "projects": 2, "subscribers": 1,
                "hosts": {"mode": "auto", "probe": "fails", "state": "failed", "names": null, "desired": 3, "skipped": 1,
                          "last_error": {"code": "helper_missing", "message": "the hosts helper is not installed"}, "tld_error": null},
            }),
        ),
        (
            "config_check",
            Method::ConfigCheck,
            json!({"project": "acme", "path": "/work/acme/devlanes.yaml", "warnings": [
                {"path": "templates.unused", "message": "template is never used"},
                {"code": "unknown_platform", "path": "services.api.platform",
                 "message": "no contributor provides platform `elixir`",
                 "data": {"service": "api", "platform": "elixir", "known": ["node", "python"]}},
            ]}),
        ),
        (
            "unlock",
            Method::Unlock,
            json!({"lane": "acme@2", "profile": "dev", "keys": ["API_TOKEN", "DB_PASSWORD"]}),
        ),
        (
            "daemon_status",
            Method::DaemonStatus,
            json!({"version": "0.1.0", "pid": 4242, "uptime_ms": 7_380_000, "managed_processes": 6, "socket": "/home/u/.devlanes/run/daemon.sock"}),
        ),
        (
            "daemon_stop",
            Method::DaemonShutdown,
            json!({"stopping": true, "processes": 6}),
        ),
        (
            "reset_state",
            Method::DaemonResetState,
            json!({"stopped": 6}),
        ),
    ]
}

/// The three text modes of one fixture, separated by mode headers.
fn render_text(method: Method, data: &Value) -> String {
    let mut output = String::new();
    for mode in [OutputMode::Human, OutputMode::Plain, OutputMode::Agent] {
        let rendered = runtime(mode)
            .reporter()
            .result(&Report::new(method, data))
            .expect("render");
        output.push_str(&format!(
            "--- {} (exit {})\n",
            mode.as_str(),
            rendered.exit_code
        ));
        output.push_str(&rendered.stdout);
        if !rendered.stderr.is_empty() {
            output.push_str(&format!("stderr: {}", rendered.stderr));
        }
    }
    output
}

#[test]
fn every_result_shape_in_every_text_mode() {
    for (name, method, data) in fixtures() {
        let json = runtime(OutputMode::Json)
            .reporter()
            .result(&Report::new(method, &data))
            .expect("json");
        assert_eq!(
            serde_json::from_str::<Value>(&json.stdout).expect("json output"),
            data,
            "{name}: JSON mode prints the daemon data unchanged"
        );
        let text = render_text(method, &data);
        assert!(
            !text.contains(": {"),
            "{name}: raw JSON leaked into text output\n{text}"
        );
        insta::assert_snapshot!(format!("render_{name}"), text);
    }
}

#[test]
fn agent_status_lines_start_with_a_stable_lowercase_word() {
    let words = [
        "ready",
        "failed",
        "stopped",
        "starting",
        "idle",
        "running",
        "unassigned",
        "queued",
        "exited",
        "started",
        "created",
        "cleaned",
    ];
    for (name, method, data) in fixtures() {
        if !matches!(
            method,
            Method::Start
                | Method::Stop
                | Method::Wait
                | Method::Errors
                | Method::Why
                | Method::Exec
        ) {
            continue;
        }
        let rendered = runtime(OutputMode::Agent)
            .reporter()
            .result(&Report::new(method, &data))
            .expect("render");
        // Indented detail lines and "no ..." notices are not status lines.
        for line in rendered
            .stdout
            .lines()
            .filter(|line| !line.starts_with(' ') && !line.starts_with("no "))
        {
            let first = line.split(' ').next().unwrap_or_default();
            assert!(
                words.contains(&first) || line.starts_with("acme@"),
                "{name}: {line:?} does not start with a stable word"
            );
        }
    }
}

#[test]
fn human_status_colors_unit_names_by_tag_and_failures_red() {
    let cli = Cli::parse_from(["devlanes"]);
    let colored = Runtime::resolve(
        &cli,
        Signals {
            env_output: Some("human".into()),
            stdout_tty: true,
            ..Signals::default()
        },
    )
    .expect("runtime");
    let mut api = unit("api", "ready");
    api["color"] = json!("blue");
    let mut admin = unit("admin", "failed");
    admin["color"] = json!("green");
    let data = json!({"items": [lane(Some(2), "feat/billing", "failed", vec![api, admin])],
        "truncated": false, "next_cursor": null});
    let human = colored
        .reporter()
        .result(&Report::new(Method::Status, &data))
        .expect("render")
        .stdout;
    assert!(human.contains("\x1b[34mapi\x1b[0m"), "{human:?}");
    assert!(human.contains("\x1b[31madmin\x1b[0m"), "{human:?}");
    let plain = runtime(OutputMode::Plain)
        .reporter()
        .result(&Report::new(Method::Status, &data))
        .expect("render")
        .stdout;
    assert!(!plain.contains('\x1b'));
}

#[test]
fn agent_output_stays_within_the_budget() {
    let units: Vec<Value> = (0..600)
        .map(|i| unit(&format!("svc-{i}"), "ready"))
        .collect();
    let data = json!({"items": [lane(Some(2), "feat/billing", "running", units)], "truncated": false, "next_cursor": null});
    let rendered = runtime(OutputMode::Agent)
        .reporter()
        .result(&Report::new(Method::Status, &data))
        .expect("render");
    assert!(
        rendered.stdout.len() <= DEFAULT_BUDGET,
        "{}",
        rendered.stdout.len()
    );
    assert!(rendered.stdout.contains("truncated; more: devlanes status"));
}

#[test]
fn start_progress_prints_finished_steps_once() {
    let mut output = String::new();
    let reporter = runtime(OutputMode::Plain).reporter();
    for (event, data) in [
        (
            "lane.phase",
            json!({"lane": "acme@2", "phase": "infra", "state": "started"}),
        ),
        (
            "lane.phase",
            json!({"lane": "acme@2", "phase": "infra", "state": "done"}),
        ),
        (
            "lane.phase",
            json!({"lane": "acme@2", "phase": "setup", "state": "skipped", "reason": {"code": "no_setup", "data": {}}}),
        ),
        (
            "task",
            json!({"lane": "acme@2", "task": "migrate", "state": "done", "exit_code": 0}),
        ),
        ("unit.changed", json!({"unit": unit("api", "ready")})),
        (
            "lane.phase",
            json!({"lane": "acme@2", "phase": "services", "state": "failed", "reason": {"code": "unit_failed", "data": {"lane": "acme@2", "unit": "admin"}}}),
        ),
    ] {
        let event = Event {
            seq: 1,
            event: event.into(),
            data,
        };
        output.push_str(&reporter.event(Method::Start, &event).expect("event").stdout);
    }
    insta::assert_snapshot!("render_start_progress", output);
    assert!(
        runtime(OutputMode::Agent)
            .reporter()
            .event(
                Method::Start,
                &Event {
                    seq: 1,
                    event: "lane.phase".into(),
                    data: json!({"lane": "acme@2", "phase": "infra", "state": "done"})
                }
            )
            .expect("event")
            .stdout
            .is_empty()
    );
}

#[test]
fn daemon_errors_are_worded_with_hints() {
    let errors = [
        RpcError::new(
            "not_found",
            json!({"reference": "acme@9", "candidates": ["acme@0", "acme@2"]}),
            3,
        ),
        RpcError::new(
            "ambiguous",
            json!({"reference": "feat", "candidates": ["acme@feat", "shop@feat"]}),
            3,
        ),
        RpcError::new(
            "confirmation_required",
            json!({"lane": "acme@2", "flag": "--yes"}),
            2,
        ),
        RpcError::new(
            "confirmation_required",
            json!({"task": "reset", "prompt": "Drop the database?", "flag": "--yes"}),
            2,
        ),
        RpcError::new("terminal_required", json!({"task": "console"}), 2),
        RpcError::new(
            "project_running_elsewhere",
            json!({"project": "acme", "path": "/work/other-acme"}),
            1,
        ),
        RpcError::new(
            "port_in_use_by_lane",
            json!({"port": 5173, "lane": "shop@1", "unit": "web"}),
            1,
        ),
        RpcError::new(
            "secrets_locked",
            json!({"profile": "dev", "message": "vault locked"}),
            1,
        ),
        RpcError::new(
            "docker_unavailable",
            json!({"message": "no Docker engine is reachable"}),
            1,
        ),
        RpcError::new("timeout", json!({"lane": "acme@2", "unit": "api"}), 124),
        RpcError::new("not_supported_yet", json!({"feature": "remote unlock"}), 1),
        RpcError::new(
            "config_error",
            json!({"message": "unknown field `serivces`", "file": "devlanes.yaml", "line": 2, "column": 1}),
            1,
        ),
        RpcError::new("needs_env", json!({"lane": "acme@2"}), 1),
    ];
    let reporter = runtime(OutputMode::Agent).reporter();
    let mut output = String::new();
    for error in &errors {
        let rendered = reporter.error(error, None);
        assert_eq!(rendered.exit_code, error.exit_code);
        assert!(rendered.stdout.is_empty());
        output.push_str(&rendered.stderr);
    }
    insta::assert_snapshot!("render_daemon_errors", output);
}
