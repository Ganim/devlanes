use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use devlanes_proto::{Event, LogLine, LogQuery, Method, Request, RpcError, Selection};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Rendered, Report, Runtime, UnitFailure};

/// Additive log parameters pending promotion into proto. `start` is resolved by the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneLogQuery {
    #[serde(flatten)]
    pub query: LogQuery,
    #[serde(default)]
    pub all: bool,
}

/// Keeps the client's environment in memory only; deliberately does not implement Debug.
pub struct WithPlan {
    pub start: Request,
    pub command: Vec<String>,
    pub logs_dir: Option<PathBuf>,
    pub keep: bool,
}

/// Ownership must be recorded before awaiting readiness, including partially failed starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WithOwnership {
    pub lane: String,
    /// Canonical unit references created by this invocation, excluding already-running units.
    pub units: Vec<String>,
    /// True only when this invocation brought up the whole previously idle lane.
    pub whole_lane: bool,
    pub daemon_spawned: bool,
}

/// A failed readiness check is a normal start outcome, not a transport error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WithStart {
    pub ready: bool,
    pub failures: Vec<UnitFailure>,
}

/// Peaks must come from run history, never from the latest instantaneous sample.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourcePeak {
    pub unit: String,
    pub peak_memory_bytes: Option<u64>,
    pub peak_cpu_percent: Option<f64>,
}

/// Logs grouped by unit include empty files for units without output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitLogs {
    pub unit: String,
    pub lines: Vec<LogLine>,
}

#[derive(Debug, Clone, Copy)]
pub enum CommandExit {
    Code(u8),
    Signal(u8),
}

impl CommandExit {
    /// Unix shell convention preserves the foreground command's signal status.
    pub fn code(self) -> u8 {
        match self {
            Self::Code(code) => code,
            Self::Signal(signal) => 128u8.saturating_add(signal),
        }
    }
}

