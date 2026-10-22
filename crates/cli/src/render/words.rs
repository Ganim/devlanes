//! Wording for daemon error codes and `why` reason codes. The daemon only sends
//! codes with data; every sentence and hint lives here.

use devlanes_proto::RpcError;
use serde_json::{Value, json};

use super::{INTERRUPTED, compact_fields, ctrl_c_exit, display_value, exited_with, list, text};
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

/// Why a unit's process did not start, from `spawn_failed` data `{program,
/// reason?, path_searched?, cwd?, errno?, message}`. Daemons before `reason`
/// existed send only `message`.
pub(crate) fn spawn_cause(data: &Value) -> String {
    let program = text(data, "program");
    match text(data, "reason").as_str() {
        "not_found" if program.contains('/') => format!("{program}: no such file"),
        "not_found" => format!("{program}: command not found on PATH"),
        "not_executable" => format!("{program} is not executable"),
        "cwd_missing" => format!("working directory {} does not exist", text(data, "cwd")),
        _ => {
            let mut cause = text(data, "message");
            if !program.is_empty() {
                cause = format!("{program}: {cause}");
            }
            if let Some(errno) = data.get("errno").and_then(Value::as_str) {
                cause.push_str(&format!(" ({errno})"));
            }
            cause
        }
    }
}

