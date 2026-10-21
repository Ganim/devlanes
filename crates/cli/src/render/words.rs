//! Wording for daemon error codes and `why` reason codes. The daemon only sends
//! codes with data; every sentence and hint lives here.

use devlanes_proto::RpcError;
use serde_json::{Value, json};

use super::{compact_fields, display_value, list, text};
use crate::Hint;

/// A field as text, `unknown` when absent.
fn field(data: &Value, key: &str) -> String {
    data.get(key)
        .filter(|value| !value.is_null())
        .map(display_value)
        .unwrap_or_else(|| "unknown".into())
}

/// `data.lane:data.unit`, or whichever part exists.
fn target(data: &Value) -> String {
    match (text(data, "lane"), text(data, "unit")) {
        (lane, unit) if !lane.is_empty() && !unit.is_empty() => format!("{lane}:{unit}"),
        (lane, unit) if unit.is_empty() => lane,
        (_, unit) => unit,
    }
}

/// `devlanes logs LANE:UNIT --tail 200` when the data names a unit.
fn logs_hint(data: &Value) -> Option<Hint> {
    let unit = text(data, "unit");
    if unit.is_empty() {
        return None;
    }
    let lane = text(data, "lane");
    Some(if lane.is_empty() {
        Hint::command(["logs", unit.as_str(), "--tail", "200"])
    } else {
        Hint::logs(&lane, &unit, 200)
    })
}

