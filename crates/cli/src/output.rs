use std::collections::BTreeMap;

use base64::{Engine, prelude::BASE64_STANDARD};
use devlanes_proto::{ActorKind, Event, LaneSummary, LogLine, Method, RpcError, UnitSummary};
use serde_json::{Value, json};

use crate::{
    ExportShell, OutputMode, Runtime, exit,
    render::{self, cap_line, clean, error_message},
};

/// A shell-safe command that can be pasted regardless of the caller's lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint(pub String);

impl Hint {
    /// Quotes each argument separately for the platform's shell ([`hint_quote`]);
    /// selectors remain opaque to the CLI.
    pub fn command(args: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self(
            std::iter::once("devlanes".into())
                .chain(args.into_iter().map(|v| hint_quote(v.as_ref())))
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

    /// The command the user ran (`args` without the program name) with `flag` added,
    /// for refusals that need confirmation: `devlanes lane clean 1 --yes`. The flag
    /// goes before a `--`, where it would otherwise reach the wrapped command.
    pub fn rerun_with(args: impl IntoIterator<Item = impl AsRef<str>>, flag: &str) -> Self {
        let mut args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
        if !args[..end].iter().any(|a| a == flag) {
            args.insert(end, flag.to_owned());
        }
        Self::command(args)
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
        let exit_code = if report.method == Method::Doctor {
            u8::from(crate::render::doctor_failed(report.data, self.runtime()))
        } else {
            result_exit_code(report.method, report.data)
        };
        if self.runtime().quiet && self.mode() != OutputMode::Json && exit_code == 0 {
            return Ok(Rendered {
                stderr: ownership_note(report, self.runtime()),
                ..Rendered::default()
            });
        }
        if self.mode() == OutputMode::Json {
            // Only the client knows its output mode and why it was chosen.
            if report.method == Method::Doctor && report.data.is_object() {
                let mut data = report.data.clone();
                data["output"] = self.runtime().doctor_output(&Value::Null)["output"].take();
                return Ok(Rendered {
                    stdout: format!("{}\n", serde_json::to_string(&data)?),
                    exit_code,
                    ..Rendered::default()
                });
            }
            return Ok(Rendered {
                stdout: format!("{}\n", serde_json::to_string(report.data)?),
                exit_code,
                ..Rendered::default()
            });
        }
        let agent = self.mode() == OutputMode::Agent;
        let cx = render::Ctx {
            mode: self.mode(),
            color: self.runtime().color,
            runtime: self.runtime(),
        };
        let mut lines = render::result(report.method, report.data, &cx);
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

    /// Progress is silent in agent mode; explicit log/event subscriptions and the
    /// output of a foreground `run` still stream.
    fn event(&self, method: Method, event: &Event) -> Result<Rendered, serde_json::Error> {
        if self.mode() == OutputMode::Json {
            return Ok(Rendered {
                stdout: format!("{}\n", serde_json::to_string(event)?),
                exit_code: if event.event == "result" {
                    result_exit_code(method, &event.data)
                } else {
                    exit::OK
                },
                ..Rendered::default()
            });
        }
        if event.event == "result" {
            return self.result(&Report::new(method, &event.data));
        }
        let task_output = method == Method::Run && event.event == devlanes_proto::events::LOG;
        if self.runtime().quiet
            || self.mode() == OutputMode::Agent
                && !matches!(method, Method::Logs | Method::Subscribe)
                && !task_output
        {
            return Ok(Rendered::default());
        }
        let stdout = if matches!(method, Method::Logs | Method::Run)
            && event.event == devlanes_proto::events::LOG
        {
            // Followed lines arrive as `{unit, line: LogLine}`; render the line itself.
            let line = event.data.get("line").unwrap_or(&Value::Null);
            self.result(&Report::new(Method::Logs, line))?.stdout
        } else {
            let cx = render::Ctx {
                mode: self.mode(),
                color: self.runtime().color,
                runtime: self.runtime(),
            };
            let lines = render::event(method, event, &cx);
            if lines.is_empty() {
                String::new()
            } else {
                format!("{}\n", lines.join("\n"))
            }
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

/// Log lines kept after the budget runs out, for a failure summary.
const HIDDEN_TAIL: usize = 20;

/// One command-wide budget for follow mode, rather than a fresh budget per event.
pub struct ReporterStream<'a> {
    reporter: &'a dyn Reporter,
    method: Method,
    remaining: usize,
    more: Option<Hint>,
    exhausted: bool,
    /// Last log lines the budget hid; a failed `run` shows them in its summary.
    hidden: std::collections::VecDeque<String>,
}

impl<'a> ReporterStream<'a> {
    /// The continuation should include the original selectors and query filters.
    pub fn new(reporter: &'a dyn Reporter, method: Method, more: Hint) -> Self {
        Self {
            more: Some(more),
            ..Self::without_hint(reporter, method)
        }
    }

    /// A stream whose continuation is not known yet (a foreground `run` learns from
    /// its result whether the task is a unit with logs): the notice says `truncated`.
    pub fn without_hint(reporter: &'a dyn Reporter, method: Method) -> Self {
        Self {
            remaining: reporter.runtime().budget,
            reporter,
            method,
            more: None,
            exhausted: false,
            hidden: std::collections::VecDeque::new(),
        }
    }

    /// Leaves `bytes` of the budget for a summary printed after the stream.
    #[must_use]
    pub fn reserve(mut self, bytes: usize) -> Self {
        self.remaining = self.remaining.saturating_sub(bytes);
        self
    }

    /// Emits at most one truncation notice; callers can detach once exhausted.
    pub fn event(&mut self, event: &Event) -> Result<Rendered, serde_json::Error> {
        if self.exhausted {
            self.keep_hidden(event);
            return Ok(Rendered::default());
        }
        let mut rendered = self.reporter.event(self.method, event)?;
        if self.reporter.mode() != OutputMode::Agent || rendered.stdout.is_empty() {
            return Ok(rendered);
        }
        let notice = match &self.more {
            Some(more) => format!("truncated; more: {}", more.0),
            None => "truncated".to_owned(),
        };
        let reserve = (notice.len() + 1).min(64.max(self.reporter.runtime().budget / 2));
        if rendered.stdout.len() + reserve > self.remaining {
            // Reserve the notice before consuming the final event, including exact-boundary cases.
            rendered.stdout = truncate_output(&rendered.stdout, self.remaining, &notice);
            self.exhausted = true;
            self.keep_hidden(event);
        }
        self.remaining = self.remaining.saturating_sub(rendered.stdout.len());
        Ok(rendered)
    }

    /// Allows the transport to stop reading a bounded agent stream without stopping units.
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// The last log lines the budget hid, oldest first (at most 20).
    pub fn hidden_tail(&self) -> Vec<String> {
        self.hidden.iter().cloned().collect()
    }

    /// Remembers a log line that was not printed.
    fn keep_hidden(&mut self, event: &Event) {
        if event.event != devlanes_proto::events::LOG {
            return;
        }
        if let Some(text) = event.data.pointer("/line/text").and_then(Value::as_str) {
            if self.hidden.len() == HIDDEN_TAIL {
                self.hidden.pop_front();
            }
            self.hidden.push_back(text.to_owned());
        }
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
            line.push_str(&render::exit_field(code, agent));
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

/// Shell assignments for `env export`, plus the names the shell cannot take
/// (config allows names like `NEXT-PUBLIC` that POSIX `export` rejects) or values
/// with NUL.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Exported {
    pub script: String,
    pub skipped: Vec<String>,
}

/// Produces shell assignments without evaluating values received from the daemon;
/// variables the shell cannot hold are skipped, so one name does not cost the rest.
pub fn export_environment(values: &BTreeMap<String, String>, shell: ExportShell) -> Exported {
    let mut exported = Exported::default();
    for (key, value) in values {
        let mut chars = key.chars();
        let simple = chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
        let line = match shell {
            _ if value.contains('\0') => None,
            ExportShell::Posix => simple.then(|| format!("export {key}={}\n", shell_quote(value))),
            ExportShell::Pwsh => pwsh_assignment(key, value, simple),
        };
        match line {
            Some(line) => exported.script.push_str(&line),
            None => exported.skipped.push(key.clone()),
        }
    }
    exported
}

/// One PowerShell assignment, in printable ASCII only. PowerShell decodes a
/// native command's output with `[Console]::OutputEncoding`, often a legacy code
/// page, so other bytes can change meaning: under code page 1252 the UTF-8 bytes
/// 0x91 and 0x92 become typographic quotes, which end a single-quoted string.
/// Values are always quoted, because an unquoted word after `=` would run as a
/// command. `None` for names no environment can hold (empty, with `=` or control
/// characters).
fn pwsh_assignment(key: &str, value: &str, simple: bool) -> Option<String> {
    let value = pwsh_ascii_string(value);
    if simple {
        return Some(format!("$env:{key} = {value}\n"));
    }
    if key.is_empty() || key.contains('=') || key.chars().any(char::is_control) {
        return None;
    }
    // Names such as Windows' `ProgramFiles(x86)` fit the braced form. Braces,
    // backticks, drive separators and provider wildcards mean something inside
    // `${env:...}`, and non-ASCII would leave the script ASCII-only, so those
    // names go through .NET, which takes the name as a plain string.
    let braced = key.is_ascii() && !key.contains(['{', '}', '`', ':', '*', '?', '[', ']']);
    Some(if braced {
        format!("${{env:{key}}} = {value}\n")
    } else {
        format!(
            "[Environment]::SetEnvironmentVariable({}, {value}, 'Process')\n",
            pwsh_ascii_string(key)
        )
    })
}

/// A PowerShell string expression for `text` made of printable ASCII: a
/// single-quoted literal when `text` is printable ASCII, otherwise its UTF-8
/// bytes decoded from Base64, which Windows PowerShell 5.1 and PowerShell 7
/// both evaluate.
fn pwsh_ascii_string(text: &str) -> String {
    if text.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        pwsh_literal(text)
    } else {
        format!(
            "[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))",
            BASE64_STANDARD.encode(text)
        )
    }
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

/// Quotes a PowerShell argument when it needs it: single quotes, with every
/// single-quote character doubled. Besides POSIX metacharacters, PowerShell
/// treats a leading `@` (splatting), `,` (arrays), `%` (`--%` stops parsing) and
/// dotted words after `-` (split by Windows PowerShell 5.1) specially, so those
/// are quoted.
///
/// ```
/// assert_eq!(devlanes_cli::pwsh_quote("acme@2:api"), "acme@2:api");
/// assert_eq!(devlanes_cli::pwsh_quote("a,b"), "'a,b'");
/// assert_eq!(devlanes_cli::pwsh_quote("it's $x"), "'it''s $x'");
/// ```
pub fn pwsh_quote(value: &str) -> String {
    let plain = value
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"_@+=:./-".contains(&c));
    let special_start = value.starts_with('@')
        || value.starts_with('-') && value.contains(['.', ':', '=', '/', '@']);
    if !value.is_empty() && plain && !special_start {
        value.into()
    } else {
        pwsh_literal(value)
    }
}

/// A PowerShell single-quoted string holding `value` verbatim. PowerShell also
/// ends single-quoted strings at the typographic single quotes, so those are
/// doubled as well.
fn pwsh_literal(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// Quotes one word of a command shown for the user to copy (hints, `more:`
/// lines): POSIX shell syntax.
#[cfg(unix)]
pub fn hint_quote(value: &str) -> String {
    shell_quote(value)
}

/// Quotes one word of a command shown for the user to copy (hints, `more:`
/// lines): PowerShell syntax, the default shell in Windows Terminal. Words that
/// need no quoting paste into any shell.
#[cfg(windows)]
pub fn hint_quote(value: &str) -> String {
    pwsh_quote(value)
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

/// The process status for a result of `method`.
///
/// - start, restart, `lane new --start` and resume: 124 when the start timed out,
///   1 on any failure or when not ready, else 0;
/// - wait: 0 only when ready;
/// - stop, status, lane ls: 1 only for a failed unit or lane, never for a leftover
///   nonzero exit code of a unit that stopped normally;
/// - run: the task's exit code.
pub(crate) fn result_exit_code(method: Method, data: &Value) -> u8 {
    match method {
        Method::Start | Method::Restart => start_exit_code(data),
        Method::LaneNew => data.get("start").map_or(exit::OK, start_exit_code),
        Method::Resume => data
            .get("items")
            .and_then(Value::as_array)
            .map_or(exit::OK, |items| {
                items
                    .iter()
                    .map(|item| {
                        if item.get("error").is_some() {
                            exit::FAILED
                        } else {
                            start_exit_code(item)
                        }
                    })
                    .max()
                    .unwrap_or(exit::OK)
            }),
        Method::Wait => {
            if data.get("ready").and_then(Value::as_bool) == Some(true) {
                exit::OK
            } else {
                exit::FAILED
            }
        }
        Method::Stop | Method::Status | Method::LaneList => {
            if failed_state(data) {
                exit::FAILED
            } else {
                exit::OK
            }
        }
        _ => generic_exit_code(data),
    }
}

/// A [`devlanes_proto::StartResult`]'s status; other shapes (`with`'s failure pages)
/// fall back to the generic rule.
fn start_exit_code(data: &Value) -> u8 {
    if data.get("ready").is_none() {
        return generic_exit_code(data);
    }
    if data.get("timed_out").and_then(Value::as_bool) == Some(true) {
        return exit::TIMEOUT;
    }
    let failures = data
        .get("failures")
        .and_then(Value::as_array)
        .is_some_and(|failures| !failures.is_empty());
    if failures || failed_state(data) || data.get("ready").and_then(Value::as_bool) != Some(true) {
        return exit::FAILED;
    }
    exit::OK
}

/// Whether `data` (a unit, lane, page or list) holds a unit or lane in state `failed`.
fn failed_state(data: &Value) -> bool {
    if let Some(items) = data.as_array() {
        return items.iter().any(failed_state);
    }
    if data.get("state").and_then(Value::as_str) == Some("failed") {
        return true;
    }
    ["items", "units"].into_iter().any(|key| {
        data.get(key)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(failed_state))
    })
}

/// Results without a dedicated rule: a nonzero `exit_code` or a failed state.
fn generic_exit_code(data: &Value) -> u8 {
    if let Some(items) = data.as_array()
        && items.iter().any(|v| generic_exit_code(v) != 0)
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
            && items.iter().any(|v| generic_exit_code(v) != 0)
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

/// Tells an agent caller that it stopped or restarted units a human started, without
/// refusing the operation. The output mode does not matter: a human who forced
/// `--output agent` stopping their own units gets no note.
fn ownership_note(report: &Report<'_>, runtime: &Runtime) -> String {
    if runtime.actor.kind != ActorKind::Agent
        || !matches!(report.method, Method::Stop | Method::Restart)
    {
        return String::new();
    }
    // `stop` returns a page of units, `restart` a start result with `units`.
    let items = report
        .data
        .get("items")
        .or_else(|| report.data.get("units"))
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
