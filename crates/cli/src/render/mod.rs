//! Text renderers for every daemon result and event shape (human, plain, agent).
//!
//! JSON mode never reaches this module: it prints the daemon data unchanged. Each
//! renderer returns lines; the reporter adds continuation notices and applies the
//! agent budget. Agent lines that describe a state start with the lowercase state
//! word (`ready`, `failed`, `stopped`, `starting`, `idle`) so agents can match them.

mod lanes;
mod other;
mod words;

pub(crate) use devlanes_proto::exit::{INTERRUPTED, ctrl_c_exit};
use devlanes_proto::{Event, LeftProcess, Method, events};
use serde_json::Value;

pub(crate) use lanes::display_url;
pub(crate) use other::doctor_failed;
pub(crate) use words::{EXAMPLE_CONFIG, error_message, has_wording};

use crate::{OutputMode, Runtime};

/// Presentation settings for one rendering call.
pub(crate) struct Ctx<'a> {
    pub mode: OutputMode,
    pub color: bool,
    pub runtime: &'a Runtime,
}

impl Ctx<'_> {
    /// Agent mode: terse, failures first, no symbols.
    pub fn agent(&self) -> bool {
        self.mode == OutputMode::Agent
    }

    /// Human mode: symbols and (when enabled) color.
    pub fn human(&self) -> bool {
        self.mode == OutputMode::Human
    }

    /// Wraps `text` in an SGR color when color is on.
    pub fn paint(&self, text: &str, sgr: &str) -> String {
        if self.color {
            format!("\x1b[{sgr}m{text}\x1b[0m")
        } else {
            text.into()
        }
    }

    /// A state word, colored by meaning.
    pub fn state(&self, word: &str) -> String {
        self.state_text(word, word)
    }

    /// `text` colored by the meaning of `state` (`failed (1)` stays red).
    pub fn state_text(&self, state: &str, text: &str) -> String {
        let sgr = match state {
            "ready" | "running" | "ok" | "applied" | "exited" => "32",
            "failed" | "fail" | "error" => "31",
            "starting" | "stopping" | "warn" | "reconnecting" | "unassigned" | "orphaned" => "33",
            // Queued, idle and stopped units are dim.
            _ => "2",
        };
        self.paint(text, sgr)
    }

    /// A unit name in its tag color (human mode only); failed units are red, the
    /// color the daemon never assigns from tags.
    pub fn unit_name(&self, name: &str, state: &str, color: Option<&str>) -> String {
        if !self.human() {
            return name.to_owned();
        }
        let color = if state == "failed" {
            Some("red")
        } else {
            color
        };
        match color.and_then(palette_sgr) {
            Some(sgr) => self.paint(name, sgr),
            None => name.to_owned(),
        }
    }

    /// Outcome marker for phase and check lines: a symbol in human mode, a word otherwise.
    pub fn mark(&self, outcome: Outcome) -> String {
        let (symbol, word, sgr) = match outcome {
            Outcome::Ok => ("✓", "ok", "32"),
            Outcome::Fail => ("✗", "failed", "31"),
            Outcome::Skip => ("!", "skipped", "33"),
        };
        if self.human() {
            self.paint(symbol, sgr)
        } else {
            word.into()
        }
    }

    /// Advice that blocked nothing: `! text` in human mode, `warn text` otherwise.
    pub fn warning(&self, text: &str) -> String {
        if self.human() {
            format!("{} {text}", self.mark(Outcome::Skip))
        } else {
            format!("warn {text}")
        }
    }

    /// A skipped step: `! seed skipped: why` in human mode, `skipped seed: why` otherwise.
    pub fn skipped(&self, subject: &str, reason: Option<&str>) -> String {
        let reason = reason.map(|r| format!(": {r}")).unwrap_or_default();
        if self.human() {
            format!("{} {subject} skipped{reason}", self.mark(Outcome::Skip))
        } else {
            format!("skipped {subject}{reason}")
        }
    }
}