/// Candidate list for `not_found` and `ambiguous`.
fn candidates(data: &Value) -> String {
    let names: Vec<String> = list(data, "candidates").iter().map(display_value).collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// `file:line:col: message` for `config_error` data.
fn config_location(data: &Value) -> String {
    let message = text(data, "message");
    let file = text(data, "file");
    if file.is_empty() {
        let path = text(data, "path");
        return if path.is_empty() {
            message
        } else {
            format!("{path}: {message}")
        };
    }
    match (int(data, "line"), int(data, "column")) {
        (Some(line), Some(column)) => format!("{file}:{line}:{column}: {message}"),
        (Some(line), None) => format!("{file}:{line}: {message}"),
        _ => format!("{file}: {message}"),
    }
}

/// Integer field.
fn int(data: &Value, key: &str) -> Option<i64> {
    data.get(key).and_then(Value::as_i64)
}

/// `queued: 6 of 40 starting, CPU 93%` from the start gate's `queued` reason data
/// `{position, queued, in_flight, window, hold, cpu_percent?, memory_pressure?}`.
fn queued(data: &Value) -> String {
    let mut text = match (int(data, "position"), int(data, "queued")) {
        (Some(position), Some(total)) => format!("queued: {position} of {total} starting"),
        (Some(position), None) => format!("queued: {position} in line to start"),
        _ => "queued for a start slot".into(),
    };
    if let Some(cpu) = data.get("cpu_percent").and_then(Value::as_f64) {
        text.push_str(&format!(", CPU {cpu:.0}%"));
    }
    if let Some(pressure) = data.get("memory_pressure").filter(|value| !value.is_null()) {
        text.push_str(&format!(", memory pressure {}", display_value(pressure)));
    }
    if data.get("hold").and_then(Value::as_str) == Some("memory_critical") {
        text.push_str("; held until memory pressure drops");
    }
    text
}

/// A one-line message and an optional hint for a daemon error.
pub(crate) fn error_message(error: &RpcError) -> (String, Option<Hint>) {
    let data = &error.data;
    let reference = || {
        let value = text(data, "reference");
        if value.is_empty() {
            target(data)
        } else {
            value
        }
    };
    match error.code.as_str() {
        "not_found" => (
            format!(
                "{} not found; candidates: {}",
                reference(),
                candidates(data)
            ),
            Some(Hint::command(["lane", "ls"])),
        ),
        "ambiguous" => (
            format!(
                "{} matches several lanes: {}",
                reference(),
                candidates(data)
            ),
            list(data, "candidates")
                .first()
                .and_then(Value::as_str)
                .map(|first| Hint::command(["status", first])),
        ),
        "invalid_reference" => (
            format!("invalid reference {}", reference()),
            Some(Hint::command(["lane", "ls"])),
        ),
        "confirmation_required" => {
            let flag = data
                .get("flag")
                .and_then(Value::as_str)
                .unwrap_or("--yes")
                .to_owned();
            let what = if let Some(prompt) = data.get("prompt").and_then(Value::as_str) {
                format!("task {} asks: {}", text(data, "task"), super::clean(prompt))
            } else if let Some(running) = data.get("lanes_running").and_then(Value::as_array) {
                let names: Vec<String> = running.iter().map(display_value).collect();
                format!("shared infra serves running lanes {}", names.join(", "))
            } else if let Some(processes) = int(data, "processes") {
                format!("this stops {processes} running processes")
            } else if !text(data, "lane").is_empty() {
                format!("this removes lane {} and its data", text(data, "lane"))
            } else {
                "this needs confirmation".into()
            };
            (format!("{what}; rerun with {flag}"), None)
        }
        "terminal_required" => {
            let what = if data.get("task").is_some() {
                format!("task {} is interactive", text(data, "task"))
            } else if data.get("reason").is_some() {
                text(data, "reason")
            } else {
                "this command needs an interactive terminal".into()
            };
            let command = text(data, "command");
            (
                format!("{what}; run it from a terminal"),
                (!command.is_empty()).then_some(Hint(command)),
            )
        }
        "project_running_elsewhere" => (
            format!(
                "project {} is already running from {}",
                field(data, "project"),
                field(data, "path")
            ),
            data.get("project")
                .and_then(Value::as_str)
                .map(|project| Hint::command(["stop", "--project", project])),
        ),
        "port_in_use" => (
            format!(
                "port {} is in use by PID {}",
                field(data, "port"),
                field(data, "pid")
            ),
            None,
        ),
        "port_in_use_by_lane" => (
            format!(
                "port {} is used by {} in lane {}",
                field(data, "port"),
                field(data, "unit"),
                field(data, "lane")
            ),
            Some(Hint::command(["stop", &target(data)])),
        ),
        "secrets_locked" => (
            match data.get("profile").and_then(Value::as_str) {
                Some(profile) => format!("secrets for fnox profile {profile} are locked"),
                None => "secrets are locked".into(),
            },
            Some(Hint::command(["secrets", "unlock"])),
        ),
        "needs_env" => (
            "the daemon restarted and has no shell environment for this lane".into(),
            Some(match data.get("lane").and_then(Value::as_str) {
                Some(lane) => Hint::command(["start", "--lane", lane]),
                None => Hint::command(["lane", "resume"]),
            }),
        ),
        "docker_unavailable" => (
            "Docker is not reachable".into(),
            Some(Hint::command(["doctor"])),
        ),
        "timeout" => (
            match target(data) {
                found if !found.is_empty() => format!("timed out waiting for {found}"),
                _ if data.get("task").is_some() => {
                    format!("task {} timed out", text(data, "task"))
                }
                _ => "timed out".into(),
            },
            Some(match target(data) {
                found if !found.is_empty() => Hint::command(["debug", "why", &found]),
                _ => Hint::command(["status"]),
            }),
        ),
        "not_supported_yet" => {
            let feature = data
                .get("feature")
                .or_else(|| data.get("milestone"))
                .map(display_value)
                .unwrap_or_else(|| "this".into());
            (format!("{feature} is not supported yet"), None)
        }
        "config_error" => (
            format!("invalid config: {}", config_location(data)),
            Some(Hint::command(["config", "check"])),
        ),
        "no_config" => (
            "no devlanes.yaml in this worktree".into(),
            Some(Hint::command(["config", "check"])),
        ),
        "units_required" => (
            "name at least one unit".into(),
            Some(match data.get("lane").and_then(Value::as_str) {
                Some(lane) => Hint::command(["status", lane]),
                None => Hint::command(["status"]),
            }),
        ),
        "unit_failed" | "ready_timeout" | "spawn_failed" => (
            match error.code.as_str() {
                "unit_failed" => format!("{} failed", target(data)),
                "ready_timeout" => format!("{} did not become ready in time", target(data)),
                _ => format!(
                    "{} could not start: {}",
                    target(data),
                    text(data, "message")
                ),
            },
            logs_hint(data),
        ),
        "queued" => (
            queued(data),
            Some(Hint::command(["start", "--all-at-once"])),
        ),
        "exited" => (
            match int(data, "code") {
                Some(code) => format!("exited with code {code}"),
                None => match int(data, "signal") {
                    Some(signal) => format!("killed by signal {signal}"),
                    None => "exited".into(),
                },
            },
            logs_hint(data),
        ),
        "not_a_git_project" => (
            format!("project {} is not a git repository", field(data, "project")),
            None,
        ),
        "git_failed" => (format!("git failed: {}", text(data, "message")), None),
        "unsupported" | "unsupported_method" => (
            format!("daemon does not support {}", field(data, "method")),
            None,
        ),
        "unknown_method" => (
            "the running daemon is older and does not know this command".into(),
            Some(Hint::command(["daemon", "restart"])),
        ),
        "incompatible" | "protocol_mismatch" => (
            protocol_mismatch(data),
            Some(Hint::command(["daemon", "restart"])),
        ),
        // The daemon's own message names the unit and pid; the hint stops it.
        "unit_orphaned" => {
            let (lane, unit) = (text(data, "lane"), text(data, "unit"));
            (
                match data.get("message").and_then(Value::as_str) {
                    Some(message) => super::clean(message),
                    None => match int(data, "pid") {
                        Some(pid) => format!(
                            "{} is still running from a previous daemon (pid {pid}); start does not take it over",
                            target(data)
                        ),
                        None => format!(
                            "{} is still running from a previous daemon; start does not take it over",
                            target(data)
                        ),
                    },
                },
                (!unit.is_empty() && !lane.is_empty())
                    .then(|| Hint::command(["stop", unit.as_str(), "--lane", lane.as_str()])),
            )
        }
        "interrupted" => (
            format!(
                "interrupted by {}; stopping what this run started",
                field(data, "signal")
            ),
            None,
        ),
        "daemon_sandboxed" => (
            // The agent guide tells agents to look for this phrase.
            format!(
                "auto-spawn refused in sandbox ({}); run devlanes daemon install outside the sandbox, then retry",
                super::clean(&text(data, "message"))
            ),
            // Installed as a service, the daemon runs outside the sandbox.
            Some(Hint::command(["daemon", "install"])),
        ),
        "daemon_still_running" => (
            format!(
                "the old daemon (pid {}) was still running after {}s",
                field(data, "pid"),
                field(data, "waited_s")
            ),
            Some(Hint::command(["daemon", "status"])),
        ),
        "daemon_unreachable" => (
            match data.get("message").and_then(Value::as_str) {
                Some(message) => format!("daemon is unreachable: {}", super::clean(message)),
                None => "daemon is unreachable".into(),
            },
            Some(Hint::command(["daemon", "install"])),
        ),
        // The daemon's message already names the command; the hint carries it instead.
        "helper_version_mismatch" => (
            helper_mismatch(data.get("helper").unwrap_or(data)),
            Some(Hint::command(["certs", "trust", "--hosts"])),
        ),
        "helper_missing"
        | "sudo_denied"
        | "helper_outdated"
        | "helper_source_missing"
        | "helper_source_rejected"
        | "sudo_rule_inactive"
        | "hosts_unsafe_directory" => (
            data.get("message")
                .and_then(Value::as_str)
                .map(super::clean)
                .unwrap_or_else(|| error.code.replace('_', " ")),
            Some(Hint::command(["certs", "trust", "--hosts"])),
        ),
        _ => (
            match data.get("message").and_then(Value::as_str) {
                Some(message) => {
                    format!("{}: {}", super::clean(&error.code), super::clean(message))
                }
                None if data.is_null() || *data == json!({}) => super::clean(&error.code),
                None => format!("{}: {}", super::clean(&error.code), compact_fields(data)),
            },
            None,
        ),
    }
}

/// A daemon of another protocol: what runs, and what restarting it costs.
fn protocol_mismatch(data: &Value) -> String {
    let cost = match int(data, "managed_processes").unwrap_or(0) {
        0 => String::new(),
        1 => "; restarting it stops its 1 managed process".into(),
        processes => format!("; restarting it stops its {processes} managed processes"),
    };
    let protocols = match (int(data, "protocol"), int(data, "client_protocol")) {
        (Some(daemon), Some(client)) => format!(" (protocol {daemon}, this CLI speaks {client})"),
        _ => String::new(),
    };
    format!(
        "the running daemon is devlanes {}{protocols} and cannot serve this CLI{cost}",
        field(data, "version")
    )
}

/// The hosts helper against this devlanes, from doctor's `hosts.helper`
/// (`{state, installed, expected}`); without versions, a general sentence.
pub(crate) fn helper_mismatch(helper: &Value) -> String {
    let expected = text(helper, "expected");
    match (
        helper.get("installed").and_then(Value::as_str),
        helper.get("state").and_then(Value::as_str),
    ) {
        (_, Some("missing")) => "the /etc/hosts helper is not installed".into(),
        (_, Some("unrunnable")) => "the installed /etc/hosts helper cannot run".into(),
        (_, Some("incompatible")) => format!(
            "the installed /etc/hosts helper speaks another protocol than devlanes {expected}; /etc/hosts updates fail"
        ),
        (Some(installed), _) if !expected.is_empty() => format!(
            "the installed /etc/hosts helper is version {}, devlanes is {expected}",
            super::clean(installed)
        ),
        _ => "the installed /etc/hosts helper does not match this devlanes".into(),
    }
}

/// A byte count field as `1.5 GiB`, `?` when absent.
fn memory(data: &Value, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_u64)
        .map_or_else(|| "?".into(), super::bytes)
}

