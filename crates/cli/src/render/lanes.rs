//! Lanes and units: start, resume, stop, status, lane commands, exec, wait, errors,
//! why and hosts.

use devlanes_proto::{LaneSummary, RpcError, StartResult, UnitFailureData, UnitSummary};
use serde_json::Value;

use super::{
    Ctx, Outcome, bytes, cap_line, clean, display_value, int, leftover_notes, leftover_notes_of,
    list, percent, plural, table, tail_lines, text, tree, words,
};
use crate::Hint;

/// Ready units listed by name before the rest fold into `ready +N`.
const READY_SHOWN: usize = 10;

/// `6/40` for a unit waiting on the start gate: its place in line of all queued starts.
fn queue_place(unit: &UnitSummary) -> Option<String> {
    let reason = unit.reason.as_ref().filter(|r| r.code == "queued")?;
    let position = int(&reason.data, "position")?;
    Some(match int(&reason.data, "queued") {
        Some(total) => format!("{position}/{total}"),
        None => position.to_string(),
    })
}

/// The unit's display name for people, its key for everything else.
fn unit_label(unit: &UnitSummary, cx: &Ctx<'_>) -> String {
    if cx.human() {
        clean(unit.label())
    } else {
        clean(&unit.name)
    }
}

/// The state word, with the queue place for queued units (`queued 6/40`). People
/// see `running` for a unit that is ready only because its process started.
fn state_label(unit: &UnitSummary, cx: &Ctx<'_>) -> String {
    let word = if cx.human() {
        unit.shown_state()
    } else {
        &unit.state
    };
    let state = clean(&word.to_lowercase());
    match queue_place(unit).filter(|_| state == "queued") {
        Some(place) => format!("{state} {place}"),
        None => state,
    }
}

/// Whether a previous daemon left `unit` running; this daemon does not supervise it.
fn orphaned(unit: &UnitSummary) -> bool {
    unit.state == devlanes_proto::ORPHANED
}

/// `left running by a previous daemon (pid N); stop it: devlanes stop api --lane acme@2`.
fn orphan_note(unit: &UnitSummary) -> String {
    let pid = unit
        .pid
        .map(|pid| format!(" (pid {pid})"))
        .unwrap_or_default();
    format!(
        "left running by a previous daemon{pid}; stop it: {}",
        Hint::command([
            "stop",
            clean(&unit.name).as_str(),
            "--lane",
            clean(&unit.lane).as_str()
        ])
        .0
    )
}

/// Agent line for one unit: `state lane:unit exit=N urls ports actor=`.
pub(crate) fn unit_line(unit: &UnitSummary, cx: &Ctx<'_>) -> String {
    let mut line = format!(
        "{} {}:{}",
        cx.state(&clean(&unit.state.to_lowercase())),
        clean(&unit.lane),
        cx.unit_name(&clean(&unit.name), &unit.state, unit.color.as_deref())
    );
    if unit.state == "queued"
        && let Some(place) = queue_place(unit)
    {
        line.push_str(&format!(" {place}"));
    }
    if let Some(code) = unit.exit_code {
        line.push_str(&format!(" exit={code}"));
    }
    if orphaned(unit)
        && let Some(pid) = unit.pid
    {
        line.push_str(&format!(" pid={pid}"));
    }
    for url in &unit.urls {
        line.push(' ');
        line.push_str(&clean(url));
    }
    for (name, port) in &unit.ports {
        line.push_str(&format!(" {}={port}", clean(name)));
    }
    if let Some(id) = &unit.actor.agent_id {
        line.push_str(&format!(" actor={}", clean(id)));
    }
    line
}

/// A unit with its reason, failure tail and, for agents, the command to see more.
fn unit_block(unit: &UnitSummary, tail: &[Value], cx: &Ctx<'_>) -> Vec<String> {
    let mut lines = vec![unit_line(unit, cx)];
    if let Some(reason) = &unit.reason {
        lines.push(format!("  {}", words::error_message(reason).0));
    }
    if orphaned(unit) {
        lines.push(format!("  {}", orphan_note(unit)));
    }
    lines.extend(leftover_notes("  ", &unit.left_running, &unit.escaped));
    if unit.state == "failed" {
        lines.extend(tail_lines(tail));
        if cx.agent() {
            lines.push(format!(
                "  more: {}",
                Hint::logs(&unit.lane, &unit.name, 200).0
            ));
        }
    }
    lines
}

/// A unit from a full or partial summary (event payloads and `with` failure pages
/// omit fields); missing fields take their defaults.
fn loose_unit(value: &Value) -> Option<UnitSummary> {
    let mut merged = serde_json::to_value(UnitSummary::default()).ok()?;
    let (Some(target), Some(fields)) = (merged.as_object_mut(), value.as_object()) else {
        return None;
    };
    for (key, field) in fields {
        // These lists are left out of the defaults' JSON while empty.
        let listed = matches!(key.as_str(), "left_running" | "escaped");
        if (listed || target.contains_key(key)) && !field.is_null() {
            target.insert(key.clone(), field.clone());
        }
    }
    serde_json::from_value(merged).ok()
}

/// Units as data: a page of them, or one; failed first for agents.
fn units(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let items: Vec<&Value> = match data.get("items").and_then(Value::as_array) {
        Some(items) => items.iter().collect(),
        None if data.get("name").is_some() => vec![data],
        // `unit.changed` payloads: `{unit: UnitSummary}`.
        None if data.get("unit").is_some_and(Value::is_object) => vec![&data["unit"]],
        None => return tree(data),
    };
    let mut parsed: Vec<(UnitSummary, &[Value])> = items
        .into_iter()
        .filter_map(|value| Some((loose_unit(value)?, list(value, "log_tail"))))
        .collect();
    if cx.agent() {
        parsed.sort_by_key(|(unit, _)| unit.state != "failed");
    }
    parsed
        .iter()
        .flat_map(|(unit, tail)| unit_block(unit, tail, cx))
        .collect()
}

