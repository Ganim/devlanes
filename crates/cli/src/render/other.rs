//! Logs, tasks, env, metrics, events, infra, doctor, config and daemon results.

use std::collections::BTreeMap;

use devlanes_proto::{Event, LogLine, Method, RpcError, events};
use serde_json::Value;

use super::{
    Ctx, Outcome, bytes, cap_line, clean, compact_fields, display_value, duration, int,
    leftover_notes_of, list, percent, plural, table, tail_lines, text, tree, words,
};
use crate::Hint;

/// One log line as `unit#seq text`; the cursor stays copyable.
fn log_line(line: &LogLine, cx: &Ctx<'_>) -> String {
    let text = clean(&line.text);
    let text = if cx.agent() {
        cap_line(&text, 300)
    } else {
        text
    };
    format!("{}#{} {}", clean(&line.unit), line.seq, text)
}

/// Agent log shaping: collapses adjacent repeats and `node_modules` stack frames.
pub(crate) fn agent_logs(items: &[Value]) -> Vec<String> {
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

/// Lines of a page in the mode's shape.
fn page_lines(items: &[Value], cx: &Ctx<'_>) -> Vec<String> {
    if cx.agent() {
        return agent_logs(items);
    }
    items
        .iter()
        .map(
            |value| match serde_json::from_value::<LogLine>(value.clone()) {
                Ok(line) => log_line(&line, cx),
                Err(_) => compact_fields(value),
            },
        )
        .collect()
}

/// `logs`: one page (one unit or merged by time), one line, or `--all` grouped by unit.
pub(super) fn logs(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    if let Some(groups) = data.get("units").and_then(Value::as_array) {
        let lane = text(data, "lane");
        let mut lines = Vec::new();
        for group in groups {
            let unit = text(group, "unit");
            let items = list(group, "lines");
            let truncated = group.get("truncated").and_then(Value::as_bool) == Some(true);
            let note = if truncated { ", truncated" } else { "" };
            let count = plural(items.len(), "line");
            if cx.agent() {
                lines.push(format!("{unit}: {count}{note}"));
            } else {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(format!("== {unit} ({count}{note}) =="));
            }
            lines.extend(page_lines(items, cx));
            if truncated {
                lines.push(format!(
                    "  more: {}",
                    Hint::command(["logs", &format!("{lane}:{unit}"), "--tail", "500"]).0
                ));
            }
        }
        if lines.is_empty() {
            lines.push("no units in this lane".into());
        }
        return lines;
    }
    if let Some(items) = data
        .as_array()
        .or_else(|| data.get("items").and_then(Value::as_array))
    {
        if items.is_empty() {
            return vec!["no log lines".into()];
        }
        return page_lines(items, cx);
    }
    match serde_json::from_value::<LogLine>(data.clone()) {
        Ok(line) => vec![log_line(&line, cx)],
        Err(_) => tree(data),
    }
}

/// `run`: the task's exit, or where a detached or interactive task can be found.
pub(super) fn run(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let lane = text(data, "lane");
    let task = text(data, "task");
    let unit = data
        .get("unit")
        .and_then(Value::as_str)
        .map(clean)
        .unwrap_or_else(|| task.clone());
    let target = format!("{lane}:{unit}");
    if data.get("exit_code").is_some() || data.get("state").is_some() {
        let failed = text(data, "state") == "failed";
        let mut line = format!(
            "{} {lane}:{task}",
            cx.state(if failed { "failed" } else { "exited" })
        );
        if let Some(code) = int(data, "exit_code") {
            line.push_str(&format!(" exit={code}"));
        }
        let mut lines = vec![line];
        for leftovers in list(data, "leftovers") {
            // Steps run as their own units; name them, but not the task itself.
            let prefix = match text(leftovers, "unit") {
                name if name == task => "  ".to_owned(),
                name => format!("  {name}: "),
            };
            lines.extend(leftover_notes_of(&prefix, leftovers));
        }
        if failed {
            // Set by the client when the output budget hid the end of the task's output.
            lines.extend(tail_lines(list(data, "log_tail")));
            lines.push(format!("  more: {}", Hint::logs(&lane, &unit, 200).0));
        }
        return lines;
    }
    let interactive = data.get("interactive").and_then(Value::as_bool) == Some(true);
    let mut lines = vec![format!(
        "started {lane}:{task} ({})",
        if interactive {
            "interactive"
        } else {
            "detached"
        }
    )];
    lines.push(if interactive {
        format!("  attach: {}", Hint::command(["attach", target.as_str()]).0)
    } else {
        format!(
            "  logs: {}",
            Hint::command(["logs", target.as_str(), "-f"]).0
        )
    });
    lines
}

/// Comma-joined strings of an array field, `-` when empty.
fn joined(value: &Value, key: &str) -> String {
    let items: Vec<String> = list(value, key).iter().map(display_value).collect();
    if items.is_empty() {
        "-".into()
    } else {
        items.join(",")
    }
}

/// What a task runs: its script, its steps, or its own command.
fn task_runs(task: &Value) -> String {
    let mut runs = if let Some(script) = task.get("run_script").filter(|s| s.is_object()) {
        format!("script {}#{}", text(script, "source"), text(script, "name"))
    } else if !list(task, "steps").is_empty() {
        format!("tasks {} in order", joined(task, "steps"))
    } else {
        "its `run` command".into()
    };
    for (flag, note) in [
        ("confirm", "asks first"),
        ("interactive", "needs a terminal"),
    ] {
        if task.get(flag).and_then(Value::as_bool) == Some(true) {
            runs.push_str(&format!(" ({note})"));
        }
    }
    runs
}

/// `run --list`: tasks with their hooks, plus the script sources they use.
pub(super) fn tasks(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let items = list(data, "items");
    let mut lines = Vec::new();
    if items.is_empty() {
        lines.push("no tasks in this project's config".into());
    } else if cx.agent() {
        for task in items {
            let mut line = format!("task {}", text(task, "name"));
            for key in ["when", "after"] {
                let value = text(task, key);
                if !value.is_empty() {
                    line.push_str(&format!(" {key}={value}"));
                }
            }
            if !list(task, "profiles").is_empty() {
                line.push_str(&format!(" profiles={}", joined(task, "profiles")));
            }
            if let Some(script) = task.get("run_script").filter(|s| s.is_object()) {
                line.push_str(&format!(
                    " script={}#{}",
                    text(script, "source"),
                    text(script, "name")
                ));
            }
            if !list(task, "steps").is_empty() {
                line.push_str(&format!(" steps={}", joined(task, "steps")));
            }
            for flag in ["confirm", "interactive"] {
                if task.get(flag).and_then(Value::as_bool) == Some(true) {
                    line.push_str(&format!(" {flag}"));
                }
            }
            lines.push(line);
        }
    } else {
        let mut rows = vec![
            ["TASK", "WHEN", "AFTER", "PROFILES", "RUNS"]
                .map(str::to_owned)
                .to_vec(),
        ];
        for task in items {
            let or_dash = |key: &str| {
                let value = text(task, key);
                if value.is_empty() { "-".into() } else { value }
            };
            rows.push(vec![
                text(task, "name"),
                or_dash("when"),
                or_dash("after"),
                joined(task, "profiles"),
                task_runs(task),
            ]);
        }
        lines.extend(table(&rows, ""));
    }
    let sources: Vec<String> = list(data, "sources")
        .iter()
        .map(|source| {
            let runner = source
                .get("runner")
                .and_then(Value::as_str)
                .map_or("no runner".into(), clean);
            format!("{} ({runner})", text(source, "source"))
        })
        .collect();
    if !sources.is_empty() {
        lines.push(format!("sources: {}", sources.join(", ")));
    }
    lines
}

/// Escapes line breaks so one variable stays on one line.
fn one_line(value: &str) -> String {
    clean(&value.replace('\r', "\\r").replace('\n', "\\n"))
}

/// `env`: `KEY=VALUE` lines, or with `--explain` a `KEY VALUE LAYER` table where
/// redacted secret values show as `<secret>`.
pub(super) fn env(data: &Value) -> Vec<String> {
    if let Some(items) = data.get("items").and_then(Value::as_array) {
        let mut rows = vec![["KEY", "VALUE", "LAYER"].map(str::to_owned).to_vec()];
        for item in items {
            let value = if item.get("redacted").and_then(Value::as_bool) == Some(true) {
                "<secret>".into()
            } else {
                item.get("value")
                    .and_then(Value::as_str)
                    .map(one_line)
                    .unwrap_or_default()
            };
            // The value sits between key and layer, so long values stay readable.
            rows.push(vec![
                text(item, "key"),
                cap_line(&value, 120),
                text(item, "layer"),
            ]);
        }
        return table(&rows, "");
    }
    let Some(env) = data.get("env").and_then(Value::as_object) else {
        return tree(data);
    };
    let sorted: BTreeMap<&String, &Value> = env.iter().collect();
    sorted
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                clean(key),
                value
                    .as_str()
                    .map(one_line)
                    .unwrap_or_else(|| display_value(value))
            )
        })
        .collect()
}

