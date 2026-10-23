//! Wording for daemon errors in the TUI. The daemon sends a code with data;
//! sentences and hints are written here, following the CLI's wording
//! (`devlanes-cli` `render/words.rs`) so both clients say the same thing.

use devlanes_proto::RpcError;
use serde_json::Value;

use crate::{api::ApiError, text};

/// Lines kept from a long stderr in the details pane: as many as the daemon
/// keeps of a failed unit's log.
pub const STDERR_TAIL: usize = 20;

/// An error ready to show: a one-line summary, further cleaned lines (stderr
/// for failed commands), the failed command and a hint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worded {
    /// The daemon's code, when the error came from the daemon.
    pub code: Option<String>,
    pub summary: String,
    pub detail: Vec<String>,
    /// The helper command that failed; shown after `detail` because its
    /// output says more than its arguments.
    pub command: Option<String>,
    pub hint: Option<String>,
}

impl Worded {
    /// A local failure (opener, launcher, clipboard) with a plain message.
    pub fn local(message: &str) -> Self {
        let mut lines = text::clean_lines(message).into_iter();
        Self {
            code: None,
            summary: lines.next().unwrap_or_default(),
            detail: lines.collect(),
            command: None,
            hint: None,
        }
    }

    /// Summary and detail as one cleaned line, for the header.
    pub fn one_line(&self) -> String {
        let mut line = self.summary.clone();
        for detail in &self.detail {
            let detail = detail.trim();
            if !detail.is_empty() {
                line.push_str("; ");
                line.push_str(detail);
            }
        }
        text::clean(&line)
    }
}

/// Words any client-side API error; transport failures never show Debug text.
pub fn api(error: &ApiError) -> Worded {
    match error {
        ApiError::Rpc(error) => rpc(error),
        ApiError::Disconnected(message) => Worded {
            code: None,
            summary: "can't reach the daemon".into(),
            detail: text::clean_lines(message),
            command: None,
            hint: Some("devlanes daemon status".into()),
        },
        ApiError::Decode(message) => Worded {
            code: None,
            summary: "the daemon's reply didn't parse; daemon and TUI versions may differ".into(),
            detail: text::clean_lines(message),
            command: None,
            hint: Some("devlanes doctor".into()),
        },
    }
}

/// A string field, cleaned; empty when absent.
fn field(data: &Value, key: &str) -> String {
    match data.get(key) {
        Some(Value::String(value)) => text::clean(value),
        Some(Value::Null) | None => String::new(),
        Some(other) => text::clean(&other.to_string()),
    }
}

/// A field, `unknown` when absent.
fn known(data: &Value, key: &str) -> String {
    let value = field(data, key);
    if value.is_empty() {
        "unknown".into()
    } else {
        value
    }
}

/// `lane:unit`, or whichever part exists.
fn target(data: &Value) -> String {
    match (field(data, "lane"), field(data, "unit")) {
        (lane, unit) if !lane.is_empty() && !unit.is_empty() => format!("{lane}:{unit}"),
        (lane, unit) if unit.is_empty() => lane,
        (_, unit) => unit,
    }
}

/// Candidate list for `not_found` and `ambiguous`.
fn candidates(data: &Value) -> String {
    let names: Vec<String> = data
        .get("candidates")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), text::clean))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// `key=value` pairs, skipping fields every start error carries.