/// A lane warning (`StartResult.warnings`, `lane.warning`, doctor's `ca_warnings`)
/// as one line: `api.example.com is outside the CA's names; HTTPS will fail; try api.acme.test`.
pub(crate) fn lane_warning(code: &str, data: &Value) -> String {
    match code {
        "host_outside_ca" => {
            let mut line = format!(
                "{} is outside the CA's names; HTTPS will fail",
                field(data, "host")
            );
            if let Some(suggestion) = data.get("suggestion").and_then(Value::as_str) {
                line.push_str(&format!("; try {}", super::clean(suggestion)));
            }
            line
        }
        "memory_baseline" => format!(
            "this lane's units peak at {} together, more than the {} available; starts may wait for memory",
            memory(data, "peak_memory_bytes"),
            memory(data, "available_memory_bytes")
        ),
        other => match data.get("message").and_then(Value::as_str) {
            Some(message) => super::clean(message),
            None => super::clean(other).replace('_', " "),
        },
    }
}

/// A `why` reason as one line, plus the command that addresses it.
pub(crate) fn reason(code: &str, data: &Value, lane: &str, unit: &str) -> (String, Option<Hint>) {
    let start_unit = || Hint::command(["start", &format!("{lane}:{unit}")]);
    match code {
        "not_in_profile" => (
            format!("not selected by profile {}", field(data, "profile")),
            Some(start_unit()),
        ),
        "autostart_off" => (
            "autostart is off; it only starts when named".into(),
            Some(start_unit()),
        ),
        "needs_env" => (
            "the daemon restarted and has no shell environment for this lane".into(),
            Some(Hint::command(["start", "--lane", lane])),
        ),
        "secrets_locked" => (
            "its secrets are locked".into(),
            Some(Hint::command(["secrets", "unlock"])),
        ),
        "config_error" => (
            format!("invalid config: {}", config_location(data)),
            Some(Hint::command(["config", "check"])),
        ),
        "port_in_use" => (
            match data.get("pid").filter(|pid| !pid.is_null()) {
                Some(pid) => format!(
                    "port {} is taken by PID {}",
                    field(data, "port"),
                    display_value(pid)
                ),
                None => format!("port {} is taken by another process", field(data, "port")),
            },
            Some(Hint::command(["debug", "errors"])),
        ),
        "port_in_use_by_lane" => (
            format!(
                "port {} is used by {} in lane {}",
                field(data, "port"),
                field(data, "unit"),
                field(data, "lane")
            ),
            Some(Hint::command(["stop", &target(data)])),
        ),
        "waiting_after_services" => (
            "waiting for the lane's services to be ready (after: services)".into(),
            Some(Hint::command(["wait", lane])),
        ),
        "queued" => (
            queued(data),
            Some(Hint::command(["start", "--all-at-once", "--lane", lane])),
        ),
        "starting" => (
            "starting; waiting for its readiness check".into(),
            Some(Hint::command(["wait", &format!("{lane}:{unit}")])),
        ),
        "ready_timeout" | "timed_out" => (
            "did not become ready before the timeout".into(),
            Some(Hint::logs(lane, unit, 200)),
        ),
        "exited" => (
            match int(data, "code") {
                Some(code) => format!("exited with code {code}"),
                None => format!("killed by signal {}", field(data, "signal")),
            },
            Some(Hint::logs(lane, unit, 200)),
        ),
        "stopped" => {
            let requested = data.get("requested").and_then(Value::as_bool) == Some(true);
            let how = match (
                int(data, "exit_code"),
                data.get("signal").and_then(Value::as_i64),
            ) {
                (Some(code), _) => format!("exit code {code}"),
                (None, Some(signal)) => format!("signal {signal}"),
                _ => "no exit status".into(),
            };
            (
                if requested {
                    format!("stopped on request ({how})")
                } else {
                    format!("exited by itself ({how})")
                },
                Some(start_unit()),
            )
        }
        "unassigned" => (
            match data.get("reason").and_then(Value::as_str) {
                Some("lanes_max") => {
                    format!("lane is unassigned: lanes.max is {}", field(data, "max"))
                }
                Some("pool_exhausted") => "lane is unassigned: the port pool is full".into(),
                _ => "lane is unassigned".into(),
            },
            Some(Hint::command(["lane", "gc"])),
        ),
        "no_config" => (
            "this worktree has no devlanes.yaml".into(),
            Some(Hint::command(["config", "check"])),
        ),
        "spawn_failed" => (
            format!("could not start: {}", text(data, "message")),
            Some(Hint::logs(lane, unit, 200)),
        ),
        "helper_missing" | "sudo_denied" | "helper_outdated" | "helper_version_mismatch" => {
            let hosts: Vec<String> = list(data, "hosts").iter().map(display_value).collect();
            let problem = match code {
                "helper_missing" => "the /etc/hosts helper is not installed",
                "sudo_denied" => "sudo will not run the /etc/hosts helper without a password",
                "helper_version_mismatch" => {
                    "the installed /etc/hosts helper does not match this devlanes"
                }
                _ => "the installed /etc/hosts helper is outdated",
            };
            (
                format!("{problem}, so {} may not resolve", hosts.join(", ")),
                Some(Hint::command(["certs", "trust", "--hosts"])),
            )
        }
        other => (
            if data.is_null() || *data == json!({}) {
                super::clean(other).replace('_', " ")
            } else {
                format!(
                    "{}: {}",
                    super::clean(other).replace('_', " "),
                    compact_fields(data)
                )
            },
            None,
        ),
    }
}