/// SGR code of a `devlanes_core::color` palette name.
fn palette_sgr(color: &str) -> Option<&'static str> {
    Some(match color {
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "blue" => "34",
        "magenta" => "35",
        "cyan" => "36",
        "white" => "37",
        "gray" => "90",
        "bright-red" => "91",
        "bright-green" => "92",
        "bright-yellow" => "93",
        "bright-blue" => "94",
        "bright-magenta" => "95",
        "bright-cyan" => "96",
        _ => return None,
    })
}

/// `1 unit`, `2 units`.
pub(crate) fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else if word.ends_with('s') {
        format!("{count} {word}es")
    } else {
        format!("{count} {word}s")
    }
}

/// Result of a step or check, for [`Ctx::mark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ok,
    Fail,
    Skip,
}

/// Lines for a final result of `method`.
pub(crate) fn result(method: Method, data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    match method {
        Method::Start | Method::Restart => lanes::start_like(data, cx),
        Method::Resume => lanes::resume(data, cx),
        Method::Stop => lanes::stop(data, cx),
        Method::Status | Method::LaneList => lanes::status(data, cx),
        Method::LaneNew => lanes::lane_new(data, cx),
        Method::LaneClean => lanes::lane_clean(data, cx),
        Method::LaneGc => lanes::lane_gc(data),
        Method::Exec => lanes::exec(data, cx),
        Method::Wait => lanes::wait(data, cx),
        Method::Errors => lanes::errors(data, cx),
        Method::Why => lanes::why(data, cx),
        Method::Hosts => lanes::hosts(data, cx),
        Method::Logs => other::logs(data, cx),
        Method::Run => other::run(data, cx),
        Method::TaskList => other::tasks(data, cx),
        Method::Env => other::env(data),
        Method::Url => other::url(data),
        Method::Metrics => other::metrics(data, cx),
        Method::Subscribe => other::events_page(data, cx),
        Method::InfraUp | Method::InfraStatus | Method::InfraDown => other::infra(method, data),
        Method::Doctor => other::doctor(data, cx),
        Method::ConfigCheck => other::config_check(data, cx),
        Method::Unlock => other::unlock(data),
        Method::DaemonStatus => other::daemon_status(data),
        Method::DaemonShutdown => other::daemon_shutdown(data),
        Method::DaemonResetState => other::reset_state(data),
        _ => tree(data),
    }
}

/// Progress lines for a streamed event; empty when the event adds nothing to the
/// final result. Followed log lines are rendered by the reporter itself.
pub(crate) fn event(method: Method, event: &Event, cx: &Ctx<'_>) -> Vec<String> {
    let starting = matches!(
        method,
        Method::Start | Method::Restart | Method::Resume | Method::LaneNew
    );
    match event.event.as_str() {
        events::PHASE if starting => other::phase_line(&event.data, cx).into_iter().collect(),
        events::TASK if starting => other::task_line(&event.data, cx).into_iter().collect(),
        // The final result lists every unit; transitions would only repeat it.
        events::UNIT_CHANGED | events::LANE_CHANGED if starting => Vec::new(),
        // `wait` streams its lane's events for the progress bar only.
        events::PHASE | events::TASK | events::UNIT_CHANGED | events::LANE_CHANGED
            if method == Method::Wait =>
        {
            Vec::new()
        }
        // `{lane, code, data}`; a start names its own lane already.
        events::LANE_WARNING => {
            let line = words::lane_warning(&text(&event.data, "code"), &event.data["data"]);
            let lane = text(&event.data, "lane");
            vec![if starting || lane.is_empty() {
                cx.warning(&line)
            } else {
                cx.warning(&format!("{lane}: {line}"))
            }]
        }
        devlanes_proto::remote::WARNING_EVENT => vec![cx.warning(&words::remote_warning(
            &text(&event.data, "code"),
            &event.data,
        ))],
        _ => vec![other::event_line(event, cx)],
    }
}