/// The fix for a spawn the program, not the unit, broke: the lane's PATH for a
/// program not found there, `chmod +x` for one that cannot run.
pub(crate) fn spawn_hint(data: &Value) -> Option<Hint> {
    let unit = text(data, "unit");
    let lane = text(data, "lane");
    match text(data, "reason").as_str() {
        "not_found" if !text(data, "program").contains('/') && !unit.is_empty() => {
            let mut args = vec!["env", "show", unit.as_str()];
            if !lane.is_empty() {
                args.extend(["--lane", lane.as_str()]);
            }
            Some(Hint(format!(
                "check the lane's PATH: {}",
                Hint::command(args).0
            )))
        }
        "not_executable" => data
            .get("resolved")
            .and_then(Value::as_str)
            .map(|path| Hint(format!("chmod +x {}", crate::shell_quote(path)))),
        _ => None,
    }
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
        // The caller replaces the hint with one that lists what this command accepts.
        "not_found" => {
            let options = list(data, "candidates");
            let name = reference();
            // A typo of a listed name is the likeliest meaning.
            let mut message = format!("{name} not found");
            if let Some(guess) = did_you_mean(name.rsplit(':').next().unwrap_or(&name), options) {
                message.push_str(&format!("; did you mean {}?", super::clean(guess)));
            }
            match options.len() {
                0 => {}
                1..=12 => message.push_str(&format!("; options: {}", candidates(data))),
                n => message.push_str(&format!("; {n} options")),
            }
            (message, Some(Hint::command(["lane", "ls"])))
        }
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
            } else if let Some(lanes) = data
                .get("lanes")
                .and_then(Value::as_array)
                .filter(|lanes| !lanes.is_empty())
            {
                // `lane gc`: the lanes whose worktree is gone.
                let names: Vec<String> = lanes.iter().map(display_value).collect();
                format!("this removes lanes {} and their data", names.join(", "))
            } else if let Some(processes) = int(data, "processes") {
                match processes {
                    1 => "this stops 1 running process".into(),
                    n => format!("this stops {n} running processes"),
                }
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
        "no_config" => no_config(data),
        "units_required" => (
            "name at least one unit".into(),
            Some(match data.get("lane").and_then(Value::as_str) {
                Some(lane) => Hint::command(["status", lane]),
                None => Hint::command(["status"]),
            }),
        ),
        // `with -- COMMAND` could not start the command (127, like a shell).
        "spawn_failed" if data.get("program").is_some() && data.get("unit").is_none() => (
            format!(
                "cannot run {}: {}",
                field(data, "program"),
                text(data, "message")
            ),
            None,
        ),
        // The unit alone: start summaries list it under its lane already.
        "spawn_failed" => (
            format!(
                "{} could not start: {}",
                text(data, "unit"),
                spawn_cause(data)
            ),
            spawn_hint(data).or_else(|| logs_hint(data)),
        ),
        "unit_failed" | "ready_timeout" => (
            match error.code.as_str() {
                "unit_failed" => format!("{} failed", target(data)),
                // Without a target it sits on the unit's own row.
                "ready_timeout" if target(data).is_empty() => "did not become ready in time".into(),
                _ => format!("{} did not become ready in time", target(data)),
            },
            logs_hint(data),
        ),
        "queued" => (
            queued(data),
            Some(Hint::command(["start", "--all-at-once"])),
        ),
        "exited" => (
            match int(data, "code") {
                Some(code) => exited_with(code),
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
        "start_cancelled" => (
            format!(
                "the start of {} was cancelled by a stop, clean or reset of the lane",
                field(data, "lane")
            ),
            data.get("lane")
                .and_then(Value::as_str)
                .map(|lane| Hint::command(["status", lane])),
        ),
        // `{lane, units}` from `lane clean`; a start phase skipped for this reason
        // carries no data.
        "lane_running" => {
            let units: Vec<String> = list(data, "units").iter().map(display_value).collect();
            match data.get("lane").and_then(Value::as_str) {
                Some(lane) if !units.is_empty() => (
                    format!(
                        "{} still runs {}; stop the lane, then retry",
                        super::clean(lane),
                        units.join(", ")
                    ),
                    Some(Hint::command(["stop", "--lane", lane])),
                ),
                _ => ("the lane is already running".into(), None),
            }
        }
        "daemon_stopping" => (
            "the daemon is shutting down; retry once it has stopped, or start a new one now".into(),
            Some(Hint::command(["daemon", "restart"])),
        ),
        "name_taken" => {
            let (name, lane) = (text(data, "name"), text(data, "lane"));
            let kind = match text(data, "kind").as_str() {
                "task" => "task",
                _ => "service",
            };
            (
                format!(
                    "{name} is a configured {kind} of {lane}; give exec another --name, or run the configured {kind}"
                ),
                Some(if kind == "task" {
                    Hint::command(["run", name.as_str(), "--lane", lane.as_str()])
                } else {
                    Hint::command(["start", &format!("{lane}:{name}")])
                }),
            )
        }
        "response_too_large" => (
            format!(
                "the answer is larger than the {} MiB message limit; ask for less: a smaller --tail or --budget, or one unit at a time",
                int(data, "limit_bytes").map_or(16, |bytes| bytes / (1024 * 1024))
            ),
            None,
        ),
        "client_disconnected" => (
            "the connection to the daemon closed before the answer arrived".into(),
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
                // The daemon's `command` is the exact stop for this orphan.
                match data.get("command").and_then(Value::as_str) {
                    Some(command) => Some(Hint(super::clean(command))),
                    None => (!unit.is_empty() && !lane.is_empty())
                        .then(|| Hint::command(["stop", unit.as_str(), "--lane", lane.as_str()])),
                },
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
            // These messages already say what to do; installing a service would not help.
            match text(data, "reason").as_str() {
                "daemon_elevated" | "endpoint_taken" => None,
                _ => Some(Hint::command(["daemon", "install"])),
            },
        ),
        // The daemon's message already names the command; the hint carries it instead.
        "helper_version_mismatch" => (
            helper_mismatch(data.get("helper").unwrap_or(data)),
            Some(Hint::command(["certs", "trust", "--hosts"])),
        ),
        // Windows: the elevated installer's prompt got a No.
        "elevation_cancelled" => (
            "the administrator prompt was declined; nothing was changed".into(),
            None,
        ),
        "helper_missing"
        | "sudo_denied"
        | "helper_outdated"
        | "helper_source_missing"
        | "helper_source_rejected"
        | "sudo_rule_inactive"
        | "hosts_unsafe_directory"
        | "service_missing"
        | "service_denied"
        | "service_untrusted"
        | "service_source_missing"
        | "service_source_rejected" => (
            data.get("message")
                .and_then(Value::as_str)
                .map(super::clean)
                .unwrap_or_else(|| error.code.replace('_', " ")),
            Some(Hint::command(["certs", "trust", "--hosts"])),
        ),
        code => more_errors(code, data).unwrap_or_else(|| {
            (
                match data.get("message").and_then(Value::as_str) {
                    Some(message) => {
                        format!("{}: {}", super::clean(code), super::clean(message))
                    }
                    None if data.is_null() || *data == json!({}) => super::clean(code),
                    None => format!("{}: {}", super::clean(code), compact_fields(data)),
                },
                None,
            )
        }),
    }
}

/// Where to find a minimal `devlanes.yaml` to start from.
pub(crate) const EXAMPLE_CONFIG: &str =
    "https://github.com/devlanes-run/devlanes/blob/main/docs/examples/minimal.yaml";

/// `no_config` (`{path, searched_to}`; older daemons send `{lane}`): where the
/// search looked, and an example to create the file from. Never `config check`,
/// which fails the same way.
fn no_config(data: &Value) -> (String, Option<Hint>) {
    let path = text(data, "path");
    let root = text(data, "searched_to");
    let message = match (path.is_empty(), root.is_empty()) {
        (false, false) => {
            format!("no devlanes.yaml in {path} or its parents (searched up to {root})")
        }
        (false, true) => format!("no devlanes.yaml in {path} or its parents"),
        _ if !text(data, "lane").is_empty() => {
            format!("lane {} has no devlanes.yaml", text(data, "lane"))
        }
        _ => "no devlanes.yaml in this directory or its parents".into(),
    };
    let place = if root.is_empty() {
        "the repository root".to_owned()
    } else {
        root
    };
    (
        message,
        Some(Hint(format!(
            "create devlanes.yaml in {place}; a minimal one: {EXAMPLE_CONFIG}"
        ))),
    )
}

/// The closest of `candidates` to `name` by edit distance, when it is close enough
/// to be a typo (at most a third of the name's length, and at least 1).
fn did_you_mean<'a>(name: &str, candidates: &'a [Value]) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    candidates
        .iter()
        .filter_map(Value::as_str)
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance over characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.iter().enumerate() {
            let cost = usize::from(left != *right);
            current.push(
                (previous[j] + cost)
                    .min(previous[j + 1] + 1)
                    .min(current[j] + 1),
            );
        }
        previous = current;
    }
    previous[b.len()]
}

