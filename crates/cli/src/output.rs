use std::collections::BTreeMap;

use devlanes_proto::{ActorKind, Event, LaneSummary, LogLine, Method, RpcError, UnitSummary};
use serde_json::{Value, json};

use crate::{OutputMode, Runtime, exit};

/// A shell-safe command that can be pasted regardless of the caller's lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint(pub String);

impl Hint {
    /// Quotes each argument separately; selectors remain opaque to the CLI.
    pub fn command(args: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self(
            std::iter::once("devlanes".into())
                .chain(args.into_iter().map(|v| shell_quote(v.as_ref())))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// Qualifies the target so failure tails remain useful outside the original cwd.
    pub fn logs(lane: &str, unit: &str, tail: usize) -> Self {
        Self::command([
            "logs".into(),
            format!("{lane}:{unit}"),
            "--tail".into(),
            tail.to_string(),
        ])
    }
}

/// Rendering input preserves the wire payload for JSON output.
pub struct Report<'a> {
    pub method: Method,
    pub data: &'a Value,
    pub more: Option<Hint>,
}

impl<'a> Report<'a> {
    /// Supplies the method so scalar and list results can retain command semantics.
    pub fn new(method: Method, data: &'a Value) -> Self {
        Self {
            method,
            data,
            more: None,
        }
    }

    /// Adds the exact cursor/query command when the transport handler has that context.
    pub fn with_more(mut self, hint: Hint) -> Self {
        self.more = Some(hint);
        self
    }
}

/// Keep data and diagnostics separate until the binary writes its standard streams.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
}

/// Clients render typed responses through one policy without printing in handlers.
pub trait Reporter: Send + Sync {
    /// Returns the resolved mode, including explicit overrides.
    fn mode(&self) -> OutputMode;
    /// Exposes shared color, budget and attribution policy.
    fn runtime(&self) -> &Runtime;

    /// JSON is serialized unchanged; text modes shape data on the client only.
    fn result(&self, report: &Report<'_>) -> Result<Rendered, serde_json::Error> {
        let exit_code = result_exit_code(report.data);
        if self.runtime().quiet && self.mode() != OutputMode::Json && exit_code == 0 {
            return Ok(Rendered {
                stderr: ownership_note(report, self.runtime()),
                ..Rendered::default()
            });
        }
        if self.mode() == OutputMode::Json {
            return Ok(Rendered {
                stdout: format!("{}\n", serde_json::to_string(report.data)?),
                exit_code,
                ..Rendered::default()
            });
        }
        let agent = self.mode() == OutputMode::Agent;
        let mut lines = text_result(report.method, report.data, agent, self.runtime().color);
        if report.data.get("truncated").and_then(Value::as_bool) == Some(true)
            || agent && report.method == Method::Logs && logs_shaped(report.data)
        {
            lines.push(continuation(report));
        }
        let mut stdout = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        let stderr = ownership_note(report, self.runtime());
        if agent {
            stdout = budget_output(
                &stdout,
                self.runtime().budget.saturating_sub(stderr.len()),
                &continuation(report),
            );
        }
        Ok(Rendered {
            stdout,
            stderr,
            exit_code,
        })
    }

    /// Progress is silent in agent mode; explicit log/event subscriptions still stream.
    fn event(&self, method: Method, event: &Event) -> Result<Rendered, serde_json::Error> {
        if self.mode() == OutputMode::Json {
            return Ok(Rendered {
                stdout: format!("{}\n", serde_json::to_string(event)?),
                exit_code: if event.event == "result" {
                    result_exit_code(&event.data)
                } else {
                    exit::OK
                },
                ..Rendered::default()
            });
        }
        if event.event == "result" {
            return self.result(&Report::new(method, &event.data));
        }
        if self.runtime().quiet
            || self.mode() == OutputMode::Agent
                && !matches!(method, Method::Logs | Method::Subscribe)
        {
            return Ok(Rendered::default());
        }
        let stdout = if method == Method::Logs {
            self.result(&Report::new(method, &event.data))?.stdout
        } else {
            format!("{} {}\n", clean(&event.event), compact_fields(&event.data))
        };
        Ok(Rendered {
            stdout,
            ..Rendered::default()
        })
    }