/// `start`, `restart` and the start part of `lane new --start`: a [`StartResult`].
/// Anything else (`with` passes failure pages and an empty object) renders as units.
pub(super) fn start_like(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    if data.get("ready").is_none() || data.get("lane").is_none() {
        return if data.as_object().is_some_and(|map| map.is_empty()) {
            Vec::new()
        } else {
            units(data, cx)
        };
    }
    match serde_json::from_value::<StartResult>(data.clone()) {
        Ok(result) => start_result(&result, cx),
        Err(_) => tree(data),
    }
}

/// Counts and groups of one start result.
struct StartView<'a> {
    services: Vec<&'a UnitSummary>,
    ready: Vec<&'a UnitSummary>,
    failed: Vec<(
        &'a str,
        Option<&'a UnitFailureData>,
        Option<&'a UnitSummary>,
    )>,
    pending: Vec<&'a UnitSummary>,
}

impl<'a> StartView<'a> {
    /// Splits `result` into ready, failed and still starting units.
    fn new(result: &'a StartResult) -> Self {
        let services: Vec<&UnitSummary> = result
            .units
            .iter()
            .filter(|unit| unit.kind.is_empty() || unit.kind == "service")
            .filter(|unit| unit.autostart || !matches!(unit.state.as_str(), "idle" | "stopped"))
            .collect();
        let ready = result
            .units
            .iter()
            .filter(|unit| unit.state == "ready")
            .collect();
        let mut failed: Vec<(&str, Option<&UnitFailureData>, Option<&UnitSummary>)> = result
            .failures
            .iter()
            .map(|failure| {
                let unit = result.units.iter().find(|unit| unit.name == failure.unit);
                (failure.unit.as_str(), Some(failure), unit)
            })
            .collect();
        for unit in result.units.iter().filter(|unit| unit.state == "failed") {
            if !failed.iter().any(|(name, _, _)| *name == unit.name) {
                failed.push((unit.name.as_str(), None, Some(unit)));
            }
        }
        // A unit `with` reports as timed out is listed once, as a failure.
        let pending = result
            .units
            .iter()
            .filter(|unit| matches!(unit.state.as_str(), "starting" | "queued"))
            .filter(|unit| !failed.iter().any(|(name, _, _)| *name == unit.name))
            .collect();
        Self {
            services,
            ready,
            failed,
            pending,
        }
    }

    /// Ready services among the lane's services.
    fn ready_services(&self) -> usize {
        self.services
            .iter()
            .filter(|unit| unit.state == "ready")
            .count()
    }
}

/// Exit code of a failed unit, from the failure or its summary.
fn failure_exit(failure: Option<&UnitFailureData>, unit: Option<&UnitSummary>) -> Option<i32> {
    failure
        .and_then(|failure| failure.exit_code)
        .or_else(|| unit.and_then(|unit| unit.exit_code))
}

/// Reason of a failed unit, from the failure or its summary.
fn failure_reason<'a>(
    failure: Option<&'a UnitFailureData>,
    unit: Option<&'a UnitSummary>,
) -> Option<&'a RpcError> {
    failure
        .and_then(|failure| failure.reason.as_ref())
        .or_else(|| unit.and_then(|unit| unit.reason.as_ref()))
}

/// Ready units with URLs come first; the rest fold into a count.
fn split_ready<'a>(ready: &[&'a UnitSummary]) -> (Vec<&'a UnitSummary>, usize) {
    let shown: Vec<&UnitSummary> = ready
        .iter()
        .filter(|unit| !unit.urls.is_empty())
        .take(READY_SHOWN)
        .copied()
        .collect();
    (shown.clone(), ready.len() - shown.len())
}

/// One line per [`devlanes_proto::LaneWarning`] of a start: advice, never a failure.
fn start_warnings(result: &StartResult, cx: &Ctx<'_>) -> Vec<String> {
    result
        .warnings
        .iter()
        .map(|warning| cx.warning(&words::lane_warning(&warning.code, &warning.data)))
        .collect()
}

/// A start that found every selected unit up: it spawned nothing and ran no task.
fn nothing_started(result: &StartResult) -> bool {
    result.started.is_empty() && !result.already_running.is_empty() && result.failures.is_empty()
}