/// Removes terminal escapes and control bytes before rendering untrusted text.
pub(crate) fn clean(value: &str) -> String {
    strip_ansi_escapes::strip_str(value)
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect()
}

/// Caps characters rather than slicing UTF-8 bytes; URLs never go through this.
pub(crate) fn cap_line(value: &str, limit: usize) -> String {
    if value.chars().count() > limit {
        format!(
            "{}...",
            value
                .chars()
                .take(limit.saturating_sub(3))
                .collect::<String>()
        )
    } else {
        value.into()
    }
}

/// A scalar as text; strings unquoted, everything else as compact JSON.
pub(crate) fn display_value(value: &Value) -> String {
    value
        .as_str()
        .map(clean)
        .unwrap_or_else(|| clean(&value.to_string()))
}

/// `key=value` pairs, so unknown additive fields stay visible.
pub(crate) fn compact_fields(value: &Value) -> String {
    if let Some(fields) = value.as_object() {
        fields
            .iter()
            .map(|(key, value)| format!("{}={}", clean(key), display_value(value)))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        display_value(value)
    }
}

/// Cleaned string field, empty when absent.
pub(crate) fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(clean)
        .unwrap_or_default()
}

/// ` exit=<code>`, appended to status lines. A Ctrl-C exit reads
/// ` interrupted (Ctrl-C)`, except for `agent` output, whose readers scan the
/// `key=value` pairs and get ` exit=<code> (Ctrl-C)`.
pub(crate) fn exit_field(code: impl Into<i64>, agent: bool) -> String {
    let code = code.into();
    if !ctrl_c_exit(code) {
        format!(" exit={code}")
    } else if agent {
        format!(" exit={code} (Ctrl-C)")
    } else {
        format!(" {INTERRUPTED}")
    }
}

/// `exited with code <code>`, or `interrupted (Ctrl-C)`.
pub(crate) fn exited_with(code: impl Into<i64>) -> String {
    let code = code.into();
    if ctrl_c_exit(code) {
        INTERRUPTED.into()
    } else {
        format!("exited with code {code}")
    }
}

/// Integer field.
pub(crate) fn int(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

/// Array field, empty when absent.
pub(crate) fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Visible width, ignoring color sequences.
fn width(cell: &str) -> usize {
    strip_ansi_escapes::strip_str(cell).chars().count()
}

/// Aligns `rows` into columns two spaces apart; the last column is not padded.
pub(crate) fn table(rows: &[Vec<String>], indent: &str) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(width(cell));
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = indent.to_owned();
            for (index, cell) in row.iter().enumerate() {
                line.push_str(cell);
                if index + 1 < row.len() {
                    line.push_str(&" ".repeat(widths[index] - width(cell) + 2));
                }
            }
            line.trim_end().to_owned()
        })
        .collect()
}

/// Binary-prefixed size with one decimal, matching the CI resource summary.
pub(crate) fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if value < 1024 {
        return format!("{value} B");
    }
    let mut size = value as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// CPU share with one decimal below 10 % and none above.
pub(crate) fn percent(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.1}%")
    } else {
        format!("{value:.0}%")
    }
}

/// Milliseconds as `2h 3m`, `4m 10s` or `12s`.
pub(crate) fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Last ≤20 tail lines as `  | line`, each capped at 300 characters.
pub(crate) fn tail_lines(tail: &[Value]) -> Vec<String> {
    let skip = tail.len().saturating_sub(20);
    tail.iter()
        .skip(skip)
        .filter_map(|v| v.as_str().or_else(|| v.get("text").and_then(Value::as_str)))
        .map(|line| format!("  | {}", cap_line(&clean(line), 300)))
        .collect()
}

