use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use devlanes_proto::{Event, Method, events};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::{OutputMode, PlainReporter, Rendered, Report, Reporter, Runtime};

/// Provider markers are cooperative environment signals, not authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CiKind {
    Generic,
    GithubActions,
    Gitlab,
    Buildkite,
    Circleci,
    AzurePipelines,
    Jenkins,
    Teamcity,
    Bitbucket,
}

impl CiKind {
    /// Stable names for doctor output and snapshots.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::GithubActions => "github-actions",
            Self::Gitlab => "gitlab",
            Self::Buildkite => "buildkite",
            Self::Circleci => "circleci",
            Self::AzurePipelines => "azure-pipelines",
            Self::Jenkins => "jenkins",
            Self::Teamcity => "teamcity",
            Self::Bitbucket => "bitbucket",
        }
    }

    /// Only providers whose console ANSI support is part of the CLI contract opt in.
    pub fn supports_ansi(self) -> bool {
        matches!(self, Self::GithubActions | Self::Gitlab)
    }

    /// Provider markers win over generic CI; the fixed order resolves mixed environments.
    pub fn detect_with(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        // Names verified against ci-info/vendors.json and Bitbucket's variables reference.
        for (name, provider) in [
            ("GITHUB_ACTIONS", Self::GithubActions),
            ("GITLAB_CI", Self::Gitlab),
            ("BUILDKITE", Self::Buildkite),
            ("CIRCLECI", Self::Circleci),
            ("TF_BUILD", Self::AzurePipelines),
            ("JENKINS_URL", Self::Jenkins),
            ("TEAMCITY_VERSION", Self::Teamcity),
            ("BITBUCKET_BUILD_NUMBER", Self::Bitbucket),
        ] {
            if lookup(name).as_deref().is_some_and(truthy) {
                return Some(provider);
            }
        }
        lookup("CI")
            .as_deref()
            .is_some_and(truthy)
            .then_some(Self::Generic)
    }
}

/// Empty and explicit false markers do not turn a developer terminal into CI.
fn truthy(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
}

/// Client-side annotations contain at most the last twenty log lines.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitFailure {
    pub lane: String,
    pub unit: String,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub log_tail: Vec<String>,
}

/// CI formatting and summary writes never enter the daemon protocol.
#[async_trait]
pub trait CiProvider: Send + Sync {
    /// Opens a provider-specific collapsible phase, or returns no decoration.
    fn group_start(&self, name: &str) -> String;
    /// Closes a previously opened phase.
    fn group_end(&self) -> String;
    /// Returns an escaped workflow annotation for a failed unit or task.
    fn error_annotation(&self, failure: &UnitFailure) -> String;
    /// Appends markdown without replacing summaries from earlier invocations.
    async fn append_summary(&self, markdown: &str) -> std::io::Result<()>;
}

/// Unrecognized providers retain plain terminal output without workflow commands.
#[derive(Debug, Default)]
pub struct NoCiProvider;