/// Transport and foreground process operations supplied by the composition root.
#[async_trait]
pub trait WithDriver: Send {
    /// Streams phases, waits for readiness, and records ownership even when returning Err.
    /// Retain request context so subsequent lane queries use the same host/project.
    async fn start(
        &mut self,
        request: &Request,
        events: &mut (dyn FnMut(Event) + Send),
    ) -> Result<WithStart, RpcError>;
    /// Returns the partial-start ownership journal; must exclude pre-existing units.
    fn ownership(&self) -> WithOwnership;
    /// Returns the lane environment, including DEVLANES_PUBLIC_URL_<SERVICE> and CA variables.
    async fn env(&mut self, lane: &str) -> Result<BTreeMap<String, String>, RpcError>;
    /// Returns every page of run logs, not a presentation-budgeted tail.
    async fn logs(&mut self, lane: &str, query: LaneLogQuery) -> Result<Vec<UnitLogs>, RpcError>;
    /// Returns per-unit peaks for the current lane start; absent samples remain None.
    async fn metrics(&mut self, lane: &str) -> Result<Vec<ResourcePeak>, RpcError>;
    /// Stops only this selection in the specified canonical lane.
    async fn stop(&mut self, lane: &str, selection: Selection) -> Result<(), RpcError>;
    /// Called only when ownership reports an auto-spawned daemon.
    async fn stop_daemon_if_spawned(&mut self) -> Result<(), RpcError>;
    /// Inherit stdio, forward Ctrl-C to the child first, await exit, and report its signal/code.
    /// Do not cancel this future on Ctrl-C: the orchestrator must finish teardown afterwards.
    async fn spawn_command(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<CommandExit, RpcError>;
}

/// Exit status and cleanup errors are separate so teardown cannot mask the test command's code.
#[derive(Debug, Default)]
pub struct WithOutcome {
    pub exit_code: u8,
    pub cleanup_errors: Vec<String>,
    pub log_files: Vec<PathBuf>,
}

/// Runs the full lifecycle. Await to completion; cancellation by dropping skips async cleanup.
/// The output callback must not panic and should record write errors rather than aborting cleanup.
pub async fn run_with<D: WithDriver + ?Sized>(
    plan: &WithPlan,
    runtime: &Runtime,
    driver: &mut D,
    emit: &mut (dyn FnMut(Rendered) + Send),
) -> WithOutcome {
    let reporter = runtime.reporter();
    let mut result = WithOutcome {
        exit_code: 1,
        ..WithOutcome::default()
    };
    let start = driver
        .start(&plan.start, &mut |event| {
            if let Ok(rendered) = reporter.event(Method::Start, &event) {
                emit(rendered);
            }
        })
        .await;
    let ownership = driver.ownership();
    let mut failures = Vec::new();
    match start {
        Ok(start) if start.ready && start.failures.is_empty() => {
            // A terminal start result closes any still-open CI phase before test output begins.
            if let Ok(rendered) = reporter.result(&Report::new(Method::Start, &json!({}))) {
                emit(rendered);
            }
            match driver.env(&ownership.lane).await {
                Ok(lane_env) => {
                    let mut env = plan.start.context.env.clone().unwrap_or_default();
                    env.extend(lane_env);
                    match driver
                        .spawn_command(&plan.command, &env, &plan.start.context.cwd)
                        .await
                    {
                        Ok(status) => result.exit_code = status.code(),
                        Err(error) => emit(reporter.error(&error, None)),
                    }
                }
                Err(error) => emit(reporter.error(&error, None)),
            }
        }
        Ok(start) => {
            failures = start.failures;
            if failures.is_empty() {
                emit(reporter.error(
                    &RpcError::new("start_failed", json!({"lane": ownership.lane}), 1),
                    None,
                ));
            }
        }
        Err(error) => emit(reporter.error(&error, None)),
    }
    // A start or command failure cannot bypass artifacts, metrics, or owned-resource cleanup.
    let mut logs = Vec::new();
    if !ownership.lane.is_empty() && (plan.logs_dir.is_some() || !failures.is_empty()) {
        match driver
            .logs(
                &ownership.lane,
                LaneLogQuery {
                    all: true,
                    query: LogQuery {
                        since: Some("start".into()),
                        ..LogQuery::default()
                    },
                },
            )
            .await
        {
            Ok(found) => logs = found,
            Err(error) => {
                result.cleanup_errors.push(error.code.clone());
                emit(reporter.error(&error, None));
            }
        }
    }
    for failure in &mut failures {
        if failure.log_tail.is_empty()
            && let Some(unit) = logs.iter().find(|logs| {
                logs.unit == failure.unit
                    || logs.unit == format!("{}:{}", failure.lane, failure.unit)
            })
        {
            failure.log_tail = unit
                .lines
                .iter()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|line| line.text.clone())
                .collect();
        }
    }
    let failure_data = json!({"items": failures.iter().map(|f| json!({
        "name": f.unit, "lane": f.lane, "state": "failed", "exit_code": f.exit_code,
        "pid": null, "ports": {}, "urls": [], "actor": {"kind":"human"}, "reason": null,
        "log_tail": f.log_tail,
    })).collect::<Vec<_>>(), "truncated": false, "next_cursor": null});
    if let Ok(rendered) = reporter.result(&Report::new(Method::Start, &failure_data)) {
        emit(rendered);
    }
    if let Some(directory) = &plan.logs_dir {
        match write_log_artifacts(directory, &logs).await {
            Ok(files) => result.log_files = files,
            Err(error) => record_io(&mut result, emit, error),
        }
    }
    let mut peaks = if ownership.lane.is_empty() {
        Vec::new()
    } else {
        match driver.metrics(&ownership.lane).await {
            Ok(peaks) => peaks,
            Err(error) => {
                result.cleanup_errors.push(error.code.clone());
                emit(reporter.error(&error, None));
                ownership
                    .units
                    .iter()
                    .map(|unit| ResourcePeak {
                        unit: unit.clone(),
                        ..ResourcePeak::default()
                    })
                    .collect()
            }
        }
    };
    for unit in &ownership.units {
        if !peaks
            .iter()
            .any(|peak| peak.unit == *unit || format!("{}:{}", ownership.lane, peak.unit) == *unit)
        {
            peaks.push(ResourcePeak {
                unit: unit.clone(),
                ..ResourcePeak::default()
            });
        }
    }
    let summary = resource_summary(&peaks);
    let stdout = if runtime.mode == crate::OutputMode::Json {
        format!(
            "{}\n",
            json!({"event":"with.summary", "exit_code":result.exit_code,"items":peaks})
        )
    } else {
        summary.clone()
    };
    emit(Rendered {
        stdout,
        ..Rendered::default()
    });
    if let Err(error) = runtime
        .append_ci_summary(&format!(
            "## devlanes with\n\nExit code: {}\n\n{}",
            result.exit_code, summary
        ))
        .await
    {
        record_io(&mut result, emit, error);
    }
    if !plan.keep {
        if !ownership.lane.is_empty() && (ownership.whole_lane || !ownership.units.is_empty()) {
            let selection = Selection {
                units: if ownership.whole_lane {
                    Vec::new()
                } else {
                    ownership.units.clone()
                },
                ..Selection::default()
            };
            if let Err(error) = driver.stop(&ownership.lane, selection).await {
                result.cleanup_errors.push(error.code.clone());
                emit(reporter.error(&error, None));
            }
        }
        if ownership.daemon_spawned
            && let Err(error) = driver.stop_daemon_if_spawned().await
        {
            result.cleanup_errors.push(error.code.clone());
            emit(reporter.error(&error, None));
        }
    }
    result
}