fn compact_fields(data: &Value) -> String {
    let Some(fields) = data.as_object() else {
        return text::clean(&data.to_string());
    };
    fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "lane" | "started" | "message"))
        .map(|(key, value)| match value {
            Value::String(value) => format!("{key}={}", text::clean(value)),
            other => format!("{key}={}", text::clean(&other.to_string())),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `logs` hint for errors that name a unit.
fn logs_hint(data: &Value) -> Option<String> {
    let found = target(data);
    (!field(data, "unit").is_empty()).then(|| format!("devlanes logs {found} --tail 200"))
}

/// `; last answer 302` (`, expected 200` when a status was set) from an http
/// check's `ready_timeout` data, as the CLI words it; empty when nothing answered.
fn last_answer(data: &Value) -> String {
    let Some(code) = data.get("last_status").and_then(Value::as_u64) else {
        return String::new();
    };
    match data.get("expected_status").and_then(Value::as_u64) {
        Some(expected) => format!("; last answer {code}, expected {expected}"),
        None => format!("; last answer {code}"),
    }
}

/// The fix for an http check that timed out on a non-2xx answer without a
/// configured `status`, as the CLI words it.
fn status_hint(data: &Value) -> Option<String> {
    let code = data
        .get("last_status")
        .and_then(Value::as_u64)
        .filter(|code| !(200..300).contains(code))?;
    data.get("expected_status")
        .and_then(Value::as_u64)
        .is_none()
        .then(|| format!("set `ready.http.status: {code}` or use a path that answers 2xx"))
}

/// A failed helper command split into its parts. The daemon formats these as
/// "`<command>` failed (<status>): <stderr>"; structured fields win when present.
fn command_failure(data: &Value) -> Option<(String, String, String)> {
    let command = field(data, "command");
    if !command.is_empty() {
        let stderr = data
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Some((command, field(data, "status"), stderr));
    }
    let message = data.get("message").and_then(Value::as_str)?;
    let rest = message.trim_start().strip_prefix('`')?;
    let (command, rest) = rest.split_once("` failed (")?;
    let (status, stderr) = rest.split_once(')')?;
    let stderr = stderr.strip_prefix(':').unwrap_or(stderr);
    Some((
        text::clean(command),
        text::clean(status),
        stderr.trim().to_owned(),
    ))
}

/// Summary, stderr lines and the command of a failed helper command.
fn command_detail(
    what: &str,
    (command, status, stderr): (String, String, String),
) -> (String, Vec<String>, String) {
    // `exit status: 1` reads as `exit status 1`.
    let status = status.replacen(": ", " ", 1);
    let summary = if status.is_empty() {
        format!("{what} failed")
    } else {
        format!("{what} failed ({status})")
    };
    (summary, text::clean_lines(&stderr), command)
}

/// The `message` field split into a first line and the rest.
fn message_lines(data: &Value) -> (String, Vec<String>) {
    let lines = data
        .get("message")
        .and_then(Value::as_str)
        .map(text::clean_lines)
        .unwrap_or_default();
    let mut lines = lines.into_iter();
    (lines.next().unwrap_or_default(), lines.collect())
}

/// `devlanes.yaml is invalid at line 2, column 1: unknown field`, the CLI's
/// wording of `config_error` (`devlanes-cli` `render/words.rs`). Line 0
/// means the file could not be read, so it is left out.
fn config_invalid(data: &Value, message: &str) -> String {
    let file = field(data, "file");
    let path = field(data, "path");
    let file = match (file.is_empty(), path.is_empty()) {
        (false, _) => file,
        (true, false) => format!("devlanes.yaml in {path}"),
        (true, true) => "devlanes.yaml".into(),
    };
    let number = |key| data.get(key).and_then(Value::as_u64).filter(|n| *n > 0);
    match (number("line"), number("column")) {
        (Some(line), Some(column)) => {
            format!("{file} is invalid at line {line}, column {column}: {message}")
        }
        (Some(line), None) => format!("{file} is invalid at line {line}: {message}"),
        _ => format!("{file} is invalid: {message}"),
    }
}

/// Words a daemon error from its code and data.
pub fn rpc(error: &RpcError) -> Worded {
    let data = &error.data;
    let code = text::clean(&error.code);
    let reference = || {
        let value = field(data, "reference");
        if value.is_empty() {
            target(data)
        } else {
            value
        }
    };
    let (message, rest) = message_lines(data);
    let mut command = None;
    let (summary, detail, hint): (String, Vec<String>, Option<String>) = match code.as_str() {
        "compose_failed" | "docker_failed" => {
            let what = if code == "compose_failed" {
                "docker compose"
            } else {
                "docker"
            };
            match command_failure(data) {
                Some(parts) => {
                    let (summary, detail, failed) = command_detail(what, parts);
                    command = Some(failed);
                    (summary, detail, Some("devlanes infra status".into()))
                }
                None => (
                    if message.is_empty() {
                        format!("{what} failed")
                    } else {
                        format!("{what} failed: {message}")
                    },
                    rest,
                    Some("devlanes infra status".into()),
                ),
            }
        }
        "compose_unsupported" => (message, rest, Some("devlanes doctor".into())),
        "container_name_conflict" => (
            "another container already has this name".into(),
            text::clean_lines(&field_raw(data, "message")),
            None,
        ),
        "docker_unavailable" => (
            "Docker isn't running or can't be reached".into(),
            text::clean_lines(&field_raw(data, "message")),
            Some("devlanes doctor".into()),
        ),
        "not_found" => (
            match candidates(data) {
                list if list == "none" => format!("no lane or unit named {}", reference()),
                list => format!(
                    "no lane or unit named {}; close matches: {list}",
                    reference()
                ),
            },
            Vec::new(),
            Some("devlanes lane ls".into()),
        ),
        "ambiguous" => (
            format!(
                "{} matches more than one lane: {}",
                reference(),
                candidates(data)
            ),
            Vec::new(),
            None,
        ),
        "not_running" => (format!("{} is not running", target(data)), Vec::new(), None),
        // Start refuses a unit a previous daemon left running; stopping it is the way out.
        "unit_orphaned" => {
            command = Some(field(data, "command")).filter(|command| !command.is_empty());
            (
                format!(
                    "{} is already running as pid {}, started by an earlier daemon; x stops it",
                    target(data),
                    known(data, "pid")
                ),
                Vec::new(),
                None,
            )
        }
        "confirmation_required" => (
            "needs a confirmation the TUI can't give; run it from the CLI with --yes".into(),
            vec![compact_fields(data)],
            None,
        ),
        "terminal_required" => {
            command = Some(field(data, "command")).filter(|command| !command.is_empty());
            (
                match field(data, "task") {
                    task if !task.is_empty() => format!("{task} needs an interactive terminal"),
                    _ => "needs an interactive terminal".into(),
                },
                Vec::new(),
                None,
            )
        }
        "project_running_elsewhere" => (
            format!(
                "{} is already running from {}",
                known(data, "project"),
                known(data, "path")
            ),
            Vec::new(),
            None,
        ),
        "port_in_use" => (
            format!(
                "port {} is taken by pid {}",
                known(data, "port"),
                known(data, "pid")
            ),
            Vec::new(),
            None,
        ),
        "port_in_use_by_lane" => (
            format!(
                "port {} is taken by {}:{}",
                known(data, "port"),
                known(data, "lane"),
                known(data, "unit")
            ),
            Vec::new(),
            None,
        ),
        "secrets_locked" => (
            "secrets are locked".into(),
            Vec::new(),
            Some("devlanes secrets unlock".into()),
        ),
        "needs_env" => (
            "the daemon restarted and lost this lane's shell environment".into(),
            Vec::new(),
            Some(match field(data, "lane") {
                lane if lane.is_empty() => "devlanes lane resume".into(),
                lane => format!("devlanes start --lane {lane}"),
            }),
        ),
        "timeout" => (
            match target(data) {
                found if !found.is_empty() => format!("timed out waiting for {found}"),
                _ => "timed out".into(),
            },
            Vec::new(),
            None,
        ),
        "config_error" => (
            config_invalid(data, &message),
            rest,
            // `config check` reads project files only; doctor also checks the user config.
            Some(
                if known(data, "file").ends_with("/config.yaml") {
                    "devlanes doctor"
                } else {
                    "devlanes config check"
                }
                .into(),
            ),
        ),
        "no_config" => (
            "no devlanes.yaml in this worktree".into(),
            Vec::new(),
            Some("devlanes config check".into()),
        ),
        "worktree_missing" => (
            format!("worktree {} is gone", known(data, "path")),
            Vec::new(),
            Some("devlanes lane gc".into()),
        ),
        "unassigned" => (
            "lane is unassigned".into(),
            Vec::new(),
            Some("devlanes lane gc".into()),
        ),
        "pool_exhausted" => (
            "no free port blocks left".into(),
            Vec::new(),
            Some("devlanes lane gc".into()),
        ),
        "units_required" => ("name at least one unit".into(), Vec::new(), None),
        "unit_failed" => (format!("{} failed", target(data)), rest, logs_hint(data)),
        "ready_timeout" => (
            format!("{} wasn't ready in time{}", target(data), last_answer(data)),
            rest,
            status_hint(data).or_else(|| logs_hint(data)),
        ),
        "container_not_ready" => (
            format!(
                "container {} didn't pass its {} check in time{}",
                known(data, "container"),
                known(data, "check"),
                last_answer(data)
            ),
            Vec::new(),
            status_hint(data).or_else(|| Some("devlanes infra status".into())),
        ),
        "spawn_failed" => (
            format!("{} couldn't start: {message}", target(data)),
            rest,
            logs_hint(data),
        ),
        "git_failed" => (format!("git failed: {message}"), rest, None),
        "unsupported" | "unsupported_method" => (
            format!(
                "the daemon is too old for {}; upgrade and restart it",
                known(data, "method")
            ),
            Vec::new(),
            None,
        ),
        _ => {
            let words = code.replace('_', " ");
            match (message.is_empty(), compact_fields(data)) {
                (false, _) => (format!("{words}: {message}"), rest, None),
                (true, fields) if fields.is_empty() => (words, Vec::new(), None),
                (true, fields) => (format!("{words}: {fields}"), Vec::new(), None),
            }
        }
    };
    Worded {
        code: Some(code),
        summary,
        detail,
        command,
        hint,
    }
}

/// A raw string field, empty when absent; cleaned later line by line.
fn field_raw(data: &Value, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compose_failure_splits_command_status_and_stderr() {
        let error = RpcError::new(
            "compose_failed",
            json!({
                "message": "`docker compose -p acme -f /work/acme/compose.yaml up -d` failed (exit status: 1): \x1b[31mopen /work/acme/compose.yaml: no such file\x1b[0m\n\tretry later",
                "lane": "acme@1",
                "started": []
            }),
            1,
        );
        let worded = rpc(&error);
        assert_eq!(worded.summary, "docker compose failed (exit status 1)");
        assert_eq!(
            worded.detail,
            [
                "open /work/acme/compose.yaml: no such file",
                "    retry later"
            ]
        );
        assert_eq!(
            worded.command.as_deref(),
            Some("docker compose -p acme -f /work/acme/compose.yaml up -d")
        );
        assert_eq!(worded.code.as_deref(), Some("compose_failed"));
        assert_eq!(
            worded.one_line(),
            "docker compose failed (exit status 1); open /work/acme/compose.yaml: no such file; retry later"
        );
    }

    /// An http check that saw a non-2xx answer without `status` set says which
    /// code came back and how to accept it; a configured status keeps the logs hint.
    #[test]
    fn ready_timeout_names_the_last_http_answer() {
        let base = json!({"lane": "acme@0", "unit": "web", "check": "http"});
        let mut redirected = base.clone();
        redirected["last_status"] = json!(302);
        redirected["expected_status"] = Value::Null;
        let worded = rpc(&RpcError::new("ready_timeout", redirected, 1));
        assert_eq!(
            worded.summary,
            "acme@0:web wasn't ready in time; last answer 302"
        );
        assert_eq!(
            worded.hint.as_deref(),
            Some("set `ready.http.status: 302` or use a path that answers 2xx")
        );
        let mut expected = base.clone();
        expected["last_status"] = json!(404);
        expected["expected_status"] = json!(200);
        let worded = rpc(&RpcError::new("ready_timeout", expected, 1));
        assert_eq!(
            worded.summary,
            "acme@0:web wasn't ready in time; last answer 404, expected 200"
        );
        assert_eq!(
            worded.hint.as_deref(),
            Some("devlanes logs acme@0:web --tail 200")
        );
        let worded = rpc(&RpcError::new("ready_timeout", base, 1));
        assert_eq!(worded.summary, "acme@0:web wasn't ready in time");
    }

    /// A container's http check names its last answer like a service's, and a
    /// user config error points at doctor, which checks that file.
    #[test]
    fn container_timeouts_and_user_config_errors_have_their_fix() {
        let data = json!({"container": "minio", "check": "http", "last_status": 403, "expected_status": null});
        let worded = rpc(&RpcError::new("container_not_ready", data, 1));
        assert_eq!(
            worded.summary,
            "container minio didn't pass its http check in time; last answer 403"
        );
        assert_eq!(
            worded.hint.as_deref(),
            Some("set `ready.http.status: 403` or use a path that answers 2xx")
        );
        let user = json!({"message": "tui.theme: unknown variant", "file": "/home/acme/.devlanes/config.yaml", "line": 4, "column": 10});
        let worded = rpc(&RpcError::new("config_error", user, 1));
        assert_eq!(worded.hint.as_deref(), Some("devlanes doctor"));
    }

    #[test]
    fn unknown_codes_use_message_or_fields_without_debug_text() {
        let worded = rpc(&RpcError::new(
            "state_error",
            json!({"message": "disk full"}),
            1,
        ));
        assert_eq!(worded.summary, "state error: disk full");
        let worded = rpc(&RpcError::new(
            "bad_thing",
            json!({"port": 80, "lane": "acme@0"}),
            1,
        ));
        assert_eq!(worded.summary, "bad thing: port=80");
        let worded = api(&ApiError::Disconnected("client I/O: broken pipe".into()));
        assert_eq!(worded.summary, "can't reach the daemon");
        assert_eq!(worded.detail, ["client I/O: broken pipe"]);
    }

    #[test]
    fn one_line_collapses_detail() {
        let worded = Worded::local("could not open\nsecond line");
        assert_eq!(worded.one_line(), "could not open; second line");
    }
}