/// One line per non-empty list: `left running: pg_ctl (pid 4242)` and
/// `escaped: node (pid 51)`, prefixed with `prefix` (such as `setup.db: `).
pub(crate) fn leftover_notes(
    prefix: &str,
    left_running: &[LeftProcess],
    escaped: &[LeftProcess],
) -> Vec<String> {
    [("left running", left_running), ("escaped", escaped)]
        .into_iter()
        .filter(|(_, processes)| !processes.is_empty())
        .map(|(label, processes)| {
            let names: Vec<String> = processes
                .iter()
                .map(|process| format!("{} (pid {})", clean(&process.command), process.pid))
                .collect();
            format!("{prefix}{label}: {}", names.join(", "))
        })
        .collect()
}

/// [`leftover_notes`] for a JSON object with `left_running`/`escaped` arrays.
pub(crate) fn leftover_notes_of(prefix: &str, value: &Value) -> Vec<String> {
    let processes = |key: &str| -> Vec<LeftProcess> {
        value
            .get(key)
            .and_then(|list| serde_json::from_value(list.clone()).ok())
            .unwrap_or_default()
    };
    leftover_notes(prefix, &processes("left_running"), &processes("escaped"))
}

/// Indented `key: value` view of any JSON, for shapes without a dedicated renderer
/// (infra reports, remote listings). Nested objects indent; scalar arrays join.
pub(crate) fn tree(data: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    tree_into(data, "", &mut lines);
    lines
}

/// Appends `value` under `indent` to `lines`.
fn tree_into(value: &Value, indent: &str, lines: &mut Vec<String>) {
    match value {
        Value::Null => {}
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "truncated" | "next_cursor") {
                    continue;
                }
                let key = clean(key);
                match value {
                    Value::Object(inner) if !inner.is_empty() => {
                        lines.push(format!("{indent}{key}:"));
                        tree_into(value, &format!("{indent}  "), lines);
                    }
                    Value::Array(items) if items.iter().any(|v| v.is_object()) => {
                        lines.push(format!("{indent}{key}:"));
                        for item in items {
                            tree_into(item, &format!("{indent}  "), lines);
                            if item.is_object() {
                                lines.push(String::new());
                            }
                        }
                        if lines.last().is_some_and(String::is_empty) {
                            lines.pop();
                        }
                    }
                    other => lines.push(format!("{indent}{key}: {}", scalar_list(other))),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.is_object() {
                    tree_into(item, indent, lines);
                } else {
                    lines.push(format!("{indent}{}", display_value(item)));
                }
            }
        }
        other => lines.push(format!("{indent}{}", display_value(other))),
    }
}

/// Scalars as text; arrays of scalars comma-joined; `-` for null and empty.
fn scalar_list(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::Array(items) if items.is_empty() => "-".into(),
        Value::Array(items) => items
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(fields) if fields.is_empty() => "-".into(),
        other => display_value(other),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn tables_align_on_visible_width() {
        let rows = vec![
            vec!["\x1b[32mready\x1b[0m".to_owned(), "api".into()],
            vec!["failed".into(), "web".into()],
        ];
        assert_eq!(
            table(&rows, "  "),
            [
                "  \x1b[32mready\x1b[0m   api".to_owned(),
                "  failed  web".into()
            ]
        );
    }

    #[test]
    fn sizes_and_durations_are_readable() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 + 200 * 1024 * 1024), "3.2 GiB");
        assert_eq!(percent(4.25), "4.2%");
        assert_eq!(percent(42.4), "42%");
        assert_eq!(duration(7_380_000), "2h 3m");
        assert_eq!(duration(12_000), "12s");
    }

    #[test]
    fn tree_indents_nested_values() {
        let data =
            json!({"shared": {"project": "acme", "services": ["db", "redis"]}, "lane": null});
        assert_eq!(
            tree(&data),
            [
                "shared:",
                "  project: acme",
                "  services: db, redis",
                "lane: -"
            ]
        );
    }
}