/// `url`: only the URL, for scripts.
pub(super) fn url(data: &Value) -> Vec<String> {
    let found = data
        .as_str()
        .or_else(|| data.get("url").and_then(Value::as_str))
        .or_else(|| list(data, "urls").first().and_then(Value::as_str));
    found.map(clean).into_iter().collect()
}

/// `top`: CPU and memory per lane and unit, or the run's peaks.
pub(super) fn metrics(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let cpu = |value: Option<f64>| value.map_or("–".into(), percent);
    let memory = |value: Option<u64>| value.map_or("–".into(), bytes);
    if let Some(peaks) = data.get("peaks").and_then(Value::as_array) {
        let lane = text(data, "lane");
        if cx.agent() {
            return peaks
                .iter()
                .map(|peak| {
                    format!(
                        "{lane}:{} peak_cpu={} peak_mem={}",
                        text(peak, "unit"),
                        cpu(peak.get("peak_cpu_percent").and_then(Value::as_f64)),
                        memory(peak.get("peak_memory_bytes").and_then(Value::as_u64))
                    )
                })
                .collect();
        }
        let mut rows = vec![["UNIT", "PEAK CPU", "PEAK MEM"].map(str::to_owned).to_vec()];
        for peak in peaks {
            rows.push(vec![
                text(peak, "unit"),
                cpu(peak.get("peak_cpu_percent").and_then(Value::as_f64)),
                memory(peak.get("peak_memory_bytes").and_then(Value::as_u64)),
            ]);
        }
        let mut lines = vec![lane];
        lines.extend(table(&rows, "  "));
        return lines;
    }
    let units = list(data, "units");
    if units.is_empty() {
        return vec!["no running units".into()];
    }
    let mut lanes: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for unit in units {
        lanes.entry(text(unit, "lane")).or_default().push(unit);
    }
    let mut lines = Vec::new();
    for (lane, units) in lanes {
        if cx.agent() {
            for unit in units {
                lines.push(format!(
                    "{lane}:{} cpu={} mem={}",
                    text(unit, "unit"),
                    cpu(unit.get("cpu_percent").and_then(Value::as_f64)),
                    memory(unit.get("memory_bytes").and_then(Value::as_u64))
                ));
            }
            continue;
        }
        let cpus: Vec<f64> = units
            .iter()
            .filter_map(|u| u.get("cpu_percent").and_then(Value::as_f64))
            .collect();
        let memories: Vec<u64> = units
            .iter()
            .filter_map(|u| u.get("memory_bytes").and_then(Value::as_u64))
            .collect();
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!(
            "{lane}  cpu {}  mem {}",
            cpu((!cpus.is_empty()).then(|| cpus.iter().sum())),
            memory((!memories.is_empty()).then(|| memories.iter().sum()))
        ));
        let mut rows = vec![["UNIT", "CPU", "MEM"].map(str::to_owned).to_vec()];
        for unit in units {
            rows.push(vec![
                text(unit, "unit"),
                cpu(unit.get("cpu_percent").and_then(Value::as_f64)),
                memory(unit.get("memory_bytes").and_then(Value::as_u64)),
            ]);
        }
        lines.extend(table(&rows, "  "));
    }
    lines
}