#[async_trait]
impl CiProvider for NoCiProvider {
    /// No-op for consoles without a supported grouping protocol.
    fn group_start(&self, _name: &str) -> String {
        String::new()
    }
    /// No group was opened by this provider.
    fn group_end(&self) -> String {
        String::new()
    }
    /// Ordinary failure output remains available through the reporter.
    fn error_annotation(&self, _failure: &UnitFailure) -> String {
        String::new()
    }
    /// No summary destination is defined for this provider.
    async fn append_summary(&self, _markdown: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// The summary destination is injected, never looked up while rendering.
#[derive(Debug, Clone, Default)]
pub struct GithubActions {
    pub summary_path: Option<PathBuf>,
}

#[async_trait]
impl CiProvider for GithubActions {
    /// Escaping prevents phase names from injecting another workflow command.
    fn group_start(&self, name: &str) -> String {
        format!("::group::{}\n", escape_data(name))
    }
    /// GitHub groups are flat, so the reporter closes the active group before another starts.
    fn group_end(&self) -> String {
        "::endgroup::\n".into()
    }
    /// Mirrors actions/toolkit packages/core/src/command.ts escaping rules.
    fn error_annotation(&self, failure: &UnitFailure) -> String {
        let title = match failure.exit_code {
            Some(code) => format!("{} exited {code}", failure.unit),
            None => format!("{} failed", failure.unit),
        };
        let lines = failure.log_tail.iter().rev().take(20).collect::<Vec<_>>();
        let message = lines
            .into_iter()
            .rev()
            .map(|line| {
                // The ANSI stripper normalizes CR; preserve it for workflow-command escaping.
                line.split('\r')
                    .map(strip_ansi_escapes::strip_str)
                    .collect::<Vec<_>>()
                    .join("\r")
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "::error title={}::{}\n",
            escape_property(&title),
            escape_data(if message.is_empty() { &title } else { &message })
        )
    }
    /// Tokio keeps the summary-file append off the executor's blocking filesystem path.
    async fn append_summary(&self, markdown: &str) -> std::io::Result<()> {
        let Some(path) = &self.summary_path else {
            return Ok(());
        };
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(markdown.as_bytes()).await?;
        if !markdown.ends_with('\n') {
            file.write_all(b"\n").await?;
        }
        file.flush().await
    }
}

/// Escape percent first so the generated escape sequences are not double-escaped.
fn escape_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Colons and commas are delimiters in workflow-command properties, not message data.
fn escape_property(value: &str) -> String {
    escape_data(value).replace(':', "%3A").replace(',', "%2C")
}

#[derive(Default)]
struct CiState {
    phase: Option<(String, String)>,
    annotated: BTreeSet<(String, String)>,
}

/// Wraps the plain reporter while tracking balanced phase groups and duplicate failures.
pub struct CiPlainReporter {
    plain: PlainReporter,
    provider: Arc<dyn CiProvider>,
    state: Mutex<CiState>,
}

impl CiPlainReporter {
    /// The injected provider permits hermetic tests and future provider integrations.
    pub fn new(runtime: Runtime, provider: Arc<dyn CiProvider>) -> Self {
        Self {
            plain: PlainReporter(runtime),
            provider,
            state: Mutex::new(CiState::default()),
        }
    }

    /// Emits only one annotation per unit in a single start invocation.
    fn annotations(&self, value: &Value) -> String {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut output = String::new();
        for failure in failures(value) {
            if state
                .annotated
                .insert((failure.lane.clone(), failure.unit.clone()))
            {
                output.push_str(&self.provider.error_annotation(&failure));
            }
        }
        output
    }

    /// A terminal response closes any phase left open by an interrupted start.
    fn close_group(&self) -> String {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase.take().is_some() {
            self.provider.group_end()
        } else {
            String::new()
        }
    }
}

impl Reporter for CiPlainReporter {
    /// CI integration is exclusive to plain mode; JSON never receives command strings.
    fn mode(&self) -> OutputMode {
        OutputMode::Plain
    }
    /// Reuses detection instead of reading environment during rendering.
    fn runtime(&self) -> &Runtime {
        self.plain.runtime()
    }
    /// Annotations accompany final failures even when progress was suppressed.
    fn result(&self, report: &Report<'_>) -> Result<Rendered, serde_json::Error> {
        let mut result = self.plain.result(report)?;
        result.stdout = format!(
            "{}{}{}",
            self.close_group(),
            self.annotations(report.data),
            result.stdout
        );
        Ok(result)
    }
    /// Accepts proto's lane.phase and unit/task event shapes without daemon-side wording.
    fn event(&self, method: Method, event: &Event) -> Result<Rendered, serde_json::Error> {
        if event.event == events::RESULT {
            return self.result(&Report::new(method, &event.data));
        }
        let mut prefix = String::new();
        let mut suffix = String::new();
        if event.event == events::PHASE && !self.runtime().quiet {
            let field = |key| {
                event
                    .data
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            };
            let key = (field("lane").to_owned(), field("phase").to_owned());
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match field("state") {
                "started" | "starting" => {
                    if state.phase.as_ref() != Some(&key) {
                        if state.phase.take().is_some() {
                            prefix.push_str(&self.provider.group_end());
                        }
                        prefix.push_str(&self.provider.group_start(&key.1));
                        state.phase = Some(key);
                    }
                }
                "done" | "failed" | "skipped" if state.phase.as_ref() == Some(&key) => {
                    suffix.push_str(&self.provider.group_end());
                    state.phase = None;
                }
                _ => {}
            }
        }
        // Events without tails are annotated by the final report after the client fetches logs.
        if has_tail(&event.data) {
            prefix.push_str(&self.annotations(&event.data));
        }
        let mut result = self.plain.event(method, event)?;
        result.stdout.insert_str(0, &prefix);
        result.stdout.push_str(&suffix);
        Ok(result)
    }
}

/// Extracts failure payloads from page, lane, unit.changed and task response shapes.
pub(crate) fn failures(value: &Value) -> Vec<UnitFailure> {
    if let Some(items) = value.as_array() {
        return items.iter().flat_map(failures).collect();
    }
    for key in ["items", "units", "failures"] {
        if let Some(items) = value.get(key).and_then(Value::as_array) {
            return items.iter().flat_map(failures).collect();
        }
    }
    if let Some(unit) = value.get("unit").filter(|v| v.is_object()) {
        return failures(unit);
    }
    if value.get("state").and_then(Value::as_str) != Some("failed") {
        return Vec::new();
    }
    let Some(unit) = value
        .get("name")
        .or_else(|| value.get("unit"))
        .or_else(|| value.get("task"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    vec![UnitFailure {
        lane: value
            .get("lane")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        unit: unit.into(),
        exit_code: value
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok()),
        log_tail: value
            .get("log_tail")
            .and_then(Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|v| v.as_str().or_else(|| v.get("text").and_then(Value::as_str)))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    }]
}

/// Avoids an early empty annotation suppressing the final useful failure tail.
fn has_tail(value: &Value) -> bool {
    value
        .get("log_tail")
        .and_then(Value::as_array)
        .is_some_and(|lines| !lines.is_empty())
        || value.get("unit").is_some_and(has_tail)
}

impl Runtime {
    /// Returns a no-op provider unless plain output selected a supported CI console.
    pub fn ci_provider(&self) -> Arc<dyn CiProvider> {
        if self.mode == OutputMode::Plain && self.ci == Some(CiKind::GithubActions) {
            Arc::new(GithubActions {
                summary_path: self.summary_path.clone(),
            })
        } else {
            Arc::new(NoCiProvider)
        }
    }

    /// Call after a start/stop result; filesystem work is awaited rather than detached.
    pub async fn append_ci_summary(&self, markdown: &str) -> std::io::Result<()> {
        self.ci_provider().append_summary(markdown).await
    }
}