/// Implements stop --logs-dir after the caller resolves the request's canonical lane.
/// Artifact failures never prevent the explicit stop; this never shuts down the daemon.
pub async fn stop_with_logs<D: WithDriver + ?Sized>(
    lane: &str,
    selection: Selection,
    logs_dir: &Path,
    runtime: &Runtime,
    driver: &mut D,
    emit: &mut (dyn FnMut(Rendered) + Send),
) -> WithOutcome {
    let reporter = runtime.reporter();
    let mut outcome = WithOutcome::default();
    match driver
        .logs(
            lane,
            LaneLogQuery {
                all: true,
                query: LogQuery {
                    since: Some("start".into()),
                    ..LogQuery::default()
                },
            },
        )
        .await
    {
        Ok(logs) => match write_log_artifacts(logs_dir, &logs).await {
            Ok(files) => outcome.log_files = files,
            Err(error) => record_io(&mut outcome, emit, error),
        },
        Err(error) => {
            outcome.cleanup_errors.push(error.code.clone());
            emit(reporter.error(&error, None));
        }
    }
    let peaks = match driver.metrics(lane).await {
        Ok(peaks) => peaks,
        Err(error) => {
            outcome.cleanup_errors.push(error.code.clone());
            emit(reporter.error(&error, None));
            selection
                .units
                .iter()
                .map(|unit| ResourcePeak {
                    unit: unit.clone(),
                    ..ResourcePeak::default()
                })
                .collect()
        }
    };
    let summary = resource_summary(&peaks);
    emit(Rendered {
        stdout: if runtime.mode == crate::OutputMode::Json {
            format!("{}\n", json!({"event":"stop.summary", "items":peaks}))
        } else {
            summary.clone()
        },
        ..Rendered::default()
    });
    if let Err(error) = runtime
        .append_ci_summary(&format!("## devlanes stop\n\n{summary}"))
        .await
    {
        record_io(&mut outcome, emit, error);
    }
    if let Err(error) = driver.stop(lane, selection).await {
        outcome.exit_code = error.exit_code;
        outcome.cleanup_errors.push(error.code.clone());
        emit(reporter.error(&error, None));
    }
    if outcome.exit_code == 0 && !outcome.cleanup_errors.is_empty() {
        outcome.exit_code = 1;
    }
    outcome
}

/// Reports filesystem failures without skipping subsequent cleanup or changing command status.
fn record_io(
    result: &mut WithOutcome,
    emit: &mut (dyn FnMut(Rendered) + Send),
    error: std::io::Error,
) {
    result.cleanup_errors.push(error.to_string());
    emit(Rendered {
        stderr: format!("error: {error}\n"),
        exit_code: 1,
        ..Rendered::default()
    });
}

/// Reversible byte escaping avoids path traversal and collisions between punctuation variants.
pub fn log_filename(unit: &str) -> String {
    let mut name = String::from("unit-");
    for byte in unit.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("_{byte:02x}"));
        }
    }
    name.push_str(".log");
    name
}

/// Atomically replaces each unit's artifact so repeated runs do not retain stale logs.
pub async fn write_log_artifacts(
    directory: &Path,
    logs: &[UnitLogs],
) -> std::io::Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(directory).await?;
    let mut files = Vec::new();
    let mut first_error = None;
    for unit in logs {
        let path = directory.join(log_filename(&unit.unit));
        let lines = unit.lines.clone();
        let target = path.clone();
        let parent = directory.to_path_buf();
        let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            // A sibling tempfile avoids following an existing artifact symlink during writes.
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            for line in &lines {
                let text = strip_ansi_escapes::strip_str(&line.text);
                file.write_all(text.as_bytes())?;
                if !text.ends_with('\n') {
                    file.write_all(b"\n")?;
                }
            }
            file.flush()?;
            file.persist(target).map_err(|error| error.error)?;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|result| result);
        match written {
            Ok(()) => files.push(path),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(files)
    }
}

/// Markdown is readable as plain text and can be appended directly to CI step summaries.
pub fn resource_summary(peaks: &[ResourcePeak]) -> String {
    let mut result = String::from("| Unit | Peak memory | Peak CPU |\n| --- | ---: | ---: |\n");
    for peak in peaks {
        let name = strip_ansi_escapes::strip_str(&peak.unit)
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('|', "&#124;")
            .replace(['\r', '\n'], " ");
        let memory = peak
            .peak_memory_bytes
            .map(|v| format!("{:.1} MiB", v as f64 / 1_048_576.0))
            .unwrap_or_else(|| "–".into());
        let cpu = peak
            .peak_cpu_percent
            .filter(|v| v.is_finite())
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "–".into());
        result.push_str(&format!("| {name} | {memory} | {cpu} |\n"));
    }
    result
}