/// Lane and unit codes beyond the common ones in [`error_message`]; `None` for a
/// code without wording. Every code the daemon sends has an arm here or there
/// (`tests/it/codes.rs` checks the daemon's sources).
fn more_errors(code: &str, data: &Value) -> Option<(String, Option<Hint>)> {
    let lane = text(data, "lane");
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .map(super::clean);
    let with_message = |text: String| match &message {
        Some(message) => format!("{text}: {message}"),
        None => text,
    };
    let names = |key: &str| -> String {
        list(data, key)
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(", ")
    };
    Some(match code {
        "port_error" => {
            let unit = target(data);
            let line = match data.get("port").filter(|port| !port.is_null()) {
                Some(port) => format!(
                    "{unit} needs port {}, which this lane's port block lacks since the config changed",
                    display_value(port)
                ),
                None => with_message(format!("ports for {unit} could not be assigned")),
            };
            // The holder named by the data is what to stop.
            let hint = match (int(data, "pid"), text(data, "holder_lane")) {
                (Some(pid), _) => Some(Hint(format!("stop process {pid}, then retry"))),
                (None, holder) if !holder.is_empty() => {
                    Some(Hint::command(["stop", "--lane", holder.as_str()]))
                }
                _ if !lane.is_empty() => Some(Hint::command(["stop", "--lane", lane.as_str()])),
                _ => Some(Hint::command(["debug", "errors"])),
            };
            (line, hint)
        }
        "unit_running" => {
            let unit = target(data);
            (
                format!("{unit} is already running; stop it first, or restart it"),
                Some(Hint::command(["restart", unit.as_str()])),
            )
        }
        "unknown_profile" => {
            let profile = text(data, "profile");
            let options = list(data, "candidates");
            match did_you_mean(&profile, options) {
                Some(guess) => (
                    format!(
                        "no profile {profile}; did you mean {}?",
                        super::clean(guess)
                    ),
                    Some(Hint::command(["start", "--profile", guess])),
                ),
                None => (
                    format!("no profile {profile}; profiles: {}", candidates(data)),
                    None,
                ),
            }
        }
        "worktree_missing" => (
            format!("the worktree {} no longer exists", field(data, "path")),
            Some(Hint::command(["lane", "gc"])),
        ),
        "task_failed" => {
            let task = text(data, "task");
            (
                match int(data, "exit_code") {
                    Some(code) if ctrl_c_exit(code) => format!("task {task} was {INTERRUPTED}"),
                    Some(code) => format!("task {task} failed with exit code {code}"),
                    None => format!("task {task} failed"),
                },
                (!task.is_empty()).then(|| {
                    if lane.is_empty() {
                        Hint::command(["logs", task.as_str(), "--tail", "200"])
                    } else {
                        Hint::logs(&lane, &task, 200)
                    }
                }),
            )
        }
        "setup_failed" => (
            format!("infra setup failed: {}", names("entries")),
            Some(Hint::command(["debug", "errors"])),
        ),
        "service_failed" => {
            let units = names("units");
            let first = list(data, "units")
                .first()
                .map(display_value)
                .unwrap_or_default();
            (
                if units.is_empty() {
                    "a service failed".into()
                } else {
                    format!("{units} failed")
                },
                (!first.is_empty()).then(|| {
                    if lane.is_empty() {
                        Hint::command(["logs", first.as_str(), "--tail", "200"])
                    } else {
                        Hint::logs(&lane, &first, 200)
                    }
                }),
            )
        }
        "phase_failed" => {
            let inner = data
                .get("error")
                .and_then(|inner| {
                    // The inner error's hints need the lane the phase ran in.
                    let mut inner_data = inner.get("data").cloned().unwrap_or(json!({}));
                    if let Some(map) = inner_data.as_object_mut()
                        && !lane.is_empty()
                    {
                        map.entry("lane").or_insert_with(|| json!(lane));
                    }
                    Some(RpcError::new(inner.get("code")?.as_str()?, inner_data, 1))
                })
                .map(|inner| error_message(&inner));
            let phase = field(data, "phase");
            match inner {
                Some((why, hint)) => (format!("the {phase} phase failed: {why}"), hint),
                None => (format!("the {phase} phase failed"), None),
            }
        }
        "start_failed" => {
            let units = names("units");
            (
                if units.is_empty() {
                    format!("{} did not become ready", field(data, "lane"))
                } else {
                    format!("{} did not become ready: {units}", field(data, "lane"))
                },
                (!lane.is_empty()).then(|| Hint::command(["status", lane.as_str()])),
            )
        }
        "pool_exhausted" => (
            format!(
                "the port pool has no free block of {} ports for another lane",
                field(data, "block_size")
            ),
            Some(Hint::command(["lane", "gc"])),
        ),
        "block_too_small" => (
            format!(
                "the lane's port block of {} ports cannot hold every port the config asks for; raise lanes.block_size in devlanes.yaml",
                field(data, "block_size")
            ),
            None,
        ),
        "port_block_conflict" => (
            "another lane took the same port block at the same moment; retry".into(),
            None,
        ),
        "port_allocation_failed" => (
            with_message("ports could not be allocated".into()),
            Some(Hint::command(["doctor"])),
        ),
        "mixed_lanes" => (
            format!(
                "{} belong to different lanes; name units of one lane per command",
                names("units")
            ),
            None,
        ),
        "not_running" => {
            let unit = target(data);
            (
                format!("{unit} is not running"),
                Some(Hint::command(["start", unit.as_str()])),
            )
        }
        "stop_timeout" => (
            format!(
                "{} did not report its exit after the stop; its process may still run",
                target(data)
            ),
            Some(Hint::command(["status"])),
        ),
        "host_not_connected" => {
            let host = text(data, "host");
            (
                format!("not connected to {host}"),
                Some(Hint::command(["remote", "connect", host.as_str()])),
            )
        }
        "remote_failed" => (
            with_message(format!("remote {}", field(data, "host"))),
            Some(Hint::command(["remote", "ls"])),
        ),
        "secrets_failed" => (
            with_message(match data.get("profile").and_then(Value::as_str) {
                Some(profile) => format!("secrets for fnox profile {profile} could not be read"),
                None => "secrets could not be read".into(),
            }),
            Some(Hint::command(["secrets", "unlock"])),
        ),
        "script_error" => (
            match data.get("file").and_then(Value::as_str) {
                Some(file) => format!(
                    "{}: no script runner knows {}",
                    target(data),
                    super::clean(file)
                ),
                None => with_message(format!("{}: run_script", target(data))),
            },
            None,
        ),
        "contributor_failed" => (
            with_message(format!(
                "{} platform setup for {}",
                field(data, "contributor"),
                target(data)
            )),
            None,
        ),
        "launch_check_failed" => (with_message("the start was refused".into()), None),
        "prepared_file_outside_lane" => (
            format!(
                "a prepared file would be written outside the lane's worktree: {}",
                field(data, "path")
            ),
            None,
        ),
        "active_runs" => (
            message.unwrap_or_else(|| "runs are still recorded as running".into()),
            Some(Hint::command(["stop"])),
        ),
        "state_conflict" => (
            message.unwrap_or_else(|| "state.db already has a conflicting record".into()),
            Some(Hint::command(["status"])),
        ),
        "state_corrupt" => (
            message.unwrap_or_else(|| "state.db holds an unreadable value".into()),
            Some(Hint::command(["daemon", "reset-state", "--yes"])),
        ),
        "state_error" => (
            with_message("the state database failed".into()),
            Some(Hint::command(["daemon", "logs"])),
        ),
        "log_unavailable" => ("the unit's log cannot be read now; retry".into(), None),
        "unknown_stream" => ("the attached terminal is gone; attach again".into(), None),
        "bad_request" => (
            with_message("the daemon could not read the request".into()),
            Some(Hint::command(["daemon", "status"])),
        ),
        "no_url" => (
            match target(data) {
                unit if !unit.is_empty() => format!("{unit} has no URL"),
                _ => "this unit has no URL".into(),
            },
            Some(Hint::command(["url", "ls"])),
        ),
        "unsafe_home" => unsafe_home(data),
        // `{container, check, timeout_ms}`: an infra container's ready check.
        "container_not_ready" => (
            format!(
                "container {} did not pass its {} check within {}",
                field(data, "container"),
                field(data, "check"),
                data.get("timeout_ms")
                    .and_then(Value::as_u64)
                    .map_or_else(|| "its timeout".into(), super::duration)
            ),
            Some(Hint::command(["infra", "status"])),
        ),
        // A unit a `stop` or `restart` (`by`) took out of a running start.
        "cancelled_by_stop" => (
            format!(
                "taken out of the start by devlanes {}",
                match text(data, "by").as_str() {
                    "restart" => "restart",
                    _ => "stop",
                }
            ),
            None,
        ),
        "unassigned" => (
            match data.get("reason").and_then(Value::as_str) {
                Some("pool_exhausted") => format!(
                    "{} has no ports: the port pool is full",
                    field(data, "lane")
                ),
                Some("lanes_max") => format!(
                    "{} has no ports: lanes.max {} is reached",
                    field(data, "lane"),
                    field(data, "max")
                ),
                _ => format!("{} has no ports assigned", field(data, "lane")),
            },
            Some(Hint::command(["lane", "gc"])),
        ),
        "port_mirrored" => {
            let host = text(data, "host");
            (
                format!(
                    "port {} is mirrored from {host} for {}",
                    field(data, "port"),
                    field(data, "unit")
                ),
                Some(Hint::command(["remote", "disconnect", host.as_str()])),
            )
        }
        // Start phase skips (`! infra skipped: …`); `no_*` ones are not printed.
        "already_done" => ("already done".into(), None),
        "setup_only" => ("only setup was asked for".into(), None),
        "not_resolved" => ("the config could not be resolved".into(), None),
        "profile_disables_infra" => ("the selected profiles turn infra off".into(), None),
        "timed_out" => ("the start timed out".into(), None),
        "cancelled" => ("cancelled".into(), None),
        "no_infra" => ("no infra configured".into(), None),
        "no_setup" => ("no setup commands".into(), None),
        "no_tasks" => ("no tasks for this phase".into(), None),
        "no_services" => ("no services selected".into(), None),
        _ => return None,
    })
}