    /// Errors always go to stderr in the same one-line/context/hint shape.
    fn error(&self, error: &RpcError, hint: Option<&Hint>) -> Rendered {
        let (message, fallback_hint) = error_message(error);
        let mut stderr = format!("error: {}\n", clean(&message));
        if let Some(context) = error.data.get("context").and_then(Value::as_array) {
            for line in context {
                if let Some(line) = line.as_str() {
                    stderr.push_str(&format!("  {}\n", clean(line)));
                }
            }
        }
        if let Some(hint) = hint.or(fallback_hint.as_ref()) {
            stderr.push_str(&format!("hint: {}\n", hint.0));
        }
        if self.mode() == OutputMode::Agent {
            stderr = budget_output(
                &stderr,
                self.runtime().budget,
                "truncated; rerun this command with --output plain",
            );
        }
        Rendered {
            stderr,
            exit_code: error.exit_code,
            ..Rendered::default()
        }
    }
}

pub struct HumanReporter(pub Runtime);
pub struct PlainReporter(pub Runtime);
pub struct AgentReporter(pub Runtime);
pub struct JsonReporter(pub Runtime);

/// One command-wide budget for follow mode, rather than a fresh budget per event.
pub struct ReporterStream<'a> {
    reporter: &'a dyn Reporter,
    method: Method,
    remaining: usize,
    more: Hint,
    exhausted: bool,
}

impl<'a> ReporterStream<'a> {
    /// The continuation should include the original selectors and query filters.
    pub fn new(reporter: &'a dyn Reporter, method: Method, more: Hint) -> Self {
        Self {
            remaining: reporter.runtime().budget,
            reporter,
            method,
            more,
            exhausted: false,
        }
    }

    /// Emits at most one truncation notice; callers can detach once exhausted.
    pub fn event(&mut self, event: &Event) -> Result<Rendered, serde_json::Error> {
        if self.exhausted {
            return Ok(Rendered::default());
        }
        let mut rendered = self.reporter.event(self.method, event)?;
        if self.reporter.mode() != OutputMode::Agent || rendered.stdout.is_empty() {
            return Ok(rendered);
        }
        let notice = format!("truncated; more: {}", self.more.0);
        let reserve = (notice.len() + 1).min(64.max(self.reporter.runtime().budget / 2));
        if rendered.stdout.len() + reserve > self.remaining {
            // Reserve the notice before consuming the final event, including exact-boundary cases.
            rendered.stdout = truncate_output(&rendered.stdout, self.remaining, &notice);
            self.exhausted = true;
        }
        self.remaining = self.remaining.saturating_sub(rendered.stdout.len());
        Ok(rendered)
    }

    /// Allows the transport to stop reading a bounded agent stream without stopping units.
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
}

macro_rules! reporter {
    ($name:ident, $mode:ident) => {
        impl Reporter for $name {
            /// Identifies this renderer's fixed presentation contract.
            fn mode(&self) -> OutputMode {
                OutputMode::$mode
            }
            /// Shares invocation settings without redetecting the caller.
            fn runtime(&self) -> &Runtime {
                &self.0
            }
        }
    };
}
reporter!(HumanReporter, Human);
reporter!(PlainReporter, Plain);
reporter!(AgentReporter, Agent);
reporter!(JsonReporter, Json);

impl Runtime {
    /// Selects one of the four reporter implementations without transport coupling.
    pub fn reporter(&self) -> Box<dyn Reporter> {
        match self.mode {
            OutputMode::Human => Box::new(HumanReporter(self.clone())),
            OutputMode::Plain if self.ci.is_some() => Box::new(crate::CiPlainReporter::new(
                self.clone(),
                self.ci_provider(),
            )),
            OutputMode::Plain => Box::new(PlainReporter(self.clone())),
            OutputMode::Agent => Box::new(AgentReporter(self.clone())),
            OutputMode::Json => Box::new(JsonReporter(self.clone())),
        }
    }

    /// Adds client-only diagnosis after receiving the daemon's doctor result.
    pub fn doctor_output(&self, daemon: &Value) -> Value {
        json!({"daemon": daemon, "output": {"mode": self.mode.as_str(), "reason": self.reason, "color": self.color, "ci": self.ci, "description": self.output_description()}})
    }
}

/// Render is local so protocol types remain free of CLI dependencies.
pub trait Render {
    /// Returns text lines without writing to stdout or altering the wire value.
    fn text(&self, agent: bool, color: bool) -> Vec<String>;
}