/// One event as `name subject state`, for `events` and streams without progress lines.
pub(crate) fn event_line(event: &Event) -> String {
    let data = &event.data;
    let name = clean(&event.event);
    match event.event.as_str() {
        events::LANE_CHANGED => {
            let lane = &data["lane"];
            format!("{name} {} {}", text(lane, "reference"), text(lane, "state"))
        }
        events::UNIT_CHANGED => {
            let unit = &data["unit"];
            format!(
                "{name} {}:{} {}",
                text(unit, "lane"),
                text(unit, "name"),
                text(unit, "state")
            )
        }
        events::PHASE => format!(
            "{name} {} {} {}",
            text(data, "lane"),
            text(data, "phase"),
            text(data, "state")
        ),
        events::TASK => {
            let mut line = format!(
                "{name} {} {} {}",
                text(data, "lane"),
                text(data, "task"),
                text(data, "state")
            );
            if let Some(code) = int(data, "exit_code") {
                line.push_str(&format!(" exit={code}"));
            }
            line
        }
        events::LOG => {
            let line = &data["line"];
            format!(
                "{name} {}#{} {}",
                text(line, "unit"),
                display_value(&line["seq"]),
                cap_line(&text(line, "text"), 300)
            )
        }
        events::LANE_REMOVED => format!("{name} {}", text(data, "lane")),
        _ => format!("{name} {}", compact_fields(data))
            .trim_end()
            .to_owned(),
    }
}

/// `events` without `-f`: the recent events.
pub(super) fn events_page(data: &Value) -> Vec<String> {
    let items = list(data, "items");
    if items.is_empty() {
        return vec!["no recent events".into()];
    }
    items
        .iter()
        .filter_map(|item| serde_json::from_value::<Event>(item.clone()).ok())
        .map(|event| event_line(&event))
        .collect()
}

/// A `{code, data}` reason as words.
fn reason_text(reason: &Value) -> Option<String> {
    let code = reason.get("code").and_then(Value::as_str)?;
    let error = RpcError::new(code, reason.get("data").cloned().unwrap_or(Value::Null), 1);
    Some(words::error_message(&error).0)
}