/// A `remote.warning` event (`{host, code, lane?, hosts?, port?}`) as one line.
pub(crate) fn remote_warning(code: &str, data: &Value) -> String {
    let host = field(data, "host");
    let hosts = list(data, "hosts")
        .iter()
        .map(display_value)
        .collect::<Vec<_>>()
        .join(", ");
    match code {
        "lane_skipped" => format!(
            "{host}: lane {} is not mirrored; this machine already serves {hosts}",
            field(data, "lane")
        ),
        "hosts_skipped" => format!(
            "{host}: {hosts} are not mirrored; they are outside the hostnames this machine routes"
        ),
        "port_in_use" => format!(
            "{host}: port {} is busy here, so it is not mirrored",
            field(data, "port")
        ),
        other => match data.get("message").and_then(Value::as_str) {
            Some(message) => format!("{host}: {}", super::clean(message)),
            None => format!("{host}: {}", super::clean(other).replace('_', " ")),
        },
    }
}

/// Whether `code` has its own wording as an error, lane warning or remote
/// warning, rather than the generic `code: message` fallback.
pub(crate) fn has_wording(code: &str) -> bool {
    const PROBE: &str = "zz-probe";
    let data = json!({"message": PROBE});
    let (error, _) = error_message(&RpcError::new(code, data.clone(), 1));
    let fallback = format!("{code}: {PROBE}");
    error != fallback
        || lane_warning(code, &data) != PROBE
        || remote_warning(code, &data) != format!("unknown: {PROBE}")
}