impl Render for UnitSummary {
    /// Keeps canonical targets and full URLs on status lines in every text mode.
    fn text(&self, agent: bool, color: bool) -> Vec<String> {
        let state = clean(&self.state.to_lowercase());
        let label = if color {
            format!(
                "\x1b[{}m{}\x1b[0m",
                if state == "failed" { "31" } else { "32" },
                state
            )
        } else {
            state
        };
        let mut line = format!("{label} {}:{}", clean(&self.lane), clean(&self.name));
        if let Some(code) = self.exit_code {
            line.push_str(&format!(" exit={code}"));
        }
        for url in &self.urls {
            line.push(' ');
            line.push_str(&clean(url));
        }
        for (name, port) in &self.ports {
            line.push_str(&format!(" {}={port}", clean(name)));
        }
        if let Some(id) = &self.actor.agent_id {
            line.push_str(&format!(" actor={}", clean(id)));
        }
        let mut lines = vec![line];
        if let Some(reason) = &self.reason {
            lines.push(format!("  {}", error_message(reason).0));
        }
        if agent && self.state == "failed" {
            lines.push(format!(
                "  more: {}",
                Hint::logs(&self.lane, &self.name, 200).0
            ));
        }
        lines
    }
}

impl Render for LaneSummary {
    /// Failed units precede ready units without changing the API's collection order.
    fn text(&self, agent: bool, color: bool) -> Vec<String> {
        let mut line = format!(
            "{} {}",
            clean(&self.state.to_lowercase()),
            clean(&self.reference)
        );
        if let Some(branch) = &self.branch {
            line.push_str(&format!(" ({})", clean(branch)));
        }
        if self.config_changed {
            line.push_str(" config changed");
        }
        let mut result = vec![line];
        let mut units: Vec<_> = self.units.iter().collect();
        if agent {
            units.sort_by_key(|u| u.state != "failed");
        }
        for unit in units {
            result.extend(unit.text(agent, color));
        }
        result
    }
}

impl Render for LogLine {
    /// Cursors remain copyable even when text is capped by agent presentation policy.
    fn text(&self, agent: bool, _color: bool) -> Vec<String> {
        let text = clean(&self.text);
        let text = if agent { cap_line(&text, 300) } else { text };
        vec![format!("{}#{} {}", clean(&self.unit), self.seq, text)]
    }
}

/// Produces shell assignments without evaluating values received from the daemon.
pub fn export_environment(values: &BTreeMap<String, String>) -> Result<String, crate::Error> {
    let mut output = String::new();
    for (key, value) in values {
        let mut chars = key.chars();
        if !chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
            || value.contains('\0')
        {
            return Err(crate::Error::Usage(format!(
                "cannot export invalid shell variable {key:?}"
            )));
        }
        output.push_str(&format!("export {key}={}\n", shell_quote(value)));
    }
    Ok(output)
}

/// Quotes POSIX shell metacharacters, including empty strings and embedded quotes.
///
/// ```
/// assert_eq!(devlanes_cli::shell_quote("$(id)"), "'$(id)'");
/// ```
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// Removes terminal escapes and control bytes before rendering untrusted logs.
fn clean(value: &str) -> String {
    strip_ansi_escapes::strip_str(value)
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect()
}

/// Caps characters rather than slicing UTF-8 bytes; URLs bypass this log-only helper.
fn cap_line(value: &str, limit: usize) -> String {
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

/// Typed payloads receive specialized rendering; unknown additive fields remain inspectable.
fn text_result(method: Method, data: &Value, agent: bool, color: bool) -> Vec<String> {
    if method == Method::Url {
        if let Some(url) = data
            .as_str()
            .or_else(|| data.get("url").and_then(Value::as_str))
        {
            return vec![clean(url)];
        }
        if let Some(urls) = data.get("urls").and_then(Value::as_array) {
            return urls.iter().filter_map(Value::as_str).map(clean).collect();
        }
    }
    if let Some(items) = data
        .as_array()
        .or_else(|| data.get("items").and_then(Value::as_array))
    {
        if method == Method::Logs && agent {
            return agent_logs(items);
        }
        let mut items: Vec<_> = items.iter().collect();
        if agent {
            items.sort_by_key(|v| v.get("state").and_then(Value::as_str) != Some("failed"));
        }
        return items
            .into_iter()
            .flat_map(|v| text_result(method, v, agent, color))
            .collect();
    }
    if data.get("reference").is_some()
        && let Ok(lane) = serde_json::from_value::<LaneSummary>(data.clone())
    {
        return lane.text(agent, color);
    }
    if data.get("state").is_some()
        && let Ok(unit) = serde_json::from_value::<UnitSummary>(data.clone())
    {
        let mut result = unit.text(agent, color);
        if unit.state == "failed"
            && let Some(tail) = data.get("log_tail").and_then(Value::as_array)
        {
            let insert = if agent {
                result.len().saturating_sub(1)
            } else {
                result.len()
            };
            let lines = tail
                .iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .filter_map(|v| v.as_str().or_else(|| v.get("text").and_then(Value::as_str)))
                .map(|line| format!("  | {}", cap_line(&clean(line), 300)));
            result.splice(insert..insert, lines);
        }
        return result;
    }
    if method == Method::Logs
        && let Ok(line) = serde_json::from_value::<LogLine>(data.clone())
    {
        return line.text(agent, color);
    }
    match data {
        Value::Null => Vec::new(),
        Value::String(value) => vec![clean(value)],
        Value::Object(fields) => fields
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "truncated" | "next_cursor"))
            .map(|(key, value)| format!("{}: {}", clean(key), display_value(value)))
            .collect(),
        _ => vec![display_value(data)],
    }
}

