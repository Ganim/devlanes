//! Launch contributors, secrets providers and `run_script` runners.
//!
//! Lane resolution calls these while it plans a process launch. They return data
//! only; the daemon spawns processes and writes files. Integrations implement the
//! traits and the binary registers them.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::Error;

/// `ca_env` key holding the path of the devlanes root certificate (PEM).
/// Runtimes that append roots, such as Node's `NODE_EXTRA_CA_CERTS`, point here.
pub const CA_CERT_ENV: &str = "DEVLANES_CA_CERT";

/// `ca_env` key holding the path of a bundle of the system roots plus the devlanes
/// root. Runtimes that replace their roots, such as Python's `SSL_CERT_FILE`, point here.
pub const CA_BUNDLE_ENV: &str = "DEVLANES_CA_BUNDLE";

/// Static service facts available before any expression is evaluated.
#[derive(Debug, Clone, Copy)]
pub struct ServiceView<'a> {
    pub name: &'a str,
    /// Explicit `platform`; inference happens later, at launch planning.
    pub platform: Option<&'a str>,
    pub preset: Option<&'a str>,
    pub tags: &'a [String],
}

/// Kind of process being planned. Contributors that only make sense for
/// long-running services check it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Service,
    Task,
    /// `infra.setup` or `infra.teardown` entry.
    InfraCommand,
}

/// Command before contributor arguments are appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftCommand {
    /// Shell text, run with `/bin/sh -c`.
    Shell(String),
    /// Executable and arguments, run without a shell.
    Argv(Vec<String>),
    /// `run_script` reference; a [`ScriptRunner`] renders it after contributors run.
    Script { source: PathBuf, name: String },
}

/// A file the daemon writes before spawning, such as a framework config overlay.
/// Contents must not contain secrets: the daemon writes them to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

/// Mutable launch plan shared by contributors, applied in priority order.
/// Neither `Debug` nor serializable: `env` can hold values from env managers.
pub struct LaunchDraft {
    pub command: DraftCommand,
    /// Appended to the command. Script runners place them (for example after `--`).
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Contributor layer of the environment (above the client environment).
    pub env: BTreeMap<String, String>,
    /// Variables to drop from the client environment, such as direnv unsets.
    pub remove: BTreeSet<String>,
    pub files: Vec<PreparedFile>,
    /// Non-fatal diagnostics shown with the unit, such as an untrusted mise config.
    pub notes: Vec<String>,
}

/// `run_script` facts resolved before contributors run, so presets can inspect
/// the script body (for example to reject a hardcoded `--port`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptInfo {
    /// Absolute path of the source file.
    pub source: PathBuf,
    /// Script name after profile remapping.
    pub name: String,
    pub body: Option<String>,
    /// Directory the runner executes in.
    pub cwd: PathBuf,
    /// [`ScriptRunner::id`] of the runner that handles the source.
    pub runner: &'static str,
}

/// Read-only facts for one unit (one replica of a service, a task or an infra command).
pub struct ContribCx<'a> {
    pub kind: UnitKind,
    pub project: &'a str,
    /// Service, task or infra entry name as written in the config.
    pub name: &'a str,
    /// One-based replica index for services.
    pub replica: Option<u16>,
    pub lane_index: u32,
    pub lane_name: &'a str,
    pub lane_path: &'a Path,
    pub primary_path: &'a Path,
    /// Explicit or inferred platform (`node`, `python`, ...).
    pub platform: Option<&'a str>,
    pub preset: Option<&'a str>,
    /// Preset options after expression evaluation, as JSON.
    pub preset_options: Option<&'a serde_json::Value>,
    pub port: Option<u16>,
    /// Named ports (`self.ports`), without the primary port.
    pub ports: &'a BTreeMap<String, u16>,
    pub public_url: Option<&'a str>,
    pub pty: bool,
    pub script: Option<&'a ScriptInfo>,
    /// Environment of the client that started the lane.
    pub client_env: &'a BTreeMap<String, String>,
    /// Trust variables from the CA, keyed by [`CA_CERT_ENV`] and [`CA_BUNDLE_ENV`].
    pub ca_env: &'a BTreeMap<String, String>,
    /// Per-resolution cache shared by all units of one lane.
    pub memo: &'a ContribMemo,
}

/// Confidence for a claim based on the `run_script` source file, such as a
/// `package.json` script for node. See [`Contributor::detect`].
pub const DETECT_SCRIPT_SOURCE: u8 = 90;

/// Confidence for a claim based on the platform's manifest in the unit's
/// working directory, such as `package.json` for node.
pub const DETECT_MANIFEST: u8 = 60;

/// Added to a base confidence by a platform that refines another platform's
/// manifest: a bun lockfile next to a `package.json` beats plain node.
pub const DETECT_REFINE: u8 = 5;

/// Files a platform contributor may inspect when a unit omits `platform`.
#[derive(Debug, Clone, Copy)]
pub struct DetectCx<'a> {
    pub kind: UnitKind,
    /// Service, task or infra entry name as written in the config.
    pub name: &'a str,
    /// Lane worktree root; detection must not look above it.
    pub lane_path: &'a Path,
    /// Directory the unit runs in (the script's directory for `run_script`).
    pub cwd: &'a Path,
    /// The `run_script` source, when the unit uses one.
    pub script: Option<&'a ScriptInfo>,
}

