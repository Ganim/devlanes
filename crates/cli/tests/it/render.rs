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

/// A unit summary in the shape a real daemon sends (captured from `start --json`
/// of the `two-services` fixture): the loopback URL first, then the proxied one
/// with the HTTPS listener's port; units without hosts have only the first.
fn unit(name: &str, state: &str) -> Value {
    let port = 20100 + name.len() as u16;
    let mut urls = vec![json!(format!("http://127.0.0.1:{port}"))];
    if name != "worker" {
        urls.push(json!(format!("https://{name}.acme-2.localhost:8443")));
    }
    json!({
        "name": name, "lane": "acme@2", "state": state,
        "pid": if state == "idle" { Value::Null } else { json!(4100) },
        "exit_code": if state == "failed" { json!(1) } else { Value::Null },
        "ports": {"default": port},
        "urls": urls,
        "actor": {"kind": "human"},
        "reason": if state == "failed" {
            json!({"code": "exited", "data": {"code": 1, "signal": null}, "exit_code": 1})
        } else {
            Value::Null
        },
        "kind": "service",
        "autostart": true, "hidden": false, "tags": [], "color": "blue",
        "started_at_ms": if state == "idle" { Value::Null } else { json!(1_700_000_000_000_u64) },
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
        // `start bg` while bg is a previous daemon's orphan: refused, nothing ran.
        (
            "start_refused_orphan",
            Method::Start,
            json!({
                "lane": "acme@0", "branch": "main", "started": [], "already_running": [],
                "whole_lane": false, "ready": false, "timed_out": false, "skipped_tasks": [],
                "failures": [{"lane": "acme@0", "unit": "bg", "exit_code": null, "log_tail": [],
                    "reason": {"code": "unit_orphaned", "exit_code": 1, "data": {
                        "unit": "bg", "lane": "acme@0", "pid": 4242, "started_at_ms": 1, "orphaned_since_ms": 2,
                        "command": "devlanes stop bg --lane acme@0"}}}],
                "units": [],
            }),
        ),
        ("start_warnings", Method::Start, {
            let mut ready = start_result();
            ready["ready"] = json!(true);
            ready["failures"] = json!([]);
            ready["skipped_tasks"] = json!([]);
            ready["units"][2] = unit("admin", "ready");
            ready["warnings"] = json!([
                {"code": "host_outside_ca", "data": {"lane": "acme@2", "unit": "api", "host": "api.example.com",
                 "permitted": ["localhost", "test"], "suggestion": "api.acme.test"}},
                {"code": "memory_baseline", "data": {"peak_memory_bytes": 8_589_934_592_u64,
                 "available_memory_bytes": 4_294_967_296_u64, "units": []}},
            ]);
            ready
        }),
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
        ("status_lane_warnings", Method::Status, {
            let mut warned = lane(
                Some(2),
                "feat/billing",
                "running",
                vec![unit("api", "ready")],
            );
            warned["warnings"] = json!([
                {"code": "ports_pending", "data": {"lane": "acme@2", "message": "the edited config needs a different port block"}},
                {"code": "host_conflict", "data": {"host": "api.acme.localhost", "lane": "acme@0", "other": "acme@2",
                 "message": "api.acme.localhost is routed by acme@0 and acme@2; acme@0 keeps it because it has running units"}},
            ]);
            page(vec![
                lane(Some(0), "main", "running", vec![unit("api", "ready")]),
                warned,
            ])
        }),
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
        (
            "lane_gc_kept",
            Method::LaneGc,
            json!({"cleaned": [], "kept_running": ["acme@3", "acme@4"], "offsets_removed": 0}),
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
            "hosts_warnings",
            Method::Hosts,
            json!({
                "items": [
                    {"host": "api.acme.localhost", "project": "acme", "lane": "acme@0", "unit": "api", "port": 20001, "fallback": false, "state": "ready"},
                ],
                "proxy": {"http": {"bound": true, "port": 80, "fallback_reason": null}},
                "warnings": [
                    {"code": "host_conflict", "data": {"host": "api.acme.localhost", "lane": "acme@0", "other": "acme@2",
                     "message": "api.acme.localhost is routed by acme@0 and acme@2; acme@0 keeps it because it comes first by project and lane index"}},
                    {"code": "host_rejected", "data": {"lane": "acme@2", "unit": "web", "host": "web_app.localhost",
                     "message": "`web_app.localhost` contains `_`, which DNS names cannot; the route is left out"}},
                ],
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
            "doctor_findings",
            Method::Doctor,
            json!({
                "version": "0.2.0", "pid": 4242, "home": "/home/u/.devlanes", "db_path": "/home/u/.devlanes/state.db",
                "schema_version": 5, "supported_schema_version": 5, "socket": "/home/u/.devlanes/run/daemon.sock",
                "sandbox": {"codex_sandbox": false, "app_sandbox": false},
                "infra": {"docker": {"reachable": true, "version": "27.1.1"}, "compose": {"version": "2.29.1", "supports_lane_overrides": true}},
                "proxy": {
                    "http": {"bound": true, "port": 80, "fallback_reason": null, "serving": false, "listeners": [
                        {"address": "127.0.0.1:80", "running": true, "accept_errors": 3, "last_error": "Too many open files (os error 24)"},
                        {"address": "[::1]:80", "running": false, "accept_errors": 0, "last_error": null},
                    ]},
                    "https": {"bound": true, "port": 443, "fallback_reason": null, "serving": true, "listeners": [
                        {"address": "127.0.0.1:443", "running": true, "accept_errors": 0, "last_error": null},
                    ]},
                },
                "ca": {"present": true, "kind": "devlanes", "path": "/home/u/.devlanes/ca/ca.pem", "trusted": "unknown",
                       "name_constrained": false,
                       "trust": [{"store": "system", "state": "untrusted"},
                                 {"store": "firefox", "state": "unknown", "reason": "certutil is not installed"}],
                       "hint": "the devlanes CA has no name constraints (it predates them), so a leaked key could sign certificates for any site",
                       "fix": "devlanes certs untrust && rm -r /home/u/.devlanes/ca && devlanes certs trust && devlanes daemon restart"},
                "route_warnings": [{"code": "host_rejected", "data": {"lane": "acme@2", "unit": "web", "host": "web_app.localhost",
                                    "message": "`web_app.localhost` contains `_`, which DNS names cannot; the route is left out"}}],
                "ca_warnings": [{"code": "host_outside_ca", "data": {
                    "lane": "acme@2", "unit": "api", "host": "api.example.com",
                    "permitted": ["localhost", "test"], "suggestion": "api.acme-2.localhost",
                }}],
                "projects": 2, "subscribers": 1,
                "hosts": {"mode": "auto", "probe": "loopback", "state": "applied", "names": 3, "desired": 3, "skipped": 0,
                          "last_error": null, "tld_error": null,
                          "helper": {"state": "mismatch", "level": "warning", "installed": "0.1.0", "expected": "0.2.0",
                                     "message": "The installed hosts helper is version 0.1.0 and devlanes is 0.2.0.",
                                     "fix": "devlanes certs trust --hosts"}},
                "user_config": {
                    "path": "/home/u/.devlanes/config.yaml", "valid": false,
                    "error": {"message": "unknown field `proxyy`", "file": "/home/u/.devlanes/config.yaml", "line": 4, "column": 1},
                    "warnings": [{"path": "tls.trust.1", "message": "mobile certificate trust is not supported yet; target ignored"}],
                },
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
                {"code": "host_outside_ca", "path": "services.api.hosts",
                 "message": "api.example.com is outside the names the devlanes CA can certify",
                 "host": "api.example.com", "suggestion": "api.acme.test"},
                {"code": "ports_pending", "path": "lanes", "lane": "acme@0",
                 "message": "the edited config needs a different port block"},
                {"code": "host_conflict", "path": "services.web.hosts", "host": "web.acme.localhost",
                 "lane": "shop@0", "other": "acme@0", "message": "web.acme.localhost is routed by shop@0 and acme@0"},
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

/// The daemon's shell warnings name their path in the sentence, so `config
/// check` prints them without a `path:` prefix.
#[test]
fn config_check_shell_fallback_is_not_prefixed() {
    let message = "`services.web.run` and 2 more run with cmd.exe: Git for Windows' sh.exe was not found, and POSIX syntax may fail there. Install Git for Windows, then run `devlanes daemon restart`.";
    let text = render_text(
        Method::ConfigCheck,
        &json!({"project": "acme", "path": "/work/acme/devlanes.yaml", "warnings": [
            {"code": "shell_fallback", "path": "services.web.run", "message": message},
        ]}),
    );
    assert!(text.contains(&format!("warn {message}")), "{text}");
    assert!(!text.contains("services.web.run: "), "{text}");
}

#[test]
fn every_result_shape_in_every_text_mode() {
    for (name, method, data) in fixtures() {
        let json = runtime(OutputMode::Json)
            .reporter()
            .result(&Report::new(method, &data))
            .expect("json");
        let mut printed = serde_json::from_str::<Value>(&json.stdout).expect("json output");
        // doctor adds the client's own output section (tested on its own).
        if method == Method::Doctor {
            printed.as_object_mut().map(|map| map.remove("output"));
        }
        assert_eq!(
            printed, data,
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

/// Doctor without a daemon: the CLI's own facts, the stopped daemon as a warning
/// (exit 0), and exit 1 once a local check fails.
#[test]
fn doctor_without_a_daemon() {
    let mut data = json!({
        "running": false, "version": "0.3.0", "home": "/home/u/.devlanes",
        "socket": "/home/u/.devlanes/run/daemon.sock", "db_path": "/home/u/.devlanes/state.db",
        "home_check": {"path": "/home/u/.devlanes", "exists": true, "mode": "700", "problem": null},
        "sandbox": {"codex_sandbox": false, "app_sandbox": false},
        "ca": {"present": true, "kind": "devlanes", "path": "/home/u/.devlanes/ca/ca.pem",
               "name_constrained": true, "hint": null, "fix": null,
               "trust": [{"store": "system", "state": "trusted"}]},
        "user_config": {"path": "/home/u/.devlanes/config.yaml", "valid": true, "error": null, "errors": [], "warnings": []},
        "port_holders": [{"listener": "https", "port": 443, "pid": 812, "command": "nginx"}],
        "hosts": {"helper": {"state": "ok", "level": "ok", "installed": "0.3.0", "expected": "0.3.0"}},
        "release": {"version": "0.3.0", "build": "local", "daemon_version": null, "install_method": "install.sh",
                    "check": null, "latest": null},
    });
    let text = render_text(Method::Doctor, &data);
    insta::assert_snapshot!("render_doctor_without_daemon", text);
    assert!(text.contains("--- plain (exit 0)"), "{text}");
    assert!(
        !text.contains("Docker") && !text.contains("schema") && !text.contains("projects"),
        "no daemon facts invented: {text}"
    );
    // A local failure still fails doctor.
    data["ca"]["trust"] = json!([{"store": "system", "state": "untrusted"}]);
    let text = render_text(Method::Doctor, &data);
    assert!(text.contains("--- plain (exit 1)"), "{text}");
    assert!(
        text.contains("fail tls: CA not trusted by system"),
        "{text}"
    );
}

/// Trust entries with a location (Windows) name the store and the dialog `certs
/// trust` shows, once. A CA in the deny list fails without a `certs trust` fix.
#[test]
fn doctor_names_the_store_and_its_dialog() {
    let approval = "choose Yes in the Windows security warning";
    let data = json!({
        "running": false, "version": "0.3.0", "home": r"C:\Users\u\.devlanes",
        "socket": r"\\.\pipe\devlanes-0", "db_path": r"C:\Users\u\.devlanes\state.db",
        "ca": {"present": true, "kind": "devlanes", "path": r"C:\Users\u\.devlanes\ca\ca.pem",
               "name_constrained": true, "hint": null, "fix": null,
               "trust": [
                   {"store": "system", "state": "untrusted", "location": "Windows user root store",
                    "approval": approval},
                   {"store": "windows", "state": "untrusted",
                    "location": r"Cert:\CurrentUser\devlanes-test", "approval": approval},
               ]},
    });
    let text = render_text(Method::Doctor, &data);
    for expected in [
        "fail tls: CA not trusted by system (Windows user root store)",
        "devlanes certs trust, then choose Yes in the Windows security warning",
        r"fail tls: CA not trusted by windows (Cert:\CurrentUser\devlanes-test)",
    ] {
        assert!(text.contains(expected), "{expected}:\n{text}");
    }
    for mode in text.split("--- ").filter(|mode| !mode.is_empty()) {
        assert!(
            mode.matches(approval).count() <= 1,
            "approval repeated:\n{mode}"
        );
    }

    let mut distrusted = data.clone();
    distrusted["ca"]["trust"] = json!([
        {"store": "system", "state": "distrusted", "location": "Windows user root store",
         "reason": r"Cert:\CurrentUser\Disallowed also holds it", "approval": approval},
    ]);
    let text = render_text(Method::Doctor, &distrusted);
    assert!(
        text.contains(
            r"fail tls: CA distrusted by system (Windows user root store): Cert:\CurrentUser\Disallowed also holds it"
        ),
        "{text}"
    );
    assert!(!text.contains(approval), "{text}");
}

/// Windows terminal facts. Without a daemon an elevated shell fails doctor and a
/// job that kills on close without breakaway is a warning; with one running, the
/// elevated shell is only a warning and the job is not mentioned. An ordinary
/// shell adds nothing. An open home fails (the daemon refuses it unless it is
/// empty), with the fix `home_check` carries, if any.
#[test]
fn doctor_terminal_and_acl_checks() {
    let base = json!({
        "running": false, "version": "0.3.0", "home": r"C:\Users\u\.devlanes",
        "socket": r"\\.\pipe\devlanes-0", "db_path": r"C:\Users\u\.devlanes\state.db",
        "home_check": {"path": r"C:\Users\u\.devlanes", "exists": true, "problem": null},
        "sandbox": {"codex_sandbox": false, "app_sandbox": false},
    });
    let data = |running: bool, terminal: Value| {
        let mut data = base.clone();
        data["running"] = json!(running);
        data["terminal"] = terminal;
        if running {
            // A running daemon reports its schema; without it doctor fails.
            data["schema_version"] = json!(1);
            data["supported_schema_version"] = json!(1);
        }
        data
    };
    let with =
        |running: bool, terminal: Value| render_text(Method::Doctor, &data(running, terminal));
    // The agent line after the one containing `needle`, where a fix would go.
    let after = |text: &str, needle: &str| {
        let agent = text.split("--- agent").nth(1).unwrap_or_default();
        let mut lines = agent.lines().skip_while(|line| !line.contains(needle));
        assert!(lines.next().is_some(), "{needle} missing\n{text}");
        lines.next().unwrap_or_default().to_owned()
    };
    let killing = json!({"elevated": false, "job": {"kill_on_close": true, "breakaway_ok": false}});

    let plain = with(false, json!({"elevated": false, "job": null}));
    assert!(!plain.contains("terminal"), "{plain}");
    assert!(plain.contains("--- plain (exit 0)"), "{plain}");
    let mut open = data(false, json!({"elevated": false, "job": null}));
    open["home_check"]["problem"] = json!("acl");
    let open_text = render_text(Method::Doctor, &open);
    assert!(open_text.contains("--- plain (exit 1)"), "{open_text}");
    assert!(
        open_text.contains(
            "fail home: C:\\Users\\u\\.devlanes lets other accounts in; the daemon refuses it unless it is empty"
        ),
        "{open_text}"
    );
    assert!(
        !after(&open_text, "lets other accounts in").contains("fix:"),
        "{open_text}"
    );
    let mut fixable = open;
    fixable["home_check"]["fix"] = json!(r"icacls C:\Users\u\.devlanes /inheritance:r");
    let fixable = render_text(Method::Doctor, &fixable);
    assert_eq!(
        after(&fixable, "lets other accounts in"),
        r"  fix: icacls C:\Users\u\.devlanes /inheritance:r",
        "{fixable}"
    );

    let elevated = with(false, json!({"elevated": true, "job": null}));
    for mode in ["human", "plain", "agent"] {
        assert!(
            elevated.contains(&format!("--- {mode} (exit 1)")),
            "{elevated}"
        );
    }
    assert!(
        elevated.contains(
            "fail terminal: this terminal runs as administrator; the daemon refuses to run elevated"
        ),
        "{elevated}"
    );
    assert_eq!(
        after(&elevated, "runs as administrator"),
        "  fix: run devlanes from a terminal that is not running as administrator",
        "{elevated}"
    );
    let elevated_running = with(true, json!({"elevated": true, "job": null}));
    assert!(
        elevated_running.contains("--- agent (exit 0)"),
        "{elevated_running}"
    );
    assert!(
        elevated_running.contains(
            "warn terminal: this terminal runs as administrator; it cannot start the daemon"
        ),
        "{elevated_running}"
    );
    assert!(
        !after(&elevated_running, "runs as administrator").contains("fix:"),
        "{elevated_running}"
    );
    // `DEVLANES_ALLOW_ELEVATED=1` (CI runners): a warning, not a failure.
    let allowed = with(
        false,
        json!({"elevated": true, "elevation_allowed": true, "job": null}),
    );
    assert!(allowed.contains("--- agent (exit 0)"), "{allowed}");
    assert!(
        allowed.contains(
            "warn terminal: this terminal runs as administrator, and DEVLANES_ALLOW_ELEVATED=1"
        ),
        "{allowed}"
    );

    let stopped = with(false, killing.clone());
    assert!(
        stopped.contains("warn terminal: this terminal stops every process it started"),
        "{stopped}"
    );
    assert_eq!(
        after(&stopped, "stops every process"),
        "  fix: start the daemon from a terminal outside this one (for example Windows Terminal), then devlanes daemon restart",
        "{stopped}"
    );
    let running = with(true, killing);
    assert!(!running.contains("stops every process"), "{running}");
    let escapable = with(
        false,
        json!({"elevated": false, "job": {"kill_on_close": true, "breakaway_ok": true}}),
    );
    assert!(!escapable.contains("stops every process"), "{escapable}");

    // JSON prints the facts unchanged; the exit code follows the same checks.
    for (running, exit_code) in [(false, 1), (true, 0)] {
        let data = data(running, json!({"elevated": true, "job": null}));
        let json = runtime(OutputMode::Json)
            .reporter()
            .result(&Report::new(Method::Doctor, &data))
            .expect("json");
        assert_eq!(json.exit_code, exit_code, "running: {running}");
        let printed = serde_json::from_str::<Value>(&json.stdout).expect("json output");
        assert_eq!(printed["terminal"], data["terminal"]);
        assert_eq!(printed["home_check"], data["home_check"]);
    }
}

/// The Windows build (`windows`): Windows 11 24H2 (build 26100) or later is a
/// note, anything older fails doctor with the fix.
#[test]
fn doctor_windows_build_check() {
    let data = |build: u64| {
        json!({
            "running": false, "version": "0.3.0", "home": r"C:\Users\u\.devlanes",
            "socket": r"\\.\pipe\devlanes-0", "db_path": r"C:\Users\u\.devlanes\state.db",
            "sandbox": {"codex_sandbox": false, "app_sandbox": false},
            "windows": {"build": build},
        })
    };
    let current = render_text(Method::Doctor, &data(26_100));
    assert!(
        current.contains("ok windows: Windows build 26100"),
        "{current}"
    );
    assert!(current.contains("--- agent (exit 0)"), "{current}");
    let old = render_text(Method::Doctor, &data(22_631));
    assert!(
        old.contains(
            "fail windows: Windows build 22631 is older than Windows 11 24H2 (build 26100), the oldest devlanes supports"
        ),
        "{old}"
    );
    assert!(
        old.contains("  fix: update Windows to Windows 11 24H2 or later"),
        "{old}"
    );
    assert!(old.contains("--- agent (exit 1)"), "{old}");
}

/// WSL facts (`wsl`, from the CLI inside a distro, and the daemon's
/// `windows_drive_worktrees`): interop on is a passing check; interop off, no
/// systemd, a CA Windows browsers do not trust and worktrees on Windows drives
/// are warnings with fixes; none fails doctor. Outside WSL nothing is added.
#[test]
fn doctor_wsl_checks() {
    let base = json!({
        "running": true, "version": "0.3.0", "pid": 42, "home": "/home/acme/.devlanes",
        "socket": "/home/acme/.devlanes/run/daemon.sock",
        "db_path": "/home/acme/.devlanes/state.db",
        "schema_version": 1, "supported_schema_version": 1,
        "sandbox": {"codex_sandbox": false, "app_sandbox": false},
        "proxy": {"http": {"bound": true, "port": 8080, "fallback_reason": "port 80 needs root"},
                  "https": {"bound": true, "port": 8443, "fallback_reason": "port 443 needs root"}},
        "ca": {"present": true, "path": "/home/acme/.devlanes/ca/ca.pem",
               "trust": [{"store": "system", "state": "trusted"}]},
        "hosts": {"mode": "auto", "state": "applied", "names": 2},
        "windows_drive_worktrees": [
            {"lane": "acme@2", "path": "/mnt/c/src/acme-2", "drive": r"C:\"},
        ],
    });
    let mut data = base.clone();
    data["wsl"] = json!({"distro": "Ubuntu", "version": 2, "interop": true, "systemd": false});
    let text = render_text(Method::Doctor, &data);
    insta::assert_snapshot!("render_doctor_wsl", text);
    assert!(text.contains("--- agent (exit 0)"), "{text}");

    // Outside WSL the same daemon facts add no wsl group.
    let outside = render_text(Method::Doctor, &base);
    assert!(!outside.contains("wsl:"), "{outside}");
    assert!(!outside.contains("Windows drive"), "{outside}");

    // Interop off; the windows store is configured, so the CA hint is absent.
    let mut quiet = data.clone();
    quiet["wsl"]["interop"] = json!(false);
    quiet["wsl"]["distro"] = Value::Null;
    quiet["wsl"]["systemd"] = json!(true);
    quiet["ca"]["trust"] =
        json!([{"store": "system", "state": "trusted"}, {"store": "windows", "state": "trusted"}]);
    quiet["windows_drive_worktrees"] = json!([]);
    let quiet = render_text(Method::Doctor, &quiet);
    assert!(
        quiet
            .contains("warn wsl: WSL 2, Windows interop off: devlanes cannot open URLs in Windows"),
        "{quiet}"
    );
    for absent in ["systemd", "tls.trust leaves out windows", "Windows drive"] {
        assert!(!quiet.contains(absent), "{absent}\n{quiet}");
    }
}

/// Port ranges Windows reserves (`excluded_ports`): a pool clear of them is a
/// passing check, overlaps with the pool, the proxy ports or mirrored box ports
/// are warnings with a fix, and an unreadable list (whole or one family) is a
/// warning too. None of them fails doctor.
#[test]
fn doctor_excluded_port_checks() {
    let with = |excluded: Value| {
        render_text(
            Method::Doctor,
            &json!({
                "running": false, "version": "0.3.0", "home": r"C:\Users\u\.devlanes",
                "socket": r"\\.\pipe\devlanes-0", "db_path": r"C:\Users\u\.devlanes\state.db",
                "sandbox": {"codex_sandbox": false, "app_sandbox": false},
                "excluded_ports": excluded,
            }),
        )
    };
    let after = |text: &str, needle: &str| {
        let agent = text.split("--- agent").nth(1).unwrap_or_default();
        let mut lines = agent.lines().skip_while(|line| !line.contains(needle));
        assert!(lines.next().is_some(), "{needle} missing\n{text}");
        lines.next().unwrap_or_default().to_owned()
    };
    let pool = json!({"start": 20000, "end": 32767});

    let clear = with(json!({
        "ranges": [{"start": 50000, "end": 50059}], "pool": pool,
        "pool_overlaps": [], "mirrored": [],
    }));
    assert!(
        clear.contains("ok ports: no port range Windows reserves overlaps ports.range 20000-32767"),
        "{clear}"
    );

    let overlapping = with(json!({
        "ranges": [{"start": 27339, "end": 27339}, {"start": 32700, "end": 32799}],
        "pool": pool,
        "pool_overlaps": [{"start": 27339, "end": 27339}, {"start": 32700, "end": 32799}],
        "mirrored": [27339, 32701],
        "proxy": [
            {"listener": "http", "port": 80, "fallback": false},
            {"listener": "https", "port": 8443, "fallback": true},
        ],
    }));
    insta::assert_snapshot!("render_doctor_excluded_ports", overlapping);
    assert!(
        overlapping.contains(
            "warn ports: proxy ports inside port ranges Windows reserves: 80 (proxy.http_port), 8443 (HTTPS fallback)"
        ),
        "{overlapping}"
    );
    assert!(
        overlapping.contains("new port blocks skip them, but the ranges can change after a reboot"),
        "{overlapping}"
    );
    assert!(overlapping.contains("--- agent (exit 0)"), "{overlapping}");
    assert!(
        overlapping.contains(
            "warn ports: ports.range 20000-32767 overlaps ports Windows reserves (27339, 32700-32799)"
        ),
        "{overlapping}"
    );
    assert!(
        after(&overlapping, "overlaps ports Windows reserves")
            .starts_with("  fix: set ports.range in ~/.devlanes/config.yaml clear of them"),
        "{overlapping}"
    );
    assert!(
        overlapping
            .contains("warn ports: box ports 27339, 32701 are inside port ranges Windows reserves"),
        "{overlapping}"
    );
    let one = with(json!({
        "ranges": [{"start": 27339, "end": 27339}], "pool": null,
        "pool_overlaps": [], "mirrored": [27339],
    }));
    assert!(
        one.contains("warn ports: box port 27339 is inside a port range"),
        "{one}"
    );
    assert!(!one.contains("overlaps"), "{one}");

    let failed = with(json!({"error": "netsh did not answer within 10s"}));
    assert!(
        failed.contains("warn ports: cannot read the port ranges Windows reserves: netsh did not answer within 10s"),
        "{failed}"
    );
    assert!(failed.contains("--- agent (exit 0)"), "{failed}");
    assert!(!failed.contains("ports.range"), "{failed}");

    // One family failed: its error shows next to the checks of what was read.
    let partial = with(json!({
        "ranges": [{"start": 27339, "end": 27339}], "pool": pool,
        "pool_overlaps": [{"start": 27339, "end": 27339}], "mirrored": [], "proxy": [],
        "error": "ipv6: netsh failed (exit code: 1)",
    }));
    for expected in [
        "warn ports: cannot read the port ranges Windows reserves: ipv6: netsh failed (exit code: 1)",
        "warn ports: ports.range 20000-32767 overlaps ports Windows reserves (27339)",
    ] {
        assert!(partial.contains(expected), "{expected}\n{partial}");
    }

    // Unix reports nothing, so nothing is rendered.
    let unix = with(Value::Null);
    assert!(!unix.contains("ports:"), "{unix}");
}

/// `STATUS_CONTROL_C_EXIT` (0xC000013A as a signed exit code) reads as
/// `interrupted (Ctrl-C)` in task results, error lists and reasons; the data
/// stays the number. Agent status lines keep `exit=<code>` so their
/// `key=value` pairs still scan, and name Ctrl-C after it.
#[test]
fn ctrl_c_exits_read_as_interrupted() {
    const CTRL_C: i64 = -1_073_741_510;
    let cases = [
        (
            Method::Run,
            json!({"task": "test", "lane": "acme@2", "unit": "test", "interactive": false, "exit_code": CTRL_C, "state": "failed"}),
            Some("failed acme@2:test exit=-1073741510 (Ctrl-C)\n"),
        ),
        (
            Method::Errors,
            json!({"items": [{"lane": "acme@2", "unit": "admin", "state": "failed", "exit_code": CTRL_C,
                "crashes": 1, "reason": null, "error_lines": [], "port_conflicts": []}],
                "truncated": false, "next_cursor": null}),
            Some("failed acme@2:admin exit=-1073741510 (Ctrl-C) crashes=1\n"),
        ),
        (
            Method::Why,
            json!({"lane": "acme@2", "unit": "admin", "state": "failed",
                "reasons": [{"code": "exited", "data": {"code": CTRL_C, "signal": null}}]}),
            None,
        ),
    ];
    for (method, data, agent_line) in cases {
        let text = render_text(method, &data);
        let (text, agent) = text.split_once("--- agent").expect("agent section");
        assert!(text.contains("interrupted (Ctrl-C)"), "{text}");
        assert!(!text.contains("1073741510"), "{text}");
        match agent_line {
            Some(line) => {
                assert!(agent.contains(line), "{agent}");
                assert!(!agent.contains("interrupted"), "{agent}");
            }
            None => {
                assert!(agent.contains("interrupted (Ctrl-C)"), "{agent}");
                assert!(!agent.contains("1073741510"), "{agent}");
            }
        }
        let json = runtime(OutputMode::Json)
            .reporter()
            .result(&Report::new(method, &data))
            .expect("json");
        assert!(json.stdout.contains("-1073741510"), "{}", json.stdout);
    }
    // Ordinary codes keep their number.
    let text = render_text(
        Method::Run,
        &json!({"task": "test", "lane": "acme@2", "unit": "test", "interactive": false, "exit_code": 2, "state": "failed"}),
    );
    assert!(text.contains("exit=2"), "{text}");
}

/// The Windows logon entry (`autostart`): `daemon status` names it in one line
/// and doctor as a check; an entry for another devlanes or home is a warning
/// with the fix. Without the facts (Unix) neither mentions logon.
#[test]
fn logon_entry_in_status_and_doctor() {
    let entry = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run\devlanes";
    let current = json!({"installed": true, "entry": entry, "current": true,
        "command": r#""C:\devlanes\devlanesw.exe" --home "C:\Users\u\.devlanes""#});
    let stale = json!({"installed": true, "entry": entry, "current": false,
        "command": r#""C:\old\devlanesw.exe" --home "C:\Users\u\.devlanes""#});
    let missing = json!({"installed": false, "entry": entry, "current": false});
    let status = |autostart: &Value| {
        let mut data = json!({"version": "0.3.0", "pid": 7, "socket": r"\\.\pipe\devlanes-0"});
        if !autostart.is_null() {
            data["autostart"] = autostart.clone();
        }
        render_text(Method::DaemonStatus, &data)
    };
    let text = status(&current);
    assert!(
        text.contains(&format!("logon: starts at logon ({entry})")),
        "{text}"
    );
    let text = status(&missing);
    assert!(
        text.contains("logon: does not start at logon (devlanes daemon install adds that)"),
        "{text}"
    );
    assert!(!status(&Value::Null).contains("logon"));

    let doctor = |autostart: &Value| {
        let mut data = json!({
            "running": false, "version": "0.3.0", "home": r"C:\Users\u\.devlanes",
            "socket": r"\\.\pipe\devlanes-0",
            "sandbox": {"codex_sandbox": false, "app_sandbox": false},
        });
        if !autostart.is_null() {
            data["autostart"] = autostart.clone();
        }
        render_text(Method::Doctor, &data)
    };
    let text = doctor(&current);
    assert!(
        text.contains(&format!("ok daemon: starts at logon ({entry})")),
        "{text}"
    );
    let text = doctor(&stale);
    assert!(
        text.contains(
            r#"warn daemon: starts at logon with another devlanes or home: "C:\old\devlanesw.exe""#
        ),
        "{text}"
    );
    assert!(text.contains("  fix: devlanes daemon install"), "{text}");
    assert!(text.contains("--- agent (exit 0)"), "{text}");
    assert!(!doctor(&Value::Null).contains("logon"));

    // Turned off in Task Manager: the entry exists but Explorer skips it.
    let mut disabled = current.clone();
    disabled["disabled"] = json!(true);
    let text = doctor(&disabled);
    assert!(
        text.contains(&format!(
            "warn daemon: the logon entry ({entry}) is turned off in Task Manager"
        )),
        "{text}"
    );
    assert!(text.contains("  fix: devlanes daemon install"), "{text}");
    assert!(
        status(&disabled).contains("logon: the logon entry"),
        "{}",
        status(&disabled)
    );
}

#[test]
/// `daemon stop` names what it stopped and the `lane.stop` tasks that failed.
fn daemon_stop_reports_units_and_failed_tasks() {
    let data = json!({
        "stopping": true, "processes": 2, "reason": "explicit",
        "stopped": [{"lane": "acme@0", "unit": "api"}, {"lane": "acme@0", "unit": "web"}],
        "tasks": [
            {"lane": "acme@0", "task": "flush", "exit_code": 0},
            {"lane": "acme@0", "task": "notify", "exit_code": 3},
            {"lane": "acme@1", "task": "slow", "error": {"code": "cancelled", "data": null}},
        ],
    });
    let text = render_text(Method::DaemonShutdown, &data);
    assert!(text.contains("stopped the daemon and 2 units"), "{text}");
    assert!(text.contains("ran 3 lane.stop tasks"), "{text}");
    assert!(text.contains("acme@0 notify: exit 3"), "{text}");
    assert!(text.contains("acme@1 slow: cancelled"), "{text}");
    assert!(!text.contains("flush"), "{text}");
    let idle = json!({"stopping": true, "processes": 0, "stopped": [], "tasks": []});
    let text = render_text(Method::DaemonShutdown, &idle);
    assert!(text.contains("stopped the daemon\n"), "{text}");
    assert!(!text.contains("lane.stop"), "{text}");
}

#[test]
/// A stop timeout that does not fit in the service manager's stop window is a
/// warning naming the unit and the fix; doctor still exits 0.
fn doctor_warns_about_stop_timeouts_past_the_service_window() {
    let service = |installed: bool, window: u64| {
        json!({
            "manager": "launchd agent", "path": "/u/Library/LaunchAgents/run.devlanes.daemon.plist",
            "installed": installed, "stop_window_s": window,
            "window_source": if window == 60 { "installed" } else { "default" },
            "recommended_window_s": 60,
        })
    };
    let timeouts = json!([
        {"project": "acme", "unit": "api", "timeout_ms": 10_000},
        {"project": "acme", "unit": "db", "timeout_ms": 90_000},
    ]);
    // `running: false` keeps the daemon's own checks (schema) out of the exit code.
    let data = json!({"running": false, "service": service(true, 60), "stop_timeouts": timeouts});
    let text = render_text(Method::Doctor, &data);
    assert!(text.contains("--- plain (exit 0)"), "{text}");
    assert!(text.contains("the daemon gets 60s to stop"), "{text}");
    assert!(
        text.contains("1 unit with a stop timeout longer than the 60s the launchd agent gives the daemon to stop: acme:db (90s)"),
        "{text}"
    );
    assert!(
        text.contains("lower services.<name>.stop.timeout to 55s or less"),
        "{text}"
    );
    assert!(!text.contains("acme:api"), "{text}");

    let data = json!({"service": service(false, 5), "stop_timeouts": timeouts});
    let text = render_text(Method::Doctor, &data);
    assert!(
        text.contains("2 units with a stop timeout longer than the 5s a launchd agent gets by default (devlanes is not installed as a service): acme:api (10s), acme:db (90s)"),
        "{text}"
    );
    assert!(
        text.contains(
            "devlanes daemon install, and lower services.<name>.stop.timeout to 55s or less"
        ),
        "{text}"
    );

    let data = json!({"service": service(true, 5), "stop_timeouts": [timeouts[0].clone()]});
    let text = render_text(Method::Doctor, &data);
    assert!(
        text.contains("gives the daemon 5s to stop, its default; devlanes now installs 60s"),
        "{text}"
    );
    assert!(
        text.contains("devlanes daemon install, which gives the daemon 60s"),
        "{text}"
    );
}

#[test]
/// A human terminal with colors gets a unit's colors back, each line ending in a
/// reset and nothing but SGR kept; every other output gets plain text.
fn logs_keep_colors_only_where_they_show() {
    let line = json!({"unit": "api", "seq": 7, "timestamp_ms": 1, "level": "info",
        "text": "INFO ready", "styled": "\u{1b}[32mINFO\u{1b}[0m \u{1b}[2Kready\u{1b}[0m"});
    let page = json!({"items": [line], "truncated": false, "next_cursor": null});
    let text = colored_runtime()
        .reporter()
        .result(&Report::new(Method::Logs, &page))
        .expect("render")
        .stdout;
    assert_eq!(text, "api#7 \u{1b}[32mINFO\u{1b}[0m ready\u{1b}[0m\n");
    for mode in [OutputMode::Human, OutputMode::Plain, OutputMode::Agent] {
        let text = runtime(mode)
            .reporter()
            .result(&Report::new(Method::Logs, &page))
            .expect("render")
            .stdout;
        assert!(!text.contains('\u{1b}'), "{mode:?}: {text:?}");
        assert!(text.contains("api#7 INFO ready"), "{mode:?}: {text:?}");
    }
}

/// A human runtime on a terminal, with colors.
fn colored_runtime() -> Runtime {
    let cli = Cli::parse_from(["devlanes"]);
    Runtime::resolve(
        &cli,
        Signals {
            env_output: Some("human".into()),
            stdout_tty: true,
            stdin_tty: true,
            ..Signals::default()
        },
    )
    .expect("runtime")
}

#[test]
/// Failure tails of `start` and `wait` show the unit's colors on a colored
/// terminal, and plain text everywhere else.
fn failure_tails_keep_colors_only_where_they_show() {
    let styled = "\u{1b}[31mError\u{1b}[0m: boom\u{1b}[0m";
    let start = json!({
        "lane": "acme@0", "ready": false, "started": ["api"],
        "failures": [{"lane": "acme@0", "unit": "api", "exit_code": 1,
            "log_tail": ["Error: boom"], "log_tail_styled": [styled]}],
    });
    let wait = json!({
        "lane": "acme@0", "unit": "api", "state": "failed", "ready": false, "failed": ["api"],
        "exit_code": 1, "log_tail": ["Error: boom"], "log_tail_styled": [styled],
    });
    for (method, data) in [(Method::Start, &start), (Method::Wait, &wait)] {
        let text = colored_runtime()
            .reporter()
            .result(&Report::new(method, data))
            .expect("render")
            .stdout;
        assert!(text.contains(styled), "{method:?}: {text:?}");
        for mode in [OutputMode::Human, OutputMode::Plain, OutputMode::Agent] {
            let text = runtime(mode)
                .reporter()
                .result(&Report::new(method, data))
                .expect("render")
                .stdout;
            assert!(!text.contains('\u{1b}'), "{method:?} {mode:?}: {text:?}");
            assert!(
                text.contains("Error: boom"),
                "{method:?} {mode:?}: {text:?}"
            );
        }
    }
}

/// Splits [`render_text`] output into its human, plain and agent parts.
fn modes(text: &str) -> Vec<&str> {
    text.split("--- ").filter(|part| !part.is_empty()).collect()
}

#[test]
/// People see the display name and `running` for a unit without a readiness
/// check; plain and agent output keep the key and the state word.
fn human_status_shows_display_names_and_running() {
    let mut api = unit("api", "ready");
    api["display_name"] = json!("Billing API");
    api["ready_check"] = json!("started");
    let mut web = unit("web", "ready");
    web["ready_check"] = json!("tcp");
    let data = json!({"items": [lane(Some(2), "feat/billing", "running", vec![api, web])],
                      "truncated": false, "next_cursor": null});
    let text = render_text(Method::Status, &data);
    let parts = modes(&text);
    let human = parts[0];
    assert!(
        human.contains("Billing API") && human.contains("running"),
        "{human}"
    );
    assert!(!human.contains(" api "), "{human}");
    for other in &parts[1..] {
        assert!(!other.contains("Billing API"), "{other}");
    }
}

#[test]
/// A start that found everything up says so instead of looking like a fresh start.
fn start_of_a_running_lane_says_already_running() {
    let data = json!({
        "lane": "acme@2", "branch": "feat/billing", "started": [],
        "already_running": ["acme@2:api"], "whole_lane": false, "ready": true, "timed_out": false,
        "failures": [], "skipped_tasks": [], "units": [unit("api", "ready")],
    });
    let text = render_text(Method::Start, &data);
    let parts = modes(&text);
    assert!(
        parts[0].contains("already running; nothing was started"),
        "{text}"
    );
    assert!(
        parts[2].contains("already running (nothing started)"),
        "{text}"
    );
}

#[test]
fn doctor_says_how_to_recreate_an_unconstrained_ca() {
    let data = json!({
        "ca": {
            "present": true, "kind": "devlanes", "path": "/home/u/.devlanes/ca/ca.pem",
            "name_constrained": false,
            "hint": "the devlanes CA has no name constraints",
            "fix": "devlanes certs untrust && rm -r /home/u/.devlanes/ca && devlanes certs trust && devlanes daemon restart",
        },
    });
    let text = render_text(Method::Doctor, &data);
    assert!(
        text.contains("the devlanes CA has no name constraints"),
        "{text}"
    );
    assert!(text.contains("rm -r /home/u/.devlanes/ca"), "{text}");
    let constrained = json!({"ca": {"present": true, "path": "/x/ca.pem", "name_constrained": true, "hint": null}});
    assert!(!render_text(Method::Doctor, &constrained).contains("name constraints"));
}

#[test]
fn doctor_shows_release_facts_and_the_upgrade_command() {
    let release = |update_available: bool, daemon: &str| {
        json!({"release": {
            "version": "0.2.1", "build": "v0.2.1", "daemon_version": daemon,
            "install_method": "Homebrew", "command": "brew upgrade devlanes-run/tap/devlanes",
            "check": "ok", "check_off_reason": null, "latest": "0.3.0",
            "checked_at": null, "update_available": update_available,
        }})
    };
    let text = render_text(Method::Doctor, &release(true, "0.2.1"));
    assert!(
        text.contains("devlanes 0.2.1 (release build v0.2.1), installed with Homebrew"),
        "{text}"
    );
    assert!(text.contains("devlanes 0.3.0 is available"), "{text}");
    assert!(
        text.contains("fix: brew upgrade devlanes-run/tap/devlanes"),
        "{text}"
    );
    assert!(!text.contains("the daemon runs"), "{text}");

    let text = render_text(Method::Doctor, &release(false, "0.2.0"));
    assert!(text.contains("latest release 0.3.0"), "{text}");
    assert!(
        text.contains("the daemon runs 0.2.0, this CLI is 0.2.1"),
        "{text}"
    );
    assert!(
        text.contains("devlanes daemon restart when convenient (it stops services)"),
        "{text}"
    );

    let mut upgraded = release(false, "0.2.1");
    upgraded["release"]["old_copies"] = json!({
        "removed": [r"C:\Users\u\AppData\Local\Programs\devlanes\devlanesw.exe.old-20261022120000"],
        "in_use": [r"C:\Users\u\AppData\Local\Programs\devlanes\devlanes.exe.old-20261022120000"],
    });
    let text = render_text(Method::Doctor, &upgraded);
    assert!(
        text.contains("deleted 1 old executable left by an upgrade"),
        "{text}"
    );
    assert!(
        text.contains(r"the previous version still runs from C:\Users\u\AppData\Local\Programs\devlanes\devlanes.exe.old-20261022120000"),
        "{text}"
    );
    assert!(
        text.contains("doctor deletes the old files afterwards"),
        "{text}"
    );

    let off = json!({"release": {"version": "0.2.1", "build": "local", "install_method": "install.sh",
        "check": "off", "check_off_reason": "config"}});
    let text = render_text(Method::Doctor, &off);
    assert!(text.contains("(local build)"), "{text}");
    assert!(
        text.contains("release checks are off (updates.check: false)"),
        "{text}"
    );
}

#[test]
fn leftover_processes_get_a_one_line_note() {
    let pg = json!([{"pid": 4242, "command": "pg_ctl"}]);
    let run = json!({
        "task": "boot", "lane": "acme@2", "unit": null, "interactive": false,
        "exit_code": 0, "state": "exited",
        "leftovers": [{"unit": "db", "left_running": pg}, {"unit": "boot", "escaped": [{"pid": 51, "command": "node"}]}],
    });
    let text = render_text(Method::Run, &run);
    assert!(
        text.contains("  db: left running: pg_ctl (pid 4242)"),
        "{text}"
    );
    assert!(text.contains("  escaped: node (pid 51)"), "{text}");

    let mut start = start_result();
    start["leftovers"] = json!([{"unit": "setup.db", "left_running": pg}]);
    let text = render_text(Method::Start, &start);
    assert_eq!(
        text.matches("setup.db: left running: pg_ctl (pid 4242)")
            .count(),
        3,
        "{text}"
    );

    let mut task = unit("db-start", "exited");
    task["kind"] = json!("task");
    task["left_running"] = pg.clone();
    let mut status = lane(
        Some(2),
        "feat/billing",
        "running",
        vec![unit("api", "ready"), task],
    );
    status["containers"] = json!([
        {"name": "postgres", "ports": {"default": 20150}, "cpu_percent": 1.5, "memory_bytes": 52_000_000},
        {"name": "redis", "ports": {}, "shared": true, "cpu_percent": 0.5, "memory_bytes": 8_000_000},
        {"name": "mail", "ports": {}},
    ]);
    let text = render_text(Method::Status, &json!({"items": [status]}));
    assert!(
        text.contains("db-start: left running: pg_ctl (pid 4242)"),
        "{text}"
    );
    let human = text.split("--- plain").next().unwrap_or_default();
    assert!(human.contains("postgres  container"), "{human}");
    assert!(human.contains("redis     shared container"), "{human}");
    assert!(
        !human.contains("mail"),
        "unsampled containers stay out\n{human}"
    );

    let why = json!({
        "lane": "acme@2", "unit": "db-start", "state": "stopped", "reasons": [],
        "left_running": pg,
    });
    let text = render_text(Method::Why, &why);
    assert!(text.contains("  left running: pg_ctl (pid 4242)"), "{text}");
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
        "warn",
        "next:",
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

/// A matching helper and a valid user config show as ok; a failed /etc/hosts write
/// that already blames the helper version is not repeated by the helper check.
#[test]
fn doctor_helper_and_user_config_checks() {
    let data = json!({
        "hosts": {"mode": "auto", "probe": "loopback", "state": "applied", "names": 2, "last_error": null,
                  "helper": {"state": "ok", "level": "ok", "installed": "0.2.0", "expected": "0.2.0"}},
        "user_config": {"path": "/home/u/.devlanes/config.yaml", "valid": true, "error": null, "warnings": []},
    });
    let text = render_text(Method::Doctor, &data);
    assert!(text.contains("ok hosts: hosts helper 0.2.0"), "{text}");
    assert!(
        text.contains("ok config: user config /home/u/.devlanes/config.yaml"),
        "{text}"
    );
    let failed = json!({"hosts": {"mode": "auto", "state": "failed",
        "last_error": {"code": "helper_version_mismatch", "message": "The installed hosts helper is version 0.1.0 and devlanes is 0.2.0."},
        "helper": {"state": "mismatch", "level": "warning", "installed": "0.1.0", "expected": "0.2.0"}}});
    let text = render_text(Method::Doctor, &failed);
    assert!(
        !text.contains("helper is version 0.1.0, devlanes"),
        "{text}"
    );
    assert!(text.contains("fix: devlanes certs trust --hosts"), "{text}");
}

/// The resolver strategy's DNS server and the OS resolver file, from `dns`.
#[test]
fn doctor_checks_the_dns_server_and_the_resolver_file() {
    let dns = |file: Value, running: bool| {
        json!({"dns": {"tld": "test", "listen": "127.0.0.1:15353", "running": running,
            "error": if running { Value::Null } else { json!("address in use") },
            "resolver_file": file}})
    };
    let ok = dns(
        json!({"path": "/etc/resolver/test", "installed": true, "matches": true}),
        true,
    );
    let text = render_text(Method::Doctor, &ok);
    assert!(
        text.contains(
            "ok dns: DNS server for .test on 127.0.0.1:15353, resolver file /etc/resolver/test"
        ),
        "{text}"
    );
    let missing = dns(
        json!({"path": "/etc/resolver/test", "installed": false, "matches": false}),
        true,
    );
    let text = render_text(Method::Doctor, &missing);
    assert!(
        text.contains(
            "warn dns: .test lookups do not reach the devlanes DNS server: nothing at /etc/resolver/test"
        ),
        "{text}"
    );
    assert!(
        text.contains("fix: devlanes certs trust --resolver"),
        "{text}"
    );
    let down = dns(Value::Null, false);
    let checks = doctor_checks_of(&down);
    let check = checks.iter().find(|c| c.group == "dns").expect("dns check");
    assert_eq!(check.state, DoctorState::Fail);
    assert_eq!(
        check.text,
        "DNS server for .test is not running on 127.0.0.1:15353: address in use"
    );
    // Without the resolver strategy there is nothing to say.
    assert!(
        !doctor_checks_of(&json!({}))
            .iter()
            .any(|c| c.group == "dns")
    );
    // Other clients get the same lines, minus this command's output mode.
    assert!(!checks.iter().any(|c| c.group == "output"));
}

/// On Windows the daemon reports the NRPT rule (`nrpt_rule`) where Unix has
/// the resolver file: the same verdicts and fix, plus what overrides a local
/// rule (another program's rule, Group Policy).
#[test]
fn doctor_checks_the_nrpt_rule() {
    let lines = |rule: Value| {
        let data = json!({"dns": {"tld": "test", "listen": "127.53.0.1:53", "running": true,
            "error": null, "nrpt_rule": rule}});
        doctor_checks_of(&data)
            .into_iter()
            .filter(|check| check.group == "dns")
            .map(|check| match check.fix {
                Some(fix) => format!("{:?} {} (fix: {fix})", check.state, check.text),
                None => format!("{:?} {}", check.state, check.text),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let rule = |changes: Value| {
        let mut rule = json!({"namespace": ".test", "server": "127.53.0.1", "installed": true,
            "matches": true, "foreign": false, "group_policy": false});
        for (key, value) in changes.as_object().expect("object") {
            rule[key] = value.clone();
        }
        rule
    };
    let output = [
        rule(json!({})),
        rule(json!({"installed": false, "matches": false})),
        rule(json!({"matches": false})),
        rule(json!({"foreign": true})),
        rule(json!({"group_policy": true})),
        json!({"namespace": ".test", "error": "Get-DnsClientNrptRule did not answer within 20 s"}),
    ]
    .into_iter()
    .map(lines)
    .collect::<Vec<_>>()
    .join("\n");
    insta::assert_snapshot!(output, @r"
    Ok DNS server for .test on 127.53.0.1:53, NRPT rule .test
    Warn .test lookups do not reach the devlanes DNS server: no NRPT rule for .test (fix: devlanes certs trust --resolver)
    Warn the NRPT rule for .test sends .test lookups elsewhere, not to 127.53.0.1:53 (fix: devlanes certs trust --resolver)
    Warn an NRPT rule for .test was not added by devlanes; it sends .test lookups elsewhere (fix: remove it yourself (Get-DnsClientNrptRule lists it) or choose another hostnames.tld)
    Warn the NRPT rule for .test is in place, but Windows ignores it while Group Policy NRPT rules exist (fix: ask the administrator to add .test -> 127.53.0.1 to the Group Policy NRPT rules)
    Warn cannot read the NRPT rules for .test: Get-DnsClientNrptRule did not answer within 20 s (fix: run Get-DnsClientNrptRule in PowerShell to see them)
    ");
    // The CLI prints the same words, with the fix on its own line.
    let text = render_text(
        Method::Doctor,
        &json!({"dns": {"tld": "test", "listen": "127.53.0.1:53", "running": true,
            "error": null, "nrpt_rule": rule(json!({"installed": false, "matches": false}))}}),
    );
    assert!(
        text.contains(
            "warn dns: .test lookups do not reach the devlanes DNS server: no NRPT rule for .test"
        ),
        "{text}"
    );
    assert!(
        text.contains("fix: devlanes certs trust --resolver"),
        "{text}"
    );
    // A server on another port is out of an NRPT rule's reach.
    let text = render_text(
        Method::Doctor,
        &json!({"dns": {"tld": "test", "listen": "127.53.0.1:5353", "running": true,
            "error": null, "nrpt_rule": rule(json!({"matches": false}))}}),
    );
    assert!(
        text.contains(
            "warn dns: .test lookups do not reach the devlanes DNS server: it listens on \
             127.53.0.1:5353, and an NRPT rule can only reach port 53"
        ),
        "{text}"
    );
    // A daemon without the rule's facts still reports its server.
    let text = render_text(
        Method::Doctor,
        &json!({"dns": {"tld": "test", "listen": "127.53.0.1:53", "running": true,
            "error": null, "nrpt_rule": null}}),
    );
    assert!(
        text.contains("ok dns: DNS server for .test on 127.53.0.1:53"),
        "{text}"
    );
}

/// `lane.warning` events are one line in every mode; outside a start they name the lane.
#[test]
fn lane_warning_events_are_one_line() {
    let event = Event {
        seq: 1,
        event: "lane.warning".into(),
        data: json!({"lane": "acme@2", "code": "host_outside_ca", "data": {
            "lane": "acme@2", "unit": "api", "host": "api.example.com",
            "permitted": ["localhost"], "suggestion": "api.acme.test",
        }}),
    };
    let mut output = String::new();
    for mode in [OutputMode::Human, OutputMode::Plain, OutputMode::Agent] {
        for method in [Method::Start, Method::Subscribe] {
            output.push_str(
                &runtime(mode)
                    .reporter()
                    .event(method, &event)
                    .expect("event")
                    .stdout,
            );
        }
    }
    insta::assert_snapshot!(output, @r"
    ! api.example.com is outside the CA's names; HTTPS will fail; try api.acme.test
    ! acme@2: api.example.com is outside the CA's names; HTTPS will fail; try api.acme.test
    warn api.example.com is outside the CA's names; HTTPS will fail; try api.acme.test
    warn acme@2: api.example.com is outside the CA's names; HTTPS will fail; try api.acme.test
    warn acme@2: api.example.com is outside the CA's names; HTTPS will fail; try api.acme.test
    ");
    // Agents see a start's warnings once, in its result.
}

/// A worktree on a Windows drive inside WSL warns at start with the path, the
/// drive and where to move it.
#[test]
fn windows_drive_warning_names_the_drive() {
    let event = Event {
        seq: 1,
        event: "lane.warning".into(),
        data: json!({"lane": "acme@2", "code": "windows_drive", "data": {
            "lane": "acme@2", "path": "/mnt/c/src/acme-2", "drive": r"C:\",
        }}),
    };
    let line = runtime(OutputMode::Plain)
        .reporter()
        .event(Method::Subscribe, &event)
        .expect("event")
        .stdout;
    assert_eq!(
        line.trim_end(),
        r"warn acme@2: /mnt/c/src/acme-2 is on the Windows drive C:\: file access is slow and file watching misses edits made from Windows; move the repository to the Linux filesystem (for example ~/src)"
    );
}

/// `daemon install` inside a WSL distro without systemd says how to turn it on.
#[test]
fn wsl_systemd_off_says_how_to_enable_it() {
    let rendered = runtime(OutputMode::Plain)
        .reporter()
        .error(&RpcError::new("wsl_systemd_off", json!({}), 1), None);
    assert!(
        rendered
            .stderr
            .contains("systemd is not running in this WSL distro, and daemon install needs it"),
        "{}",
        rendered.stderr
    );
    assert!(
        rendered
            .stderr
            .contains("set systemd=true under [boot] in /etc/wsl.conf"),
        "{}",
        rendered.stderr
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
        RpcError::new(
            "protocol_mismatch",
            json!({"protocol": 7, "client_protocol": 8, "version": "0.2.0", "build_id": "a1b2c3", "managed_processes": 6}),
            4,
        ),
        RpcError::new(
            "protocol_mismatch",
            json!({"protocol": 7, "client_protocol": 8, "version": "0.2.0", "build_id": "a1b2c3", "managed_processes": 0}),
            4,
        ),
        RpcError::new("start_cancelled", json!({"lane": "acme@2"}), 1),
        RpcError::new(
            "no_config",
            json!({"path": "/work/acme/src", "searched_to": "/work/acme"}),
            3,
        ),
        RpcError::new(
            "unknown_profile",
            json!({"profile": "e2r", "candidates": ["dev", "e2e", "full"]}),
            2,
        ),
        RpcError::new(
            "unknown_profile",
            json!({"profile": "staging", "candidates": ["dev", "e2e"]}),
            2,
        ),
        RpcError::new("port_error", json!({"unit": "api", "port": "metrics"}), 1),
        RpcError::new(
            "unit_running",
            json!({"lane": "acme@2", "unit": "probe"}),
            1,
        ),
        RpcError::new("worktree_missing", json!({"path": "/work/gone"}), 3),
        RpcError::new(
            "phase_failed",
            json!({"lane": "acme@2", "phase": "setup", "error": {"code": "task_failed", "data": {"task": "migrate", "exit_code": 2}}}),
            1,
        ),
        RpcError::new(
            "start_failed",
            json!({"lane": "acme@2", "units": ["acme@2:api"]}),
            1,
        ),
        RpcError::new(
            "unsafe_home",
            json!({"path": "/home/u/.devlanes", "problem": "mode", "mode": "755"}),
            4,
        ),
        RpcError::new(
            "spawn_failed",
            json!({"program": "missing-cmd", "message": "No such file or directory (os error 2)"}),
            127,
        ),
        RpcError::new(
            "lane_running",
            json!({"lane": "acme@2", "units": ["api", "worker"]}),
            1,
        ),
        RpcError::new(
            "daemon_stopping",
            json!({"message": "the daemon is shutting down; retry once it is gone"}),
            4,
        ),
        RpcError::new(
            "name_taken",
            json!({"name": "api", "kind": "service", "lane": "acme@2"}),
            2,
        ),
        RpcError::new(
            "name_taken",
            json!({"name": "seed", "kind": "task", "lane": "acme@2"}),
            2,
        ),
        RpcError::new(
            "response_too_large",
            json!({"limit_bytes": 16_777_216, "message": "a message exceeded the 16 MiB frame limit"}),
            1,
        ),
        RpcError::new("client_disconnected", Value::Null, 4),
        RpcError::new(
            "helper_version_mismatch",
            json!({"message": "The installed hosts helper is version 0.1.0 and devlanes is 0.2.0. Run `devlanes certs trust --hosts` to reinstall it.",
                   "fix": "devlanes certs trust --hosts",
                   "helper": {"state": "mismatch", "level": "warning", "installed": "0.1.0", "expected": "0.2.0"}}),
            1,
        ),
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

/// The Windows hosts service's codes: errors carry the daemon's or installer's
/// message and point at `certs trust --hosts`; `why` names the affected hosts.
#[test]
fn hosts_service_codes_are_worded() {
    for code in [
        "service_missing",
        "service_denied",
        "service_untrusted",
        "service_source_missing",
        "service_source_rejected",
        "elevation_cancelled",
    ] {
        assert!(has_wording(code), "{code}");
    }
    let reporter = runtime(OutputMode::Plain).reporter();
    let missing = reporter.error(
        &RpcError::new(
            "service_missing",
            json!({"message": "the devlanes hosts service is not running"}),
            1,
        ),
        None,
    );
    assert!(
        missing
            .stderr
            .contains("the devlanes hosts service is not running"),
        "{missing:?}"
    );
    assert!(
        missing.stderr.contains("devlanes certs trust --hosts"),
        "{missing:?}"
    );
    let cancelled = reporter.error(&RpcError::new("elevation_cancelled", json!({}), 1), None);
    assert!(
        cancelled
            .stderr
            .contains("administrator prompt was declined"),
        "{cancelled:?}"
    );
    let why = json!({
        "lane": "acme@2", "unit": "web", "state": "running",
        "reasons": [
            {"code": "service_missing", "data": {"hosts": ["web.acme-2.localhost"], "message": "m"}},
            {"code": "service_denied", "data": {"hosts": ["web.acme-2.localhost"], "message": "m"}},
            {"code": "service_untrusted", "data": {"hosts": ["web.acme-2.localhost"], "message": "m"}},
        ],
    });
    let text = render_text(Method::Why, &why);
    for line in [
        "the devlanes hosts service is not installed or not running, so web.acme-2.localhost may not resolve",
        "the devlanes hosts service serves another user, not this one, so web.acme-2.localhost may not resolve",
        "the hosts service's pipe is held by another program, not the devlanes service, so web.acme-2.localhost may not resolve",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    assert!(text.contains("devlanes certs trust --hosts"), "{text}");
}