/// Collapses adjacent repeats and dependency stack frames while keeping the first cursor.
fn agent_logs(items: &[Value]) -> Vec<String> {
    let mut output = Vec::new();
    let mut previous: Option<(String, String, usize)> = None;
    let mut folded = 0;
    for value in items {
        let Ok(line) = serde_json::from_value::<LogLine>(value.clone()) else {
            flush_repeat(&mut previous, &mut output);
            output.push(compact_fields(value));
            continue;
        };
        let text = clean(&line.text);
        if text.trim_start().starts_with("at ") && text.contains("node_modules") {
            flush_repeat(&mut previous, &mut output);
            folded += 1;
            continue;
        }
        if folded > 0 {
            output.push(format!("  ({folded} node_modules frames folded)"));
            folded = 0;
        }
        let key = format!("{}\0{text}", line.unit);
        if let Some((old, _, count)) = &mut previous
            && *old == key
        {
            *count += 1;
            continue;
        }
        flush_repeat(&mut previous, &mut output);
        previous = Some((
            key,
            format!(
                "{}#{} {}",
                clean(&line.unit),
                line.seq,
                cap_line(&text, 300)
            ),
            1,
        ));
    }
    flush_repeat(&mut previous, &mut output);
    if folded > 0 {
        output.push(format!("  ({folded} node_modules frames folded)"));
    }
    output
}

/// Flushes only after a text change so long repeat runs take constant extra memory.
fn flush_repeat(previous: &mut Option<(String, String, usize)>, output: &mut Vec<String>) {
    if let Some((_, mut line, count)) = previous.take() {
        if count > 1 {
            line.push_str(&format!(" (x{count})"));
        }
        output.push(line);
    }
}

/// Uses a complete line budget so URLs and multibyte text are never cut mid-token.
fn budget_output(text: &str, budget: usize, notice: &str) -> String {
    if text.len() <= budget {
        return text.into();
    }
    truncate_output(text, budget, notice)
}

/// Reserves the notice before taking whole lines, also for an unfinished stream.
fn truncate_output(text: &str, budget: usize, notice: &str) -> String {
    let notice = if notice.len() + 1 > budget {
        "truncated; rerun with --output json --budget 1048576\n".into()
    } else {
        format!("{notice}\n")
    };
    let available = budget.saturating_sub(notice.len());
    let mut result = String::new();
    for line in text.split_inclusive('\n') {
        if result.len() + line.len() > available {
            break;
        }
        result.push_str(line);
    }
    result.push_str(&notice);
    result
}

/// Prefers the handler's exact query, preserving filters and continuation cursor.
fn continuation(report: &Report<'_>) -> String {
    if let Some(hint) = &report.more {
        return format!("truncated; more: {}", hint.0);
    }
    let command = if report.method == Method::Logs {
        "logs"
    } else {
        "status"
    };
    let mut args = vec![command.to_string()];
    if command == "logs" {
        if let Some(unit) = report
            .data
            .get("items")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(|v| v.get("unit"))
            .and_then(Value::as_str)
        {
            args.push(unit.into());
        } else {
            args.push("--mine".into());
        }
        if let Some(cursor) = report.data.get("next_cursor").and_then(Value::as_str) {
            args.extend(["--cursor".into(), cursor.into()]);
        } else {
            args.extend(["--tail".into(), "200".into()]);
        }
    }
    args.extend([
        "--output".into(),
        "json".into(),
        "--budget".into(),
        "1048576".into(),
    ]);
    format!("truncated; more: {}", Hint::command(args).0)
}