/// Cache for work that is per lane rather than per unit, such as `mise env`.
/// It lives for one lane resolution, so config edits apply at the next start.
#[derive(Default)]
pub struct ContribMemo {
    entries: Mutex<HashMap<String, Arc<dyn Any + Send + Sync>>>,
}

impl ContribMemo {
    /// Returns a cached value when `key` holds a value of type `T`.
    pub fn get<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.get(key).cloned()?.downcast::<T>().ok()
    }

    /// Stores `value` under `key`, replacing an earlier entry.
    pub fn insert<T: Any + Send + Sync>(&self, key: impl Into<String>, value: Arc<T>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(key.into(), value);
    }
}

/// Adds environment, arguments or files to a launch. Platform integrations,
/// env managers and framework presets implement it.
#[async_trait]
pub trait Contributor: Send + Sync {
    /// Stable identifier used in errors and `env --explain`.
    fn id(&self) -> &'static str;

    /// Lower priorities run first; later contributors override earlier values.
    /// Env managers use 10-20, platforms 50, presets 100.
    fn priority(&self) -> i32 {
        0
    }

    /// Named ports to allocate for `service` before expressions are evaluated,
    /// so they exist in `self.ports` (wrangler's `inspector`, for example).
    fn required_ports(&self, _service: &ServiceView<'_>) -> Vec<String> {
        Vec::new()
    }

    /// Platform name this contributor provides (`node`, `python`, ...). Only
    /// contributors that return one are asked to [`Self::detect`].
    fn platform(&self) -> Option<&'static str> {
        None
    }

    /// Claims a unit whose config omits `platform`, with a confidence. Resolution
    /// picks the highest claim; ties go to the higher [`Self::priority`], then the
    /// smaller [`Self::id`]. Use [`DETECT_SCRIPT_SOURCE`], [`DETECT_MANIFEST`] and
    /// [`DETECT_REFINE`] so claims from different crates compare sensibly.
    async fn detect(&self, _cx: &DetectCx<'_>) -> Option<u8> {
        None
    }

    /// Edits the draft. Errors fail the unit's launch; use `draft.notes` for
    /// problems that should not.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error>;
}

/// One secrets lookup for a lane worktree.
pub struct SecretsRequest<'a> {
    pub worktree: &'a Path,
    /// Provider profile (`fnox.profile`); `None` uses the provider's default.
    pub profile: Option<&'a str>,
    /// True when the config names the provider, so a missing provider config is an
    /// error rather than "no secrets".
    pub explicit: bool,
    /// Client environment, for provider credentials and `PATH`.
    pub base_env: &'a BTreeMap<String, String>,
}

/// Provider output. Values stay in memory: no `Debug`, no serialization.
#[derive(Default)]
pub struct SecretEnv {
    pub set: BTreeMap<String, String>,
    /// Variables to drop from the inherited (client) environment.
    pub remove: BTreeSet<String>,
    /// File-backed secrets: variable name to file contents. The caller decides
    /// whether and where to write them.
    pub files: BTreeMap<String, String>,
}

/// Secrets failures. Messages never include secret values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretsError {
    /// The vault needs an interactive unlock (`devlanes unlock`).
    #[error("secrets are locked: {message}")]
    Locked { message: String },
    #[error("secrets provider failed: {0}")]
    Other(String),
}

/// Resolves secrets for a lane, such as `fnox env --json`.
#[async_trait]
pub trait SecretsProvider: Send + Sync {
    /// Stable identifier used in errors and `env --explain`.
    fn id(&self) -> &'static str;

    /// Returns the variables for `request`. Returns an empty set when the provider
    /// has no config in the worktree and `request.explicit` is false.
    async fn env(&self, request: &SecretsRequest<'_>) -> Result<SecretEnv, SecretsError>;
}

/// One `run_script` invocation to render.
#[derive(Debug, Clone, Copy)]
pub struct ScriptRequest<'a> {
    /// Lane worktree root. Runners must not read above it.
    pub worktree: &'a Path,
    /// Source file, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    /// Script, target or recipe name.
    pub name: &'a str,
    /// Arguments to forward to the script, typically preset flags. Runners add
    /// whatever separator their tool needs.
    pub extra_args: &'a [String],
}

/// Rendered script command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScriptPlan {
    /// Program first.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Script text when the source defines one, for presets that inspect it.
    pub body: Option<String>,
    /// Non-fatal findings, such as a package manager picked from a lockfile
    /// because `packageManager` is missing. Lane resolution copies them into
    /// the launch notes shown by `env --explain` and `why`.
    #[serde(default)]
    pub notes: Vec<String>,
    /// One line on how the runner was chosen, such as
    /// `pnpm 10.4.1 (packageManager in package.json)`. Shown with the notes.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Renders `run_script` references for one kind of source file
/// (package.json, Makefile, justfile).
#[async_trait]
pub trait ScriptRunner: Send + Sync {
    /// Stable identifier, such as `npm` or `make`.
    fn id(&self) -> &'static str;

    /// Returns whether this runner handles `source` (usually by file name).
    fn matches(&self, source: &Path) -> bool;

    /// Renders the command. Fails when the file or script is missing. Called once
    /// without `extra_args` before contributors run and again with them when
    /// contributors added arguments.
    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// The memo returns values only for the type they were stored with.
    fn memo_is_typed() {
        let memo = ContribMemo::default();
        memo.insert("k", Arc::new(5_u32));
        assert_eq!(memo.get::<u32>("k").as_deref(), Some(&5));
        assert!(memo.get::<String>("k").is_none());
        assert!(memo.get::<u32>("missing").is_none());
    }
}