/// The cli-design 3.5 start summary.
fn start_result(result: &StartResult, cx: &Ctx<'_>) -> Vec<String> {
    let view = StartView::new(result);
    let lane = clean(&result.lane);
    let (shown, folded) = split_ready(&view.ready);
    let mut lines = Vec::new();
    if cx.agent() {
        let mut header = match &result.branch {
            Some(branch) => format!("{lane} ({}): ", clean(branch)),
            None => format!("{lane}: "),
        };
        if view.services.is_empty() {
            header.push_str(if result.ready { "ready" } else { "not ready" });
        } else {
            header.push_str(&format!(
                "{}/{} ready",
                view.ready_services(),
                view.services.len()
            ));
        }
        if !view.failed.is_empty() {
            header.push_str(&format!(", {} failed", view.failed.len()));
        }
        if result.timed_out {
            header.push_str(", timed out");
        }
        if nothing_started(result) {
            header.push_str(", already running (nothing started)");
        }
        if !result.skipped_tasks.is_empty() {
            let names: Vec<String> = result.skipped_tasks.iter().map(|t| clean(t)).collect();
            header.push_str(&format!(", {} skipped", names.join(", ")));
        }
        lines.push(header);
        for (name, failure, unit) in &view.failed {
            let mut line = format!("failed {}", clean(name));
            if let Some(code) = failure_exit(*failure, *unit) {
                line.push_str(&format!(" exit={code}"));
            }
            lines.push(line);
            if let Some(reason) = failure_reason(*failure, *unit) {
                lines.push(format!("  {}", row_reason(reason)));
            }
            if let Some(failure) = failure {
                let tail: Vec<Value> = own_lines(&failure.log_tail)
                    .map(|line| Value::String(line.clone()))
                    .collect();
                lines.extend(tail_lines(&tail));
            }
            lines.push(
                match (refusal_hint(*failure, *unit), spawn_fix(*failure, *unit)) {
                    (Some(hint), _) => format!("  stop: {}", hint.0),
                    (None, Some(hint)) => format!("  hint: {}", hint.0),
                    (None, None) => format!("  more: {}", Hint::logs(&lane, name, 200).0),
                },
            );
        }
        for unit in &view.pending {
            let label = state_label(unit, cx);
            let (word, place) = label.split_once(' ').unwrap_or((label.as_str(), ""));
            let place = if place.is_empty() {
                String::new()
            } else {
                format!(" {place}")
            };
            lines.push(format!("{word} {}{place}", clean(&unit.name)));
        }
        for unit in &shown {
            lines.push(format!(
                "ready {} {}",
                clean(&unit.name),
                unit.urls
                    .iter()
                    .map(|url| clean(url))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if folded > 0 {
            lines.push(format!(
                "ready +{folded} ({})",
                Hint::command(["status", lane.as_str()]).0
            ));
        }
        lines.extend(start_leftovers(result, ""));
        lines.extend(start_warnings(result, cx));
        // The next step for a timeout: why the first unit still waits.
        if result.timed_out
            && let Some(unit) = view.pending.first()
        {
            lines.push(format!(
                "next: {}",
                Hint::command(["debug", "why", &format!("{lane}:{}", clean(&unit.name))]).0
            ));
        }
        return lines;
    }
    lines.push(match &result.branch {
        Some(branch) => format!("{lane} · {}", clean(branch)),
        None => lane.clone(),
    });
    // A start that ran no service (its units were refused) has no services
    // line: "0 of 0 ready" says nothing.
    if !view.services.is_empty() {
        let outcome = if view.failed.is_empty() && result.ready {
            Outcome::Ok
        } else {
            Outcome::Fail
        };
        lines.push(format!(
            "{} services  {} of {} ready",
            cx.mark(outcome),
            view.ready_services(),
            view.services.len()
        ));
    } else if result.ready {
        lines.push(format!("{} nothing to start", cx.mark(Outcome::Ok)));
    }
    if nothing_started(result) {
        lines.push(format!(
            "{} already running; nothing was started",
            cx.mark(Outcome::Ok)
        ));
    }
    let mut rows = Vec::new();
    for (name, failure, unit) in &view.failed {
        let mut detail = match failure_exit(*failure, *unit) {
            Some(code) => format!("exited {code}"),
            None => failure_reason(*failure, *unit)
                .map(row_reason)
                .unwrap_or_else(|| "failed".into()),
        };
        // The last line that is not a stack frame usually names the error.
        let tail: Vec<&String> =
            own_lines(failure.map_or(&[][..], |f| f.log_tail.as_slice())).collect();
        if let Some(line) = tail
            .iter()
            .rev()
            .find(|line| !clean(line).trim_start().starts_with("at "))
            .or(tail.last())
        {
            detail.push_str(&format!("   {}", cap_line(&clean(line), 120)));
        }
        rows.push(vec![cx.mark(Outcome::Fail), clean(name), detail]);
    }
    for unit in &view.pending {
        rows.push(vec![
            cx.state(&clean(&unit.state)),
            unit_label(unit, cx),
            queue_place(unit)
                .map(|place| cx.paint(&format!("{place} in line"), "2"))
                .unwrap_or_default(),
        ]);
    }
    for unit in &shown {
        rows.push(vec![
            cx.mark(Outcome::Ok),
            unit_label(unit, cx),
            display_url(&unit.urls).map_or_else(String::new, |url| clean(url)),
        ]);
    }
    lines.extend(table(&rows, "  "));
    if folded > 0 {
        let dots = if cx.human() { "…" } else { "+" };
        lines.push(format!(
            "  {dots}{folded} more ready ({})",
            Hint::command(["status", lane.as_str()]).0
        ));
    }
    if result.timed_out {
        let prefix = if cx.human() {
            format!("{} ", cx.mark(Outcome::Skip))
        } else {
            String::new()
        };
        lines.push(format!("{prefix}timed out before every unit was ready"));
    }
    lines.extend(start_warnings(result, cx));
    let cause = view
        .failed
        .first()
        .map(|(name, _, _)| format!("{} failed", clean(name)));
    for task in &result.skipped_tasks {
        lines.push(cx.skipped(&clean(task), cause.as_deref()));
    }
    lines.extend(start_leftovers(result, "  "));
    if let Some((name, failure, unit)) = view.failed.first() {
        lines.push(String::new());
        lines.push(
            match (refusal_hint(*failure, *unit), spawn_fix(*failure, *unit)) {
                (Some(hint), _) => format!("stop: {}", hint.0),
                (None, Some(hint)) => format!("hint: {}", hint.0),
                (None, None) => format!("logs: {}", Hint::logs(&lane, name, 200).0),
            },
        );
    }
    lines
}

/// A failure reason on its unit's row or under its `failed` line, where the unit
/// is already named.
fn row_reason(reason: &RpcError) -> String {
    if reason.code == "spawn_failed" && reason.data.get("unit").is_some() {
        format!("couldn't start: {}", words::spawn_cause(&reason.data))
    } else {
        words::error_message(reason).0
    }
}

/// The unit's own lines of a failure tail, without the `[devlanes]` notes
/// that repeat the failure's reason.
fn own_lines(tail: &[String]) -> impl Iterator<Item = &String> {
    tail.iter().filter(|line| !line.starts_with("[devlanes] "))
}

/// The fix for a unit whose program could not be run (see [`words::spawn_hint`]).
fn spawn_fix(failure: Option<&UnitFailureData>, unit: Option<&UnitSummary>) -> Option<Hint> {
    failure_reason(failure, unit)
        .filter(|reason| reason.code == "spawn_failed")
        .and_then(|reason| words::spawn_hint(&reason.data))
}

/// The command for a unit the start refused rather than ran: a previous
/// daemon's orphan has no new logs, it needs stopping first.
fn refusal_hint(failure: Option<&UnitFailureData>, unit: Option<&UnitSummary>) -> Option<Hint> {
    failure_reason(failure, unit)
        .filter(|reason| reason.code == "unit_orphaned")
        .and_then(|reason| words::error_message(reason).1)
}

/// `<unit>: left running: ...` notes for the start's tasks and setup commands.
fn start_leftovers(result: &StartResult, indent: &str) -> Vec<String> {
    result
        .leftovers
        .iter()
        .flat_map(|leftovers| {
            leftover_notes(
                &format!("{indent}{}: ", clean(&leftovers.unit)),
                &leftovers.left_running,
                &leftovers.escaped,
            )
        })
        .collect()
}

/// `resume`: one start summary per lane, or the lane's error.
pub(super) fn resume(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let items = list(data, "items");
    if items.is_empty() {
        return vec!["nothing to resume".into()];
    }
    let mut lines = Vec::new();
    for item in items {
        if !cx.agent() && !lines.is_empty() {
            lines.push(String::new());
        }
        match item.get("error") {
            Some(error) => {
                let message = serde_json::from_value::<RpcError>(error.clone())
                    .map(|error| words::error_message(&error).0)
                    .unwrap_or_else(|_| display_value(error));
                lines.push(format!(
                    "{} {}: {message}",
                    cx.state("failed"),
                    text(item, "lane")
                ));
            }
            None => lines.extend(start_like(item, cx)),
        }
    }
    lines
}

/// `stop`: one `stopped lane:unit` line per stopped unit.
pub(super) fn stop(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let items = list(data, "items");
    if items.is_empty() {
        return vec!["nothing was running".into()];
    }
    items
        .iter()
        .filter_map(|value| serde_json::from_value::<UnitSummary>(value.clone()).ok())
        .map(|unit| {
            // Stable agent wording: the process is gone, whatever its exit looked like.
            let word = if unit.state == "failed" {
                "failed"
            } else {
                "stopped"
            };
            let mut line = format!(
                "{} {}:{}",
                cx.state(word),
                clean(&unit.lane),
                clean(&unit.name)
            );
            if let Some(code) = unit.exit_code.filter(|code| *code != 0) {
                line.push_str(&format!(" exit={code}"));
            }
            // A stopped orphan keeps `orphaned_since_ms`.
            if unit.orphaned_since_ms.is_some() {
                line.push_str(" (left running by a previous daemon)");
            }
            line
        })
        .collect()
}

/// Service units of a lane.
fn services(lane: &LaneSummary) -> Vec<&UnitSummary> {
    lane.units
        .iter()
        .filter(|unit| unit.kind.is_empty() || unit.kind == "service")
        // A service that only starts when named counts once it runs.
        .filter(|unit| unit.autostart || !matches!(unit.state.as_str(), "idle" | "stopped"))
        .collect()
}

/// Notes shown after a lane row: moves, config, missing env or worktree, reasons.
fn lane_notes(lane: &LaneSummary) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(index) = lane.moves_to {
        notes.push(format!("moves to lane {index}"));
    }
    if lane.config_changed {
        notes.push("config changed".into());
    }
    if lane.needs_env {
        notes.push(format!(
            "needs start from a shell: {}",
            Hint::command(["start", "--lane", lane.reference.as_str()]).0
        ));
    }
    if let Some(reason) = &lane.reason {
        notes.push(match reason.code.as_str() {
            "worktree_missing" => format!("worktree missing: {}", Hint::command(["lane", "gc"]).0),
            "unassigned" => match reason.data.get("reason").and_then(Value::as_str) {
                Some("pool_exhausted") => {
                    format!("port pool full; {}", Hint::command(["lane", "gc"]).0)
                }
                Some("lanes_max") => format!(
                    "lanes.max {} reached; {}",
                    display_value(&reason.data["max"]),
                    Hint::command(["lane", "gc"]).0
                ),
                _ => "unassigned".into(),
            },
            _ => words::error_message(reason).0,
        });
    }
    if lane.state == "no-config" && lane.reason.is_none() {
        notes.push("no devlanes.yaml".into());
    }
    notes
}

/// The lane's warnings (`ports_pending`, `host_conflict`), one marked line each,
/// below the lane rather than in its note: they carry commands and semicolons.
fn lane_warning_lines(lane: &LaneSummary, cx: &Ctx<'_>) -> Vec<String> {
    lane.warnings
        .iter()
        .map(|warning| {
            cx.warning(&format!(
                "{}: {}",
                clean(&lane.reference),
                words::lane_warning(&warning.code, &warning.data)
            ))
        })
        .collect()
}

/// Whether `url` goes straight to a port on this machine (`http://127.0.0.1:20101`)
/// rather than through the proxy by hostname.
fn loopback(url: &str) -> bool {
    let authority = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost" | "0.0.0.0")
}

/// The URL to show for a unit: its first proxied (hostname) URL, else its first.
pub(crate) fn display_url(urls: &[String]) -> Option<&String> {
    urls.iter().find(|url| !loopback(url)).or(urls.first())
}

/// `scheme://` and `:port` of the first proxied unit URL of `lanes` (the proxy
/// serves every lane on the same listener), for lane URLs.
fn proxy_origin(lanes: &[LaneSummary]) -> Option<(String, String)> {
    lanes
        .iter()
        .flat_map(|lane| lane.units.iter())
        .flat_map(|unit| unit.urls.iter())
        .filter(|url| !loopback(url))
        .find_map(|url| {
            let (scheme, rest) = url.split_once("://")?;
            let authority = rest.split('/').next().unwrap_or_default();
            let port = authority
                .rsplit_once(':')
                .filter(|(host, port)| {
                    !host.ends_with(']') || port.bytes().all(|b| b.is_ascii_digit())
                })
                .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
                .map(|(_, port)| format!(":{port}"))
                .unwrap_or_default();
            Some((scheme.to_owned(), port))
        })
}

/// The lane's base URL from its domain, with the scheme and port the proxy
/// serves units on (`https://acme-2.localhost:8443`); the bare domain when no
/// unit URL shows them.
fn lane_url(lane: &LaneSummary, origin: Option<&(String, String)>) -> String {
    match (&lane.domain, origin) {
        (None, _) => "–".into(),
        (Some(domain), Some((scheme, port))) => format!("{scheme}://{}{port}", clean(domain)),
        (Some(domain), None) => clean(domain),
    }
}

/// Sum of the units' and the lane's own containers' CPU and memory, when sampled.
/// Shared containers serve every lane, so no lane total counts them.
fn usage(lane: &LaneSummary) -> (Option<f64>, Option<u64>) {
    let own = lane.containers.iter().filter(|container| !container.shared);
    let cpu: Vec<f64> = lane
        .units
        .iter()
        .filter_map(|unit| unit.cpu_percent)
        .chain(own.clone().filter_map(|container| container.cpu_percent))
        .collect();
    let memory: Vec<u64> = lane
        .units
        .iter()
        .filter_map(|unit| unit.memory_bytes)
        .chain(own.filter_map(|container| container.memory_bytes))
        .collect();
    (
        (!cpu.is_empty()).then(|| cpu.iter().sum()),
        (!memory.is_empty()).then(|| memory.iter().sum()),
    )
}

/// `status` and `lane ls`: lanes per project; units too when one lane is shown.
pub(super) fn status(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let items = list(data, "items");
    let lanes: Vec<LaneSummary> = items
        .iter()
        .filter(|item| item.get("reference").is_some())
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();
    if lanes.is_empty() {
        // Older callers pass unit pages under `status`.
        if items.iter().any(|item| item.get("name").is_some()) {
            return units(data, cx);
        }
        // A start without devlanes.yaml fails, so name the file and an example too.
        return vec![format!(
            "no lanes; run devlanes start in a repository that has a devlanes.yaml (a minimal one: {})",
            words::EXAMPLE_CONFIG
        )];
    }
    let single = lanes.len() == 1;
    let mut lines = Vec::new();
    if cx.agent() {
        for lane in &lanes {
            lines.push(agent_lane(lane));
            lines.extend(lane_warning_lines(lane, cx));
            if single {
                let mut units: Vec<&UnitSummary> = lane.units.iter().collect();
                units.sort_by_key(|unit| unit.state != "failed");
                for unit in units {
                    lines.extend(unit_block(unit, &[], cx));
                }
            }
        }
        return lines;
    }
    let origin = proxy_origin(&lanes);
    let mut project: Option<&str> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut notes: Vec<Vec<String>> = Vec::new();
    let flush =
        |lines: &mut Vec<String>, rows: &mut Vec<Vec<String>>, notes: &mut Vec<Vec<String>>| {
            if rows.is_empty() {
                return;
            }
            for (line, note) in table(rows, "  ").into_iter().zip(notes.iter()) {
                if note.is_empty() || line.trim_start().starts_with("LANE") {
                    lines.push(line);
                } else {
                    lines.push(format!("{line}  ({})", note.join("; ")));
                }
            }
            rows.clear();
            notes.clear();
        };
    for lane in &lanes {
        if project != Some(lane.project.as_str()) {
            flush(&mut lines, &mut rows, &mut notes);
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(clean(&lane.project));
            rows.push(
                ["LANE", "BRANCH", "STATE", "SERVICES", "CPU", "MEM", "URL"]
                    .map(str::to_owned)
                    .to_vec(),
            );
            notes.push(Vec::new());
            project = Some(lane.project.as_str());
        }
        let services = services(lane);
        let ready = services.iter().filter(|unit| unit.state == "ready").count();
        let active = matches!(
            lane.state.as_str(),
            "running" | "failed" | "starting" | "stopping"
        );
        let (cpu, memory) = usage(lane);
        rows.push(vec![
            lane.index.map_or("–".into(), |index| index.to_string()),
            lane.branch.as_deref().map_or("–".into(), clean),
            cx.state(&clean(&lane.state)),
            if active && !services.is_empty() {
                format!("{ready}/{}", services.len())
            } else {
                "–".into()
            },
            cpu.map_or("–".into(), percent),
            memory.map_or("–".into(), bytes),
            if lane.index.is_some() {
                // The lane's own units first: their URLs carry the port it is served on.
                lane_url(
                    lane,
                    proxy_origin(std::slice::from_ref(lane))
                        .as_ref()
                        .or(origin.as_ref()),
                )
            } else {
                "–".into()
            },
        ]);
        notes.push(lane_notes(lane));
    }
    flush(&mut lines, &mut rows, &mut notes);
    lines.extend(lanes.iter().flat_map(|lane| lane_warning_lines(lane, cx)));
    if let [lane] = lanes.as_slice()
        && !lane.units.is_empty()
    {
        lines.push(String::new());
        lines.extend(unit_table(lane, cx));
    }
    lines
}

/// `state project@N branch ready/total [failed=a,b] [notes]` for agents.
fn agent_lane(lane: &LaneSummary) -> String {
    let services = services(lane);
    if lane.index.is_none() {
        let mut line = format!(
            "{} {} {}",
            clean(&lane.state),
            clean(&lane.project),
            lane.branch
                .as_deref()
                .map_or_else(|| clean(&lane.path.display().to_string()), clean)
        );
        if let Some(reason) = &lane.reason {
            let why = match reason.data.get("reason").and_then(Value::as_str) {
                Some("pool_exhausted") => "pool full".into(),
                Some("lanes_max") => "lanes.max reached".into(),
                Some(other) => other.replace('_', " "),
                None => reason.code.replace('_', " "),
            };
            line.push_str(&format!(" reason=\"{why}\""));
        }
        return line;
    }
    let mut line = format!("{} {}", clean(&lane.state), clean(&lane.reference));
    if let Some(branch) = &lane.branch {
        line.push_str(&format!(" {}", clean(branch)));
    }
    if matches!(
        lane.state.as_str(),
        "running" | "failed" | "starting" | "stopping"
    ) && !services.is_empty()
    {
        let ready = services.iter().filter(|unit| unit.state == "ready").count();
        line.push_str(&format!(" {ready}/{}", services.len()));
    }
    let failed: Vec<String> = lane
        .units
        .iter()
        .filter(|unit| unit.state == "failed")
        .map(|unit| clean(&unit.name))
        .collect();
    if !failed.is_empty() {
        line.push_str(&format!(" failed={}", failed.join(",")));
    }
    for note in lane_notes(lane) {
        line.push_str(&format!(" ({note})"));
    }
    line
}

/// Units of one lane: `UNIT STATE PORT CPU MEM URL`, failed first.
fn unit_table(lane: &LaneSummary, cx: &Ctx<'_>) -> Vec<String> {
    let mut units: Vec<&UnitSummary> = lane.units.iter().collect();
    units.sort_by_key(|unit| unit.state != "failed");
    let mut rows = vec![
        ["UNIT", "STATE", "PORT", "CPU", "MEM", "URL"]
            .map(str::to_owned)
            .to_vec(),
    ];
    let mut extra = Vec::new();
    for unit in &units {
        let ports: Vec<String> = unit.ports.values().map(u16::to_string).collect();
        let mut state = state_label(unit, cx);
        if let Some(code) = unit.exit_code.filter(|_| unit.state == "failed") {
            state.push_str(&format!(" ({code})"));
        }
        rows.push(vec![
            cx.unit_name(&unit_label(unit, cx), &unit.state, unit.color.as_deref()),
            cx.state_text(&unit.state, &state),
            if ports.is_empty() {
                "–".into()
            } else {
                ports.join(",")
            },
            unit.cpu_percent.map_or("–".into(), percent),
            unit.memory_bytes.map_or("–".into(), bytes),
            display_url(&unit.urls).map_or_else(String::new, |url| clean(url)),
        ]);
        if let Some(reason) = &unit.reason {
            extra.push(format!(
                "  {}: {}",
                clean(&unit.name),
                words::error_message(reason).0
            ));
        }
        if orphaned(unit) {
            let note = format!("{}: {}", clean(&unit.name), orphan_note(unit));
            extra.push(format!("  {}", cx.state_text(&unit.state, &note)));
        }
        extra.extend(leftover_notes(
            &format!("  {}: ", clean(&unit.name)),
            &unit.left_running,
            &unit.escaped,
        ));
    }
    // Containers with a Docker stats sample are running; the rest stay in `infra status`.
    for container in lane
        .containers
        .iter()
        .filter(|c| c.cpu_percent.is_some() || c.memory_bytes.is_some())
    {
        let ports: Vec<String> = container.ports.values().map(u16::to_string).collect();
        let kind = if container.shared {
            "shared container"
        } else {
            "container"
        };
        rows.push(vec![
            cx.unit_name(&clean(&container.name), "ready", container.color.as_deref()),
            cx.state_text("idle", kind),
            if ports.is_empty() {
                "–".into()
            } else {
                ports.join(",")
            },
            container.cpu_percent.map_or("–".into(), percent),
            container.memory_bytes.map_or("–".into(), bytes),
            String::new(),
        ]);
    }
    let mut lines = table(&rows, "  ");
    lines.extend(extra);
    lines
}

/// `lane new`: the created lane, then the start summary when `--start` ran.
pub(super) fn lane_new(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let Ok(lane) = serde_json::from_value::<LaneSummary>(data["lane"].clone()) else {
        return tree(data);
    };
    let mut line = format!("created {}", clean(&lane.reference));
    // Another lane for an existing folder answers to its name too.
    if let Some(name) = data.get("name").and_then(Value::as_str) {
        line.push_str(&format!(" named {}", clean(name)));
    }
    if let Some(branch) = &lane.branch {
        line.push_str(&format!(" ({})", clean(branch)));
    }
    line.push_str(&format!(" at {}", clean(&lane.path.display().to_string())));
    let mut lines = vec![line];
    for note in lane_notes(&lane) {
        lines.push(format!("  {note}"));
    }
    lines.extend(lane_warning_lines(&lane, cx));
    match data.get("start") {
        Some(start) => lines.extend(start_like(start, cx)),
        None => lines.push(format!(
            "next: {}",
            Hint::command(["start", "--lane", lane.reference.as_str()]).0
        )),
    }
    lines
}

/// `lane clean`: what was stopped and run, with failures spelled out.
pub(super) fn lane_clean(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let lane = text(data, "lane");
    let stopped = list(data, "stopped").len();
    let tasks = list(data, "tasks");
    let teardown = list(data, "teardown");
    let mut summary = format!("cleaned {lane}: stopped {}", plural(stopped, "unit"));
    let names = |entries: &[Value], key: &str| -> String {
        entries
            .iter()
            .map(|entry| text(entry, key))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !tasks.is_empty() {
        summary.push_str(&format!(", ran {}", names(tasks, "task")));
    }
    if !teardown.is_empty() {
        summary.push_str(&format!(", teardown {}", names(teardown, "entry")));
    }
    let mut lines = vec![summary];
    for (entry, key) in tasks
        .iter()
        .map(|t| (t, "task"))
        .chain(teardown.iter().map(|t| (t, "entry")))
    {
        let ignored = entry.get("ignored").and_then(Value::as_bool) == Some(true);
        if let Some(error) = entry.get("error") {
            lines.push(format!(
                "  {} {}: {}",
                cx.state("failed"),
                text(entry, key),
                text(error, "code")
            ));
        } else if let Some(code) = int(entry, "exit_code").filter(|code| *code != 0) {
            let note = if ignored { " (ignored)" } else { "" };
            lines.push(format!(
                "  {} {} exit={code}{note}",
                cx.state("failed"),
                text(entry, key)
            ));
        }
    }
    if let Some(error) = data.get("infra").and_then(|infra| infra.get("error")) {
        lines.push(format!("  infra: {}", text(error, "message")));
    }
    lines
}

/// `lane gc`: removed lanes and port offsets.
pub(super) fn lane_gc(data: &Value) -> Vec<String> {
    let cleaned: Vec<String> = list(data, "cleaned").iter().map(display_value).collect();
    let offsets = int(data, "offsets_removed").unwrap_or(0);
    let lanes = if cleaned.is_empty() {
        "no lanes to clean".into()
    } else {
        format!("cleaned {}", cleaned.join(", "))
    };
    let mut lines = vec![format!("{lanes}; removed {offsets} unused port offsets")];
    // Lanes whose worktree is gone but whose units still run: gc never stops anything.
    for lane in list(data, "kept_running").iter().map(display_value) {
        lines.push(format!(
            "kept {lane}: its worktree is gone but units still run; stop them with {}, then run {} again",
            Hint::command(["stop", "--lane", lane.as_str()]).0,
            Hint::command(["lane", "gc"]).0
        ));
    }
    lines
}

/// `exec`: the adopted unit and how to follow or stop it.
pub(super) fn exec(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let Ok(unit) = serde_json::from_value::<UnitSummary>(data.clone()) else {
        return tree(data);
    };
    let mut line = unit_line(&unit, cx);
    if let Some(pid) = unit.pid {
        line.push_str(&format!(" pid={pid}"));
    }
    let target = format!("{}:{}", unit.lane, unit.name);
    vec![
        line,
        format!(
            "  logs: {}",
            Hint::command(["logs", target.as_str(), "-f"]).0
        ),
        format!("  stop: {}", Hint::command(["stop", target.as_str()]).0),
    ]
}

/// `wait`: the target's final state, with the failure tail when it failed.
pub(super) fn wait(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let lane = text(data, "lane");
    let unit = text(data, "unit");
    let target = if unit.is_empty() {
        lane.clone()
    } else {
        format!("{lane}:{unit}")
    };
    let state = text(data, "state");
    let word = match state.as_str() {
        "exited" | "stopping" => "stopped",
        other => other,
    };
    let mut line = format!("{} {target}", cx.state(word));
    if let Some(code) = int(data, "exit_code") {
        line.push_str(&format!(" exit={code}"));
    }
    let mut lines = vec![line];
    match word {
        "failed" => {
            let failed: Vec<String> = list(data, "failed").iter().map(display_value).collect();
            if unit.is_empty() && !failed.is_empty() {
                lines.push(format!("  failed units: {}", failed.join(", ")));
            }
            if let Some(reason) = data
                .get("reason")
                .and_then(|r| serde_json::from_value::<RpcError>(r.clone()).ok())
            {
                lines.push(format!("  {}", words::error_message(&reason).0));
            }
            lines.extend(tail_lines(list(data, "log_tail")));
            let first = failed.first().cloned().unwrap_or(unit);
            if !first.is_empty() {
                lines.push(format!("  more: {}", Hint::logs(&lane, &first, 200).0));
            }
        }
        "idle" => lines.push(format!(
            "  not started; {}",
            if unit.is_empty() {
                Hint::command(["start", "--lane", lane.as_str()])
            } else {
                Hint::command(["start", target.as_str()])
            }
            .0
        )),
        // Never ready: nothing supervises it, so its readiness cannot be checked.
        "orphaned" if !unit.is_empty() => {
            let pid = int(data, "pid")
                .map(|pid| format!(" (pid {pid})"))
                .unwrap_or_default();
            lines.push(format!(
                "  left running by a previous daemon{pid} and not supervised; stop it, then start it: {}",
                Hint::command(["stop", unit.as_str(), "--lane", lane.as_str()]).0
            ));
        }
        _ => {}
    }
    lines
}

/// `errors`: crashes, error lines and port conflicts per unit.
pub(super) fn errors(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let mut items: Vec<&Value> = list(data, "items").iter().collect();
    if items.is_empty() {
        return vec!["no errors".into()];
    }
    if cx.agent() {
        items.sort_by_key(|item| text(item, "state") != "failed");
    }
    let mut lines = Vec::new();
    for item in items {
        let (lane, unit) = (text(item, "lane"), text(item, "unit"));
        let mut line = format!("{} {lane}:{unit}", cx.state(&text(item, "state")));
        if let Some(code) = int(item, "exit_code") {
            line.push_str(&format!(" exit={code}"));
        }
        if let Some(crashes) = int(item, "crashes").filter(|n| *n > 0) {
            line.push_str(&format!(" crashes={crashes}"));
        }
        lines.push(line);
        if let Some(reason) = item
            .get("reason")
            .and_then(|r| serde_json::from_value::<RpcError>(r.clone()).ok())
        {
            lines.push(format!("  {}", words::error_message(&reason).0));
        }
        for conflict in list(item, "port_conflicts") {
            lines.push(match conflict.get("pid").filter(|pid| !pid.is_null()) {
                Some(pid) => format!(
                    "  port {} is held by PID {}",
                    display_value(&conflict["port"]),
                    display_value(pid)
                ),
                None => format!(
                    "  port {} is held by another process",
                    display_value(&conflict["port"])
                ),
            });
        }
        lines.extend(tail_lines(list(item, "error_lines")));
        if cx.agent() {
            lines.push(format!("  more: {}", Hint::logs(&lane, &unit, 200).0));
        }
    }
    lines
}

/// `why`: the unit's state and each reason with the command that addresses it.
pub(super) fn why(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let (lane, unit) = (text(data, "lane"), text(data, "unit"));
    let mut lines = vec![format!("{} {lane}:{unit}", cx.state(&text(data, "state")))];
    let leftovers = leftover_notes_of("  ", data);
    let reasons = list(data, "reasons");
    if reasons.is_empty() {
        lines.extend(leftovers);
        lines.push(format!(
            "  no blocking reason found; {}",
            Hint::logs(&lane, &unit, 200).0
        ));
        return lines;
    }
    for reason in reasons {
        let code = text(reason, "code");
        let (message, hint) = words::reason(
            &code,
            reason.get("data").unwrap_or(&Value::Null),
            &lane,
            &unit,
        );
        lines.push(match hint {
            Some(hint) => format!("  {message} ({})", hint.0),
            None => format!("  {message}"),
        });
    }
    lines.extend(leftovers);
    lines
}

/// `hosts`: every route as `host -> lane:unit port state`, then listener facts.
pub(super) fn hosts(data: &Value, cx: &Ctx<'_>) -> Vec<String> {
    let items = list(data, "items");
    let mut lines = Vec::new();
    if items.is_empty() {
        lines.push("no routes; start a lane to add its hostnames".into());
    } else if cx.agent() {
        for item in items {
            let mut line = format!(
                "{} {} -> {}:{} port={}",
                text(item, "state"),
                text(item, "host"),
                text(item, "lane"),
                text(item, "unit"),
                display_value(&item["port"])
            );
            if item.get("fallback").and_then(Value::as_bool) == Some(true) {
                line.push_str(" fallback");
            }
            lines.push(line);
        }
    } else {
        let mut rows = vec![
            ["HOST", "", "TARGET", "PORT", "STATE"]
                .map(str::to_owned)
                .to_vec(),
        ];
        for item in items {
            let mut host = text(item, "host");
            if item.get("fallback").and_then(Value::as_bool) == Some(true) {
                host.push_str(" (fallback)");
            }
            rows.push(vec![
                host,
                "->".into(),
                format!("{}:{}", text(item, "lane"), text(item, "unit")),
                display_value(&item["port"]),
                cx.state(&text(item, "state")),
            ]);
        }
        lines.extend(table(&rows, ""));
    }
    if let Some(proxy) = data.get("proxy").filter(|proxy| proxy.is_object()) {
        let listener = |name: &str| -> Option<String> {
            let entry = proxy.get(name)?;
            Some(
                if entry.get("bound").and_then(Value::as_bool) == Some(true) {
                    let mut text = format!("{name} :{}", display_value(&entry["port"]));
                    if let Some(reason) = entry.get("fallback_reason").and_then(Value::as_str) {
                        text.push_str(&format!(" ({})", clean(reason)));
                    }
                    text
                } else {
                    let why = entry
                        .get("error")
                        .or_else(|| entry.get("reason"))
                        .map(display_value)
                        .unwrap_or_else(|| "not bound".into());
                    format!("{name} off ({})", why.replace('_', " "))
                },
            )
        };
        let parts: Vec<String> = ["http", "https"].into_iter().filter_map(listener).collect();
        if !parts.is_empty() {
            lines.push(format!("proxy: {}", parts.join(", ")));
        }
    }
    lines.extend(route_warnings(data).iter().map(|line| cx.warning(line)));
    lines
}

/// `host_conflict` and `host_rejected` warnings, then the entries the proxy
/// dropped, one line each without a marker (`hosts` and `doctor` add their own).
pub(super) fn route_warnings(data: &Value) -> Vec<String> {
    let mut lines: Vec<String> = list(data, "warnings")
        .iter()
        .chain(list(data, "route_warnings").iter())
        .map(|warning| words::lane_warning(&text(warning, "code"), &warning["data"]))
        .collect();
    for rejected in list(&data["proxy"], "rejected") {
        lines.push(format!(
            "proxy dropped {} ({}:{}): {}",
            text(rejected, "host"),
            text(rejected, "lane"),
            text(rejected, "unit"),
            text(rejected, "reason")
        ));
    }
    lines
}
