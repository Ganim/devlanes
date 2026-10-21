//! Logs, tasks, env, metrics, events, infra, doctor, config and daemon results.

use std::collections::BTreeMap;

use devlanes_proto::{Event, LogLine, Method, RpcError, events};
use serde_json::Value;

use super::{
    Ctx, Outcome, bytes, cap_line, clean, compact_fields, display_value, duration, int, list,
    percent, plural, table, tail_lines, text, tree, words,
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
        format!("{}#{}", text(script, "source"), text(script, "name"))
    } else if !list(task, "steps").is_empty() {
        format!("steps {}", joined(task, "steps"))
    } else {
        "run".into()
    };
    for flag in ["confirm", "interactive"] {
        if task.get(flag).and_then(Value::as_bool) == Some(true) {
            runs.push_str(&format!(" ({flag})"));
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
/// Phases skipped because the project has nothing for them print nothing.
pub(super) fn phase_line(data: &Value, cx: &Ctx<'_>) -> Option<String> {
    let phase = text(data, "phase");
    let reason = data.get("reason").and_then(reason_text);
    match text(data, "state").as_str() {
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
            (!code.starts_with("no_")).then(|| cx.skipped(&phase, reason.as_deref()))
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

/// Listener checks for `http` or `https`.
fn listener(proxy: &Value, name: &'static str) -> Option<Check> {
    let entry = proxy.get(name)?;
    if entry.get("bound").and_then(Value::as_bool) == Some(true) {
        let port = display_value(&entry["port"]);
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
    let mut checks = vec![Check::new(
        "daemon",
        Outcome::Ok,
        format!(
            "devlanes {}, pid {}, socket {}",
            text(data, "version"),
            display_value(&data["pid"]),
            display_value(&data["socket"])
        ),
    )];
    match (
        int(data, "schema_version"),
        int(data, "supported_schema_version"),
    ) {
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
            format!("daemon runs inside a sandbox ({})", sandboxed.join(", ")),
        )
        .fix("devlanes daemon install, so the daemon starts outside the sandbox")
    });
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
    let proxy = &data["proxy"];
    checks.extend(
        ["http", "https"]
            .into_iter()
            .filter_map(|name| listener(proxy, name)),
    );
    let ca = &data["ca"];
    if ca.get("present").and_then(Value::as_bool) == Some(true) {
        checks.push(Check::new(
            "tls",
            Outcome::Ok,
            format!("CA at {}", display_value(&ca["path"])),
        ));
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
    let hosts = &data["hosts"];
    if hosts.is_object() {
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
    }
    checks.push(Check::new(
        "daemon",
        Outcome::Ok,
        format!(
            "{} projects loaded, {} event subscribers",
            display_value(&data["projects"]),
            display_value(&data["subscribers"])
        ),
    ));
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
    if let Some(processes) = int(data, "managed_processes") {
        line.push_str(&format!(", {processes} managed processes"));
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
        Some(processes) => vec![format!("stopping the daemon and {processes} processes")],
        None => vec!["stopping the daemon".into()],
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
