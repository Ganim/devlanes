use std::{io::IsTerminal, path::PathBuf};

use clap::ValueEnum;
use devlanes_proto::{Actor, ActorKind};

use crate::{CiKind, Cli, Error};

pub const DEFAULT_BUDGET: usize = 16 * 1024;

/// Explicit rendering policy, independent of request semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    /// Colors and symbols, for a terminal
    Human,
    /// The same information without color, one line per event
    Plain,
    /// Terse lines for AI agents: failures first, with next commands, size-capped
    Agent,
    /// The daemon's data as JSON, or NDJSON for streams
    Json,
}

impl OutputMode {
    /// Stable spelling used by doctor and environment overrides.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Plain => "plain",
            Self::Agent => "agent",
            Self::Json => "json",
        }
    }
}

/// Injectable terminal and detection signals avoid tests mutating process globals.
#[derive(Debug, Clone, Default)]
pub struct Signals {
    pub stdout_tty: bool,
    pub stdin_tty: bool,
    pub env_output: Option<String>,
    pub agent_disabled: bool,
    pub detected_actor: Option<Actor>,
    pub no_color: bool,
    pub force_color: bool,
    pub ci: Option<CiKind>,
    pub summary_path: Option<PathBuf>,
    /// `DEVLANES_NO_UI` is set: starts print CLI output instead of opening the TUI.
    pub no_ui: bool,
    /// `DEVLANES_VERBOSE`: 1 or 2, like `-v` and `-vv`.
    pub env_verbose: u8,
    /// The console cannot render escape sequences (Windows' legacy console
    /// refused VT processing): a terminal gets plain output
    /// without color instead of human output. Set by the binary, which owns the
    /// console calls.
    pub no_escapes: bool,
}

impl Signals {
    /// Calls is-ai-agent 0.7's detection API exactly once per invocation.
    pub fn detect() -> Self {
        let agent_disabled = std::env::var("DEVLANES_AGENT").as_deref() == Ok("0");
        let detected_actor = if agent_disabled {
            None
        } else {
            is_ai_agent::detect().map(|agent| Actor {
                kind: ActorKind::Agent,
                agent_id: Some(agent.id.as_str().into()),
                session_id: agent.session_id,
            })
        };
        Self {
            stdout_tty: std::io::stdout().is_terminal(),
            stdin_tty: std::io::stdin().is_terminal(),
            env_output: std::env::var("DEVLANES_OUTPUT").ok(),
            agent_disabled,
            detected_actor,
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            force_color: std::env::var("CLICOLOR_FORCE").is_ok_and(|v| !v.is_empty() && v != "0"),
            ci: None,
            summary_path: None,
            no_ui: std::env::var("DEVLANES_NO_UI").is_ok_and(|v| !matches!(v.as_str(), "" | "0")),
            env_verbose: std::env::var("DEVLANES_VERBOSE")
                .ok()
                .and_then(|v| v.trim().parse::<u8>().ok())
                .unwrap_or(0),
            no_escapes: false,
        }
        .with_ci_env(|key| std::env::var(key).ok())
    }

    /// Captures only CI markers and the summary path; tests inject a map-backed lookup.
    pub fn with_ci_env(mut self, lookup: impl Fn(&str) -> Option<String>) -> Self {
        self.ci = CiKind::detect_with(&lookup);
        self.summary_path = lookup("GITHUB_STEP_SUMMARY")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        self
    }
}

/// Resolved once and reused for every request, renderer, and terminal decision.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub mode: OutputMode,
    pub reason: &'static str,
    pub actor: Actor,
    pub stdout_tty: bool,
    pub stdin_tty: bool,
    pub color: bool,
    pub budget: usize,
    pub quiet: bool,
    pub ci: Option<CiKind>,
    pub summary_path: Option<PathBuf>,
    /// CI log-group state shared by every reporter of this invocation.
    pub ci_state: crate::CiShared,
    /// `DEVLANES_NO_UI` turned off the TUI that starts open by default.
    pub no_ui: bool,
    /// Diagnostics on stderr: 0 none, 1 debug, 2 trace (`-v`, `-vv`,
    /// `DEVLANES_VERBOSE`). Independent of the output mode.
    pub verbose: u8,
}

impl Runtime {
    /// Applies explicit output, JSON, environment, agent, CI, then terminal precedence.
    pub fn resolve(cli: &Cli, signals: Signals) -> Result<Self, Error> {
        let actor = if signals.agent_disabled {
            Actor::default()
        } else {
            signals.detected_actor.unwrap_or_default()
        };
        let (mode, reason) = if let Some(mode) = cli.output {
            (mode, "--output")
        } else if cli.json {
            (OutputMode::Json, "--json")
        } else if let Some(value) = signals.env_output.as_deref() {
            (
                OutputMode::from_str(value, false).map_err(Error::Usage)?,
                "DEVLANES_OUTPUT",
            )
        } else if actor.kind == ActorKind::Agent {
            (OutputMode::Agent, "agent detection")
        } else if signals.ci.is_some() {
            (OutputMode::Plain, "CI detection")
        } else if signals.stdout_tty && signals.no_escapes {
            (
                OutputMode::Plain,
                "the console cannot render escape sequences",
            )
        } else if signals.stdout_tty {
            (OutputMode::Human, "stdout is a terminal")
        } else {
            (OutputMode::Plain, "stdout is not a terminal")
        };
        Ok(Self {
            mode,
            reason,
            actor,
            stdout_tty: signals.stdout_tty,
            stdin_tty: signals.stdin_tty,
            color: !signals.no_color
                && !signals.no_escapes
                && (mode == OutputMode::Human && signals.stdout_tty && signals.ci.is_none()
                    || matches!(mode, OutputMode::Human | OutputMode::Plain)
                        && (signals.force_color || signals.ci.is_some_and(CiKind::supports_ansi))),
            budget: cli.budget.unwrap_or(DEFAULT_BUDGET),
            quiet: cli.quiet,
            ci: signals.ci,
            summary_path: signals.summary_path,
            ci_state: crate::CiShared::default(),
            no_ui: signals.no_ui,
            verbose: cli.verbose.max(signals.env_verbose).min(2),
        })
    }

    /// Whether a start can run inside the TUI: human output with stdin and
    /// stdout on a terminal, outside CI. Agent, JSON and plain output never can.
    pub fn can_open_ui(&self) -> bool {
        self.mode == OutputMode::Human && self.stdout_tty && self.stdin_tty && self.ci.is_none()
    }

    /// A required confirmation fails with usage status when prompting is unavailable.
    pub fn require_confirmation(&self, yes: bool) -> Result<(), Error> {
        if yes || self.can_prompt() {
            Ok(())
        } else {
            Err(Error::Usage("confirmation required; add --yes".into()))
        }
    }

    /// Clients must check this before reading interactive confirmation or passwords.
    pub fn can_prompt(&self) -> bool {
        self.stdin_tty && self.mode != OutputMode::Agent && self.ci.is_none()
    }

    /// Describes the actual mode while preserving CI attribution under explicit overrides.
    pub fn output_description(&self) -> String {
        if let Some(ci) = self.ci {
            format!("{} (CI: {})", self.mode.as_str(), ci.as_str())
        } else {
            self.mode.as_str().into()
        }
    }
}
