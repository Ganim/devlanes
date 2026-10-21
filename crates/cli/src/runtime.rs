use std::{io::IsTerminal, path::PathBuf};

use clap::ValueEnum;
use devlanes_proto::{Actor, ActorKind};

use crate::{CiKind, Cli, Error};

pub const DEFAULT_BUDGET: usize = 16 * 1024;

/// Explicit rendering policy, independent of request semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    Human,
    Plain,
    Agent,
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
                && (mode == OutputMode::Human && signals.stdout_tty && signals.ci.is_none()
                    || matches!(mode, OutputMode::Human | OutputMode::Plain)
                        && (signals.force_color || signals.ci.is_some_and(CiKind::supports_ansi))),
            budget: cli.budget.unwrap_or(DEFAULT_BUDGET),
            quiet: cli.quiet,
            ci: signals.ci,
            summary_path: signals.summary_path,
        })
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