/// Status failures and foreground task exit codes are reflected in process status.
fn result_exit_code(data: &Value) -> u8 {
    if let Some(items) = data.as_array()
        && items.iter().any(|v| result_exit_code(v) != 0)
    {
        return exit::FAILED;
    }
    if let Some(code) = data.get("exit_code").and_then(Value::as_i64)
        && code != 0
    {
        return u8::try_from(code).unwrap_or(exit::FAILED);
    }
    if data.get("state").and_then(Value::as_str) == Some("failed") {
        return exit::FAILED;
    }
    for key in ["items", "units"] {
        if let Some(items) = data.get(key).and_then(Value::as_array)
            && items.iter().any(|v| result_exit_code(v) != 0)
        {
            return exit::FAILED;
        }
    }
    exit::OK
}

/// A capped line or folded frame always carries instructions to retrieve full text.
fn logs_shaped(data: &Value) -> bool {
    if let Some(items) = data
        .as_array()
        .or_else(|| data.get("items").and_then(Value::as_array))
    {
        return items.iter().any(logs_shaped);
    }
    data.get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| {
            let text = clean(text);
            text.chars().count() > 300
                || text.trim_start().starts_with("at ") && text.contains("node_modules")
        })
}

/// Reports mixed ownership after a mutation without refusing the requested operation.
fn ownership_note(report: &Report<'_>, runtime: &Runtime) -> String {
    if runtime.mode != OutputMode::Agent || !matches!(report.method, Method::Stop | Method::Restart)
    {
        return String::new();
    }
    let items = report
        .data
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| report.data.as_array());
    if items.is_some_and(|items| {
        items.iter().any(|item| {
            item.get("actor")
                .and_then(|a| serde_json::from_value::<devlanes_proto::Actor>(a.clone()).ok())
                .is_some_and(|a| a.kind == ActorKind::Human)
        })
    }) {
        "note: affected units started by a human\n".into()
    } else {
        String::new()
    }
}

/// Words machine codes locally; the daemon never supplies terminal instructions.
fn error_message(error: &RpcError) -> (String, Option<Hint>) {
    let field = |key: &str| {
        error
            .data
            .get(key)
            .map(display_value)
            .unwrap_or_else(|| "unknown".into())
    };
    match error.code.as_str() {
        "port_in_use" => (
            format!("port {} is in use by PID {}", field("port"), field("pid")),
            None,
        ),
        "not_in_profile" => (
            format!("unit is not selected by profile {}", field("profile")),
            None,
        ),
        "autostart_disabled" => ("unit has autostart disabled".into(), None),
        "timeout" => (
            "operation timed out".into(),
            Some(Hint::command(["status"])),
        ),
        "not_found" => (
            format!("target not found: {}", field("target")),
            Some(Hint::command(["lane", "ls"])),
        ),
        "ambiguous" => (
            format!(
                "target is ambiguous: {}; candidates: {}",
                field("target"),
                field("candidates")
            ),
            None,
        ),
        "unsupported" | "unsupported_method" => {
            (format!("daemon does not support {}", field("method")), None)
        }
        "incompatible" | "protocol_mismatch" => (
            format!(
                "daemon v{} (build {}) runs {} processes; protocol is incompatible",
                field("version"),
                field("build_id"),
                field("managed_processes")
            ),
            Some(Hint::command(["daemon", "restart"])),
        ),
        "daemon_unreachable" => (
            "daemon is unreachable".into(),
            Some(Hint::command(["daemon", "install"])),
        ),
        "confirmation_required" => ("confirmation required; add --yes".into(), None),
        _ => (
            format!(
                "{}{}",
                clean(&error.code),
                if error.data.is_null() || error.data == json!({}) {
                    String::new()
                } else {
                    format!(": {}", compact_fields(&error.data))
                }
            ),
            None,
        ),
    }
}

/// Leaves unknown response fields visible instead of silently dropping new API data.
fn compact_fields(value: &Value) -> String {
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

/// Sanitizes text modes while JSON mode retains the exact serialized payload.
fn display_value(value: &Value) -> String {
    value
        .as_str()
        .map(clean)
        .unwrap_or_else(|| clean(&value.to_string()))
}