/// `unsafe_home` (`{path, problem, owner?, mode?, fix?}`): the daemon refuses a
/// state directory others could write; `mode` problems get `chmod 700`, `acl`
/// problems (Windows) come with their own `fix`.
fn unsafe_home(data: &Value) -> (String, Option<Hint>) {
    let path = field(data, "path");
    let why = match text(data, "problem").as_str() {
        "mode" => match data.get("mode").filter(|mode| !mode.is_null()) {
            Some(mode) => format!("mode {} lets other users in", display_value(mode)),
            None => "other users can access it".into(),
        },
        "acl" => "its access list lets other accounts in".into(),
        "owner" => match data.get("owner").filter(|owner| !owner.is_null()) {
            Some(owner) => format!("it is owned by {}, not you", display_value(owner)),
            None => "another user owns it".into(),
        },
        "symlink" => "it is a symlink or junction".into(),
        "not_a_directory" => "it is not a directory".into(),
        _ => text(data, "message"),
    };
    let fix = data
        .get("fix")
        .and_then(Value::as_str)
        .map(|fix| Hint(super::clean(fix)))
        .or_else(|| {
            (text(data, "problem") == "mode")
                .then(|| Hint(format!("chmod 700 {}", crate::shell_quote(&path))))
        });
    (format!("devlanes refuses its home {path}: {why}"), fix)
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
        // The new block is allocated only when nothing in the lane runs.
        "ports_pending" => {
            let lane = text(data, "lane");
            if lane.is_empty() {
                "config changed ports; they apply at the next full start of the lane".into()
            } else {
                format!(
                    "config changed ports; they apply at the next full start: {}, then {}",
                    Hint::command(["stop", "--lane", lane.as_str()]).0,
                    Hint::command(["start", "--lane", lane.as_str()]).0
                )
            }
        }
        // `lane` keeps the hostname, `other` loses it.
        "host_conflict" => format!(
            "{} is routed by {} and {}; {} keeps it, so give one of them another hostname",
            field(data, "host"),
            field(data, "lane"),
            field(data, "other"),
            field(data, "lane")
        ),
        // The daemon's message says what is wrong with the name.
        "host_rejected" => {
            let mut line = format!("hostname {} is invalid", field(data, "host"));
            let (lane, unit) = (text(data, "lane"), text(data, "unit"));
            if !lane.is_empty() && !unit.is_empty() {
                line.push_str(&format!(" ({lane}:{unit})"));
            }
            match data.get("message").and_then(Value::as_str) {
                Some(message) => line.push_str(&format!(": {}", super::clean(message))),
                None => line.push_str("; the route is left out"),
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
                Some(code) => exited_with(code),
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
                (Some(code), _) if ctrl_c_exit(code) => "Ctrl-C".into(),
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
            format!("could not start: {}", spawn_cause(data)),
            Some(spawn_hint(data).unwrap_or_else(|| Hint::logs(lane, unit, 200))),
        ),
        "helper_missing"
        | "sudo_denied"
        | "helper_outdated"
        | "helper_version_mismatch"
        | "service_missing"
        | "service_denied"
        | "service_untrusted" => {
            let hosts: Vec<String> = list(data, "hosts").iter().map(display_value).collect();
            let problem = match code {
                "helper_missing" => "the /etc/hosts helper is not installed",
                "sudo_denied" => "sudo will not run the /etc/hosts helper without a password",
                "helper_version_mismatch" => {
                    "the installed /etc/hosts helper does not match this devlanes"
                }
                // Windows: the devlanes-hosts service writes the hosts file.
                "service_missing" => "the devlanes hosts service is not installed or not running",
                "service_denied" => "the devlanes hosts service serves another user, not this one",
                "service_untrusted" => {
                    "the hosts service's pipe is held by another program, not the devlanes service"
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