/// A finished start phase: `✓ infra`, `✗ setup: reason`, `! seed skipped: reason`.
/// Phases skipped because the project has nothing for them print nothing, and
/// so do finished task and services phases: their task lines and the result's
/// services line already say it.
pub(super) fn phase_line(data: &Value, cx: &Ctx<'_>) -> Option<String> {
    let phase = text(data, "phase");
    let reason = data.get("reason").and_then(reason_text);
    match text(data, "state").as_str() {
        // Human output only: the binary replaces the line when the phase ends
        // (or erases it when the phase had nothing to do).
        "started" if cx.human() => {
            let units: Vec<String> = list(data, "units").iter().map(display_value).collect();
            let what = match phase.as_str() {
                "infra" => ": starting containers, waiting until they are healthy".to_owned(),
                "setup" => ": running infra.setup".to_owned(),
                "services" if !units.is_empty() => format!(": starting {}", units.join(", ")),
                _ => String::new(),
            };
            Some(cx.paint(&format!("… {phase}{what}"), "2"))
        }
        // What the phase waits on now; replaces the line above the same way.
        "progress" if cx.human() => devlanes_term::phase_detail(data)
            .map(|detail| cx.paint(&format!("… {phase}: {}", clean(&detail)), "2")),
        "done" if matches!(phase.as_str(), "tasks" | "services" | "after-services") => None,
        "done" => Some(format!("{} {phase}", cx.mark(Outcome::Ok))),
        "failed" => Some(format!(
            "{} {phase}{}",
            cx.mark(Outcome::Fail),
            reason.map(|r| format!(": {r}")).unwrap_or_default()
        )),
        "skipped" => {
            let code = data
                .get("reason")
                .and_then(|r| r.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Phases with nothing to do, and the lane-level phases a start into a
            // running lane never repeats, are not news.
            (!code.starts_with("no_") && code != "lane_running")
                .then(|| cx.skipped(&phase, reason.as_deref()))
        }
        _ => None,
    }
}

/// A finished task during a start: `✓ migrate` or `✗ migrate exit=1`.
pub(super) fn task_line(data: &Value, cx: &Ctx<'_>) -> Option<String> {
    let task = text(data, "task");
    match text(data, "state").as_str() {
        "done" | "exited" => Some(format!("{} {task}", cx.mark(Outcome::Ok))),
        "failed" => Some(format!(
            "{} {task}{}",
            cx.mark(Outcome::Fail),
            int(data, "exit_code")
                .map(|code| format!(" exit={code}"))
                .unwrap_or_default()
        )),
        "skipped" => Some(cx.skipped(&task, data.get("reason").and_then(reason_text).as_deref())),
        _ => None,
    }
}

/// `infra up`, `infra status` and `infra down`.
pub(super) fn infra(method: Method, data: &Value) -> Vec<String> {
    let lane = text(data, "lane");
    match method {
        Method::InfraUp => {
            let mut lines = vec![format!("infra up for {lane}")];
            if let Some(endpoints) = data.get("endpoints").and_then(Value::as_object) {
                let mut rows = vec![["SERVICE", "PORT", "PORTS"].map(str::to_owned).to_vec()];
                for (name, endpoint) in endpoints {
                    let ports: Vec<String> = endpoint
                        .get("ports")
                        .and_then(Value::as_object)
                        .map(|ports| {
                            ports
                                .iter()
                                .map(|(inner, host)| {
                                    format!("{}->{}", clean(inner), display_value(host))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    rows.push(vec![
                        clean(name),
                        endpoint.get("port").map_or("-".into(), display_value),
                        ports.join(", "),
                    ]);
                }
                if rows.len() > 1 {
                    lines.extend(table(&rows, "  "));
                }
            }
            lines
        }
        Method::InfraStatus => {
            let mut lines = vec![format!("infra for {lane}")];
            let report = tree(&data["infra"]);
            if report.is_empty() {
                lines.push("  no infra declared".into());
            }
            lines.extend(report.into_iter().map(|line| format!("  {line}")));
            lines
        }
        _ => {
            let mut lines = vec!["infra down; container data is kept".to_owned()];
            lines.extend(tree(data).into_iter().map(|line| format!("  {line}")));
            lines
        }
    }
}

/// One doctor check.
struct Check {
    group: &'static str,
    outcome: Outcome,
    text: String,
    fix: Option<String>,
}

impl Check {
    /// A check with no fix.
    fn new(group: &'static str, outcome: Outcome, text: impl Into<String>) -> Self {
        Self {
            group,
            outcome,
            text: text.into(),
            fix: None,
        }
    }

    /// Adds the fix line.
    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

/// Why a bound listener kind is not serving, from its `listeners`: each stopped
/// accept loop and each pending accept error with its count.
fn not_serving(entry: &Value) -> String {
    let problems: Vec<String> = list(entry, "listeners")
        .iter()
        .filter_map(|listener| {
            let address = text(listener, "address");
            let errors = int(listener, "accept_errors").unwrap_or(0);
            match listener.get("last_error").and_then(Value::as_str) {
                Some(error) => Some(format!(
                    "{address}: {} ({})",
                    clean(error),
                    plural(usize::try_from(errors).unwrap_or(0), "accept error")
                )),
                None if listener.get("running").and_then(Value::as_bool) == Some(false) => {
                    Some(format!("{address} stopped accepting connections"))
                }
                None => None,
            }
        })
        .collect();
    if problems.is_empty() {
        "no listener is running".into()
    } else {
        problems.join("; ")
    }
}

/// Listener checks for `http` or `https`.
fn listener(proxy: &Value, name: &'static str) -> Option<Check> {
    let entry = proxy.get(name)?;
    if entry.get("bound").and_then(Value::as_bool) == Some(true) {
        let port = display_value(&entry["port"]);
        // Bound once is not enough: an accept loop can stop or keep failing.
        if entry.get("serving").and_then(Value::as_bool) == Some(false) {
            return Some(
                Check::new(
                    "proxy",
                    Outcome::Fail,
                    format!(
                        "{name} on port {port} is not serving: {}",
                        not_serving(entry)
                    ),
                )
                .fix("devlanes daemon restart; if it persists, devlanes daemon logs"),
            );
        }
        return Some(match entry.get("fallback_reason").and_then(Value::as_str) {
            Some(reason) => Check::new(
                "proxy",
                Outcome::Skip,
                format!("{name} on fallback port {port}: {}", clean(reason)),
            )
            .fix("free the standard port or set proxy ports in ~/.devlanes/config.yaml"),
            None => Check::new("proxy", Outcome::Ok, format!("{name} on port {port}")),
        });
    }
    if entry.get("reason").and_then(Value::as_str) == Some("no_ca") {
        return Some(
            Check::new("proxy", Outcome::Skip, format!("{name} off: no CA"))
                .fix("devlanes certs trust"),
        );
    }
    Some(
        Check::new(
            "proxy",
            Outcome::Fail,
            format!("{name} not bound: {}", text(entry, "error")),
        )
        .fix("stop the process holding the port, then devlanes daemon restart"),
    )
}

/// Every doctor check from the daemon facts and this invocation's output policy.
fn doctor_checks(data: &Value, cx: &Ctx<'_>) -> Vec<Check> {
    // `running: false` comes from the CLI's own checks when no daemon answered.
    let running = data.get("running").and_then(Value::as_bool) != Some(false);
    let mut checks = vec![if running {
        Check::new(
            "daemon",
            Outcome::Ok,
            format!(
                "devlanes {}, pid {}, socket {}",
                text(data, "version"),
                display_value(&data["pid"]),
                display_value(&data["socket"])
            ),
        )
    } else {
        Check::new(
            "daemon",
            Outcome::Skip,
            format!("not running (socket {})", display_value(&data["socket"])),
        )
        .fix("devlanes start, or devlanes daemon restart")
    }];
    checks.extend(home_checks(&data["home_check"]));
    match (
        int(data, "schema_version"),
        int(data, "supported_schema_version"),
    ) {
        // Only a running daemon reads the database.
        _ if !running => {}
        (Some(found), Some(supported)) if found > supported => checks.push(
            Check::new(
                "daemon",
                Outcome::Fail,
                format!("state schema {found} is newer than this devlanes ({supported})"),
            )
            .fix("upgrade devlanes"),
        ),
        (Some(found), _) => checks.push(Check::new(
            "daemon",
            Outcome::Ok,
            format!(
                "state schema {found} at {}",
                display_value(&data["db_path"])
            ),
        )),
        (None, _) => checks.push(
            Check::new(
                "daemon",
                Outcome::Fail,
                "state database schema is unreadable",
            )
            .fix("devlanes daemon reset-state --yes"),
        ),
    }
    let sandbox = &data["sandbox"];
    let sandboxed = ["codex_sandbox", "app_sandbox"]
        .into_iter()
        .filter(|key| sandbox.get(*key).and_then(Value::as_bool) == Some(true))
        .collect::<Vec<_>>();
    checks.push(if sandboxed.is_empty() {
        Check::new("sandbox", Outcome::Ok, "not sandboxed")
    } else {
        Check::new(
            "sandbox",
            Outcome::Skip,
            format!(
                "{} inside a sandbox ({})",
                if running {
                    "daemon runs"
                } else {
                    "this shell runs"
                },
                sandboxed.join(", ")
            ),
        )
        .fix("devlanes daemon install, so the daemon starts outside the sandbox")
    });
    checks.extend(terminal_checks(&data["terminal"]));
    // Docker is the daemon's to ask; without one there is nothing to say.
    if data.get("infra").is_some() {
        checks.extend(docker_checks(data));
    }
    let proxy = &data["proxy"];
    checks.extend(
        ["http", "https"]
            .into_iter()
            .filter_map(|name| listener(proxy, name)),
    );
    // Without a daemon: what holds the ports its proxy will want.
    for holder in list(data, "port_holders") {
        let command = holder
            .get("command")
            .and_then(Value::as_str)
            .map_or_else(|| "another process".to_owned(), clean);
        checks.push(
            Check::new(
                "proxy",
                Outcome::Skip,
                format!(
                    "port {} is held by {command} (pid {}); the {} proxy will fall back to another port",
                    display_value(&holder["port"]),
                    display_value(&holder["pid"]),
                    text(holder, "listener")
                ),
            )
            .fix("stop that process, or set proxy ports in ~/.devlanes/config.yaml"),
        );
    }
    checks.extend(tls_and_hosts_checks(data, running, cx));
    checks
}

/// Whether any doctor check failed; doctor then exits 1. Warnings (a stopped
/// daemon included) do not count.
pub(crate) fn doctor_failed(data: &Value, runtime: &crate::Runtime) -> bool {
    let cx = Ctx {
        mode: runtime.mode,
        color: false,
        runtime,
    };
    doctor_checks(data, &cx)
        .iter()
        .any(|check| check.outcome == Outcome::Fail)
}

/// `home_check` (`{path, exists, mode, problem}`) from the CLI's own checks.
fn home_checks(home: &Value) -> Vec<Check> {
    if !home.is_object() {
        return Vec::new();
    }
    let path = text(home, "path");
    let check = match home.get("problem").and_then(Value::as_str) {
        _ if home.get("exists").and_then(Value::as_bool) == Some(false) => Check::new(
            "home",
            Outcome::Ok,
            format!("{path} does not exist yet; the first command creates it"),
        ),
        None => Check::new("home", Outcome::Ok, format!("{path} is private")),
        Some("mode") => Check::new(
            "home",
            Outcome::Skip,
            format!(
                "{path} has mode {}; the daemon makes it 0700 when it starts",
                text(home, "mode")
            ),
        )
        .fix(format!("chmod 700 {}", crate::shell_quote(&path))),
        Some("acl") => Check::new(
            "home",
            Outcome::Skip,
            format!(
                "{path} lets other accounts in; the daemon limits it to you, SYSTEM and Administrators when it starts"
            ),
        ),
        Some(problem) => Check::new(
            "home",
            Outcome::Fail,
            format!(
                "devlanes refuses {path}: {}",
                match problem {
                    "owner" => "another user owns it",
                    "symlink" => "it is a symlink",
                    "not_a_directory" => "it is not a directory",
                    _ => "it cannot be read",
                }
            ),
        )
        .fix("point DEVLANES_HOME at a directory you own, or fix that path by hand"),
    };
    vec![check]
}

/// The shell running `doctor` (`terminal: {elevated, job}`, reported on Windows):
/// an elevated shell cannot start the daemon, and a job that kills its processes
/// on close, with no way out, stops a daemon started from it.
fn terminal_checks(terminal: &Value) -> Vec<Check> {
    let mut checks = Vec::new();
    if terminal.get("elevated").and_then(Value::as_bool) == Some(true) {
        checks.push(
            Check::new(
                "terminal",
                Outcome::Fail,
                "this terminal runs as administrator; the daemon refuses to run elevated",
            )
            .fix("run devlanes from a terminal that is not running as administrator"),
        );
    }
    let job = &terminal["job"];
    let flag = |key: &str| job.get(key).and_then(Value::as_bool) == Some(true);
    if flag("kill_on_close") && !flag("breakaway_ok") {
        checks.push(
            Check::new(
                "terminal",
                Outcome::Skip,
                "this terminal stops every process it started when it closes, including a daemon started from it",
            )
            .fix("start the daemon from another terminal, such as Windows Terminal or PowerShell"),
        );
    }
    checks
}

/// Docker and Compose from the daemon's `infra` facts.
fn docker_checks(data: &Value) -> Vec<Check> {
    let mut checks = Vec::new();
    let docker = &data["infra"]["docker"];
    let reachable = docker.get("reachable").and_then(Value::as_bool) == Some(true);
    checks.push(if reachable {
        Check::new(
            "docker",
            Outcome::Ok,
            format!("Docker {}", display_value(&docker["version"])),
        )
    } else {
        Check::new("docker", Outcome::Skip, "Docker is not reachable")
            .fix("start Docker; only projects with infra need it")
    });
    let compose = &data["infra"]["compose"];
    if compose.get("version").is_some() {
        checks.push(
            if compose
                .get("supports_lane_overrides")
                .and_then(Value::as_bool)
                == Some(false)
            {
                Check::new(
                    "docker",
                    Outcome::Skip,
                    format!(
                        "Compose {} cannot apply per-lane overrides",
                        text(compose, "version")
                    ),
                )
                .fix("upgrade Docker Compose")
            } else {
                Check::new(
                    "docker",
                    Outcome::Ok,
                    format!("Compose {}", text(compose, "version")),
                )
            },
        );
    } else if compose.get("error").is_some() && reachable {
        // Without Docker, Compose fails for the same reason; one check says it.
        checks.push(
            Check::new(
                "docker",
                Outcome::Skip,
                format!("Compose unavailable ({})", display_value(&compose["error"])),
            )
            .fix("install the Docker Compose plugin"),
        );
    }
    checks
}

/// CA, trust, route warnings, hosts file, user config, daemon counts, release and
/// output mode.
fn tls_and_hosts_checks(data: &Value, running: bool, cx: &Ctx<'_>) -> Vec<Check> {
    let mut checks = Vec::new();
    let ca = &data["ca"];
    if ca.get("pending").and_then(Value::as_bool) == Some(true) {
        checks.push(Check::new(
            "tls",
            Outcome::Ok,
            "no CA yet; the daemon creates one when it starts",
        ));
    } else if ca.get("present").and_then(Value::as_bool) == Some(true) {
        checks.push(Check::new(
            "tls",
            Outcome::Ok,
            format!("CA at {}", display_value(&ca["path"])),
        ));
        // `[{store, state, reason?}]`, added by the CLI from this user's stores.
        for entry in list(ca, "trust") {
            let store = text(entry, "store");
            checks.push(match text(entry, "state").as_str() {
                "trusted" => Check::new("tls", Outcome::Ok, format!("CA trusted by {store}")),
                "untrusted" => {
                    Check::new("tls", Outcome::Fail, format!("CA not trusted by {store}"))
                        .fix("devlanes certs trust")
                }
                _ => Check::new(
                    "tls",
                    Outcome::Skip,
                    match entry.get("reason").and_then(Value::as_str) {
                        Some(reason) => format!(
                            "cannot tell whether {store} trusts the CA: {}",
                            clean(reason)
                        ),
                        None => format!("cannot tell whether {store} trusts the CA"),
                    },
                ),
            });
        }
        // A CA from before name constraints stays until the user recreates it.
        if let Some(hint) = ca.get("hint").and_then(Value::as_str) {
            let check = Check::new("tls", Outcome::Skip, hint);
            checks.push(match ca.get("fix").and_then(Value::as_str) {
                Some(fix) => check.fix(fix),
                None => check,
            });
        }
    } else if ca.is_object() {
        checks.push(
            Check::new(
                "tls",
                Outcome::Fail,
                format!("no CA: {}", text(ca, "error")),
            )
            .fix("devlanes certs trust"),
        );
    }
    // Routed hosts the CA cannot certify: HTTPS fails for them, plain HTTP works.
    for warning in list(data, "ca_warnings") {
        let warning_data = &warning["data"];
        let line = words::lane_warning(&text(warning, "code"), warning_data);
        let lane = text(warning_data, "lane");
        let unit = text(warning_data, "unit");
        let owner = match (lane.is_empty(), unit.is_empty()) {
            (false, false) => format!(" ({lane}:{unit})"),
            (false, true) => format!(" ({lane})"),
            _ => String::new(),
        };
        let check = Check::new("tls", Outcome::Skip, format!("{line}{owner}"));
        checks.push(match warning_data.get("fix").and_then(Value::as_str) {
            Some(fix) => check.fix(clean(fix)),
            None => check,
        });
    }
    // Hostnames two lanes claim, invalid ones, and entries the proxy dropped.
    for line in super::lanes::route_warnings(data) {
        checks.push(Check::new("proxy", Outcome::Skip, line));
    }
    let hosts = &data["hosts"];
    // Without a daemon only the helper's version is known.
    if hosts.is_object() && hosts.get("mode").is_none() {
        checks.extend(helper_check(hosts));
    } else if hosts.is_object() {
        let mode = text(hosts, "mode");
        let probe = hosts
            .get("probe")
            .and_then(Value::as_str)
            .map_or("not probed".into(), |probe| {
                format!("resolver {}", probe.replace('_', " "))
            });
        if let Some(error) = hosts.get("last_error").filter(|e| e.is_object()) {
            let code = text(error, "code");
            let check = Check::new(
                "hosts",
                Outcome::Fail,
                format!("/etc/hosts fallback {mode}: {}", text(error, "message")),
            );
            checks.push(match code.as_str() {
                "hosts_file_refused" => check.fix("fix the devlanes markers in /etc/hosts by hand"),
                _ => check.fix("devlanes certs trust --hosts"),
            });
        } else {
            let state = match (text(hosts, "state").as_str(), int(hosts, "names")) {
                ("applied", Some(names)) => format!("{names} names written"),
                ("idle", _) => "nothing written".into(),
                (other, _) => other.to_owned(),
            };
            checks.push(Check::new(
                "hosts",
                Outcome::Ok,
                format!("/etc/hosts fallback {mode}, {probe}, {state}"),
            ));
        }
        if let Some(error) = hosts.get("tld_error").filter(|e| e.is_object()) {
            checks.push(Check::new(
                "hosts",
                Outcome::Skip,
                format!(
                    "{}; only .localhost names are written",
                    text(error, "message")
                ),
            ));
        }
        if let Some(check) = helper_check(hosts) {
            checks.push(check);
        }
    }
    checks.extend(user_config_checks(&data["user_config"]));
    if running {
        checks.push(Check::new(
            "daemon",
            Outcome::Ok,
            format!(
                "{} loaded, {}",
                count(data, "projects", "project"),
                count(data, "subscribers", "event subscriber")
            ),
        ));
    }
    if let Some(release) = data.get("release").filter(|r| r.is_object()) {
        checks.extend(release_checks(release));
    }
    checks.push(Check::new(
        "output",
        Outcome::Ok,
        format!(
            "mode {} ({})",
            cx.runtime.output_description(),
            cx.runtime.reason
        ),
    ));
    checks
}

/// `hosts.helper` (`{state, level, installed, expected, fix}`, `null` before the
/// daemon checked it). A failed write that already blames the helper version says
/// the same, so it is not repeated.
fn helper_check(hosts: &Value) -> Option<Check> {
    let helper = hosts.get("helper").filter(|helper| helper.is_object())?;
    if text(&hosts["last_error"], "code") == "helper_version_mismatch" {
        return None;
    }
    if text(helper, "level") == "ok" {
        return Some(Check::new(
            "hosts",
            Outcome::Ok,
            format!("hosts helper {}", text(helper, "installed")),
        ));
    }
    let fix = helper
        .get("fix")
        .and_then(Value::as_str)
        .map_or_else(|| "devlanes certs trust --hosts".to_owned(), clean);
    Some(Check::new("hosts", Outcome::Skip, words::helper_mismatch(helper)).fix(fix))
}

/// `user_config` (`{path, valid, error {message, file, line, column}, warnings}`):
/// an invalid file fails with its location; ignored settings warn.
fn user_config_checks(config: &Value) -> Vec<Check> {
    if !config.is_object() {
        return Vec::new();
    }
    let path = text(config, "path");
    let mut checks = Vec::new();
    if config.get("valid").and_then(Value::as_bool) == Some(false) {
        let error = &config["error"];
        let file = match text(error, "file") {
            file if file.is_empty() => path.clone(),
            file => file,
        };
        let location = match (int(error, "line"), int(error, "column")) {
            (Some(line), Some(column)) => format!("{file}:{line}:{column}"),
            (Some(line), None) => format!("{file}:{line}"),
            _ => file,
        };
        checks.push(
            Check::new(
                "config",
                Outcome::Fail,
                format!("user config {location}: {}", text(error, "message")),
            )
            .fix(format!("edit {path}, then devlanes daemon restart")),
        );
    } else {
        checks.push(Check::new(
            "config",
            Outcome::Ok,
            format!("user config {path}"),
        ));
    }
    for warning in list(config, "warnings") {
        checks.push(Check::new(
            "config",
            Outcome::Skip,
            format!(
                "user config {}: {}",
                text(warning, "path"),
                text(warning, "message")
            ),
        ));
    }
    checks
}

/// The `release` facts the client adds to doctor: this binary, how it was
/// installed, a version difference with the daemon and the latest release.
fn release_checks(release: &Value) -> Vec<Check> {
    let version = text(release, "version");
    let build = match release.get("build").and_then(Value::as_str) {
        Some("local") | None => "local build".to_owned(),
        Some(build) => format!("release build {build}"),
    };
    let mut checks = vec![Check::new(
        "release",
        Outcome::Ok,
        // `a standalone binary in DIR` and `a source checkout` describe the binary.
        match text(release, "install_method") {
            how if how.starts_with("a ") => format!("devlanes {version} ({build}), {how}"),
            how => format!("devlanes {version} ({build}), installed with {how}"),
        },
    )];
    let daemon = text(release, "daemon_version");
    if !daemon.is_empty() && daemon != version {
        checks.push(
            Check::new(
                "release",
                Outcome::Skip,
                format!("the daemon runs {daemon}, this CLI is {version}"),
            )
            .fix("devlanes daemon restart when convenient (it stops services)"),
        );
    }
    let latest = release.get("latest").and_then(Value::as_str);
    let checked = release
        .get("checked_at")
        .and_then(Value::as_u64)
        .map(|ms| format!(", checked {}", ago(ms)))
        .unwrap_or_default();
    let check = match release.get("check").and_then(Value::as_str) {
        Some("off") => Check::new(
            "release",
            Outcome::Ok,
            match release.get("check_off_reason").and_then(Value::as_str) {
                Some("config") => "release checks are off (updates.check: false)",
                Some("env") => "release checks are off (DEVLANES_NO_UPDATE_CHECK)",
                Some("ci") => "release checks are off in CI",
                Some("debug_build") => "release checks are off in debug builds",
                _ => "release checks are off",
            },
        ),
        Some("ok") if release.get("update_available").and_then(Value::as_bool) == Some(true) => {
            Check::new(
                "release",
                Outcome::Skip,
                format!("devlanes {} is available{checked}", latest.unwrap_or("?")),
            )
            .fix(text(release, "command"))
        }
        Some("ok") => Check::new(
            "release",
            Outcome::Ok,
            format!("latest release {}{checked}", latest.unwrap_or("?")),
        ),
        _ => Check::new("release", Outcome::Ok, "no release check yet"),
    };
    checks.push(check);
    checks
}

/// A past epoch-milliseconds time as `5m ago`, `3h ago` or `2d ago`.
fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// `doctor`: grouped checks, each failing or degraded one with a fix.
pub(super) fn doctor(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let mut checks = doctor_checks(data, cx);
    let mut lines = Vec::new();
    if cx.agent() {
        checks.sort_by_key(|check| match check.outcome {
            Outcome::Fail => 0,
            Outcome::Skip => 1,
            Outcome::Ok => 2,
        });
        for check in checks {
            let word = match check.outcome {
                Outcome::Ok => "ok",
                Outcome::Skip => "warn",
                Outcome::Fail => "fail",
            };
            lines.push(format!("{word} {}: {}", check.group, check.text));
            if let Some(fix) = check.fix {
                lines.push(format!("  fix: {fix}"));
            }
        }
        return lines;
    }
    let mut groups: Vec<&'static str> = Vec::new();
    for check in &checks {
        if !groups.contains(&check.group) {
            groups.push(check.group);
        }
    }
    for group in groups {
        lines.push(group.to_owned());
        for check in checks.iter().filter(|check| check.group == group) {
            let word = match (check.outcome, cx.human()) {
                (outcome, true) => cx.mark(outcome),
                (Outcome::Ok, false) => "ok".into(),
                (Outcome::Skip, false) => "warn".into(),
                (Outcome::Fail, false) => "fail".into(),
            };
            lines.push(format!("  {word} {}", check.text));
            if let Some(fix) = &check.fix {
                lines.push(format!("    fix: {fix}"));
            }
        }
    }
    lines
}

/// `config check`: the project and its warnings; errors arrive as `config_error`.
pub(super) fn config_check(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let mut lines = vec![format!(
        "{} {} ({})",
        cx.state("ok"),
        text(data, "project"),
        display_value(&data["path"])
    )];
    for warning in list(data, "warnings") {
        let message = match text(warning, "code").as_str() {
            "unknown_platform" => {
                let data = &warning["data"];
                let known: Vec<String> = list(data, "known").iter().map(display_value).collect();
                format!(
                    "unknown platform {} for service {}; it starts without platform setup. Known: {}. Fix the name or drop platform: to detect it",
                    text(data, "platform"),
                    text(data, "service"),
                    known.join(", ")
                )
            }
            // `{path, code, ...data}`, worded like the same warning in start and status.
            code @ ("host_outside_ca" | "ports_pending" | "host_conflict" | "host_rejected") => {
                words::lane_warning(code, warning)
            }
            _ => text(warning, "message"),
        };
        lines.push(format!(
            "{} {}: {message}",
            cx.state("warn"),
            text(warning, "path"),
        ));
    }
    lines
}

/// `unlock`: how many secrets the daemon now holds for the lane.
pub(super) fn unlock(data: &Value) -> Vec<String> {
    let keys = data
        .get("keys")
        .and_then(Value::as_array)
        .map(Vec::len)
        .or_else(|| {
            data.get("keys")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        })
        .unwrap_or(0);
    let mut line = format!("unlocked {keys} secrets for {}", text(data, "lane"));
    if let Some(profile) = data.get("profile").and_then(Value::as_str) {
        line.push_str(&format!(" (fnox profile {})", clean(profile)));
    }
    vec![line]
}

/// `daemon status`.
pub(super) fn daemon_status(data: &Value) -> Vec<String> {
    let mut line = format!(
        "running devlanes {} pid {}",
        text(data, "version"),
        display_value(&data["pid"])
    );
    if let Some(uptime) = data.get("uptime_ms").and_then(Value::as_u64) {
        line.push_str(&format!(", up {}", duration(uptime)));
    }
    if data.get("managed_processes").is_some_and(Value::is_u64) {
        line.push_str(&format!(
            ", {}",
            count(data, "managed_processes", "managed process")
        ));
    }
    let mut lines = vec![line];
    if data.get("socket").is_some() {
        lines.push(format!("socket: {}", display_value(&data["socket"])));
    }
    lines
}

/// `daemon stop`.
pub(super) fn daemon_shutdown(data: &Value) -> Vec<String> {
    match int(data, "processes") {
        Some(0) | None => vec!["stopping the daemon".into()],
        Some(_) => vec![format!(
            "stopping the daemon and {}",
            count(data, "processes", "process")
        )],
    }
}

/// `1 project`, `3 projects` from an integer field; `? projects` when absent.
fn count(data: &Value, key: &str, word: &str) -> String {
    match data.get(key).and_then(Value::as_u64) {
        Some(n) => plural(usize::try_from(n).unwrap_or(usize::MAX), word),
        None => plural(2, word).replacen('2', "?", 1),
    }
}

/// `daemon reset-state`.
pub(super) fn reset_state(data: &Value) -> Vec<String> {
    vec![format!(
        "reset the state database; stopped {} units; lane numbers and ports are reassigned",
        data.get("stopped")
            .map_or("0".into(), |stopped| match stopped {
                Value::Array(items) => items.len().to_string(),
                other => display_value(other),
            })
    )]
}
