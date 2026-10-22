//! mise environments: runs `mise env --json` in the lane worktree when it has a
//! mise config, and adds the result (tool `PATH`, `[env]` values) to every unit.
//!
//! The command runs with the client environment, so mise prepends its tool
//! directories to the client's `PATH`. Results are cached per lane resolution.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ContribCx, Contributor, LaunchDraft},
    env,
};
use devlanes_sys::NoWindow;

/// Files that make a directory a mise project
/// (<https://mise.jdx.dev/configuration.html>).
pub const CONFIG_FILES: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    ".mise.local.toml",
    ".mise/config.toml",
    "mise/config.toml",
    ".config/mise.toml",
    ".config/mise/config.toml",
    ".tool-versions",
];

/// Parse failures of `mise env --json` output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MiseError {
    #[error("mise env printed invalid JSON: {0}")]
    Json(String),
    #[error("mise env printed a non-object JSON document")]
    NotObject,
}

/// Parses `mise env --json`: a flat object of variable names to strings.
/// Non-string scalars are rendered as text; errors never include values.
pub fn parse_env_json(stdout: &[u8]) -> Result<BTreeMap<String, String>, MiseError> {
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|e| MiseError::Json(format!("line {} column {}", e.line(), e.column())))?;
    let serde_json::Value::Object(map) = value else {
        return Err(MiseError::NotObject);
    };
    Ok(map
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(text) => Some((key, text)),
            serde_json::Value::Null => None,
            other => Some((key, other.to_string())),
        })
        .collect())
}

/// Returns the first mise config file found directly in `dir`.
pub async fn find_config(dir: &Path) -> Option<PathBuf> {
    for file in CONFIG_FILES {
        let path = dir.join(file);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Some(path);
        }
    }
    None
}

/// Outcome cached per lane.
enum Outcome {
    Env(BTreeMap<String, String>),
    Note(String),
    Skip,
}

/// Contributor running `mise env --json`.
#[derive(Debug, Clone)]
pub struct MiseContributor {
    program: PathBuf,
    timeout: Duration,
}

impl Default for MiseContributor {
    /// Uses `mise` from `PATH` with a 15 second limit (tool installs are not
    /// triggered by `mise env`, so it is normally fast).
    fn default() -> Self {
        Self::new("mise", Duration::from_secs(15))
    }
}

impl MiseContributor {
    /// Uses `program` (a name looked up on `PATH`, or a path) with `timeout`.
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }

    /// Runs mise for `lane` with `env`, mapping expected failures to notes.
    async fn load(&self, lane: &Path, env: &BTreeMap<String, String>) -> Result<Outcome, Error> {
        if find_config(lane).await.is_none() {
            return Ok(Outcome::Skip);
        }
        let mut command = tokio::process::Command::new(&self.program);
        command
            .no_window()
            .args(["env", "--json"])
            .current_dir(lane)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Outcome::Note(
                    "mise is not installed; its environment was skipped".into(),
                ));
            }
            Err(e) => return Err(Error::Integration(format!("cannot run mise: {e}"))),
        };
        // Dropping the child on timeout kills it (kill_on_drop).
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                Error::Integration(format!("mise env timed out after {:?}", self.timeout))
            })?
            .map_err(|e| Error::Integration(format!("mise env failed: {e}")))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            if stderr.contains("not trusted") {
                return Ok(Outcome::Note(format!(
                    "mise config in {} is not trusted; run `mise trust` there",
                    lane.display()
                )));
            }
            return Err(Error::Integration(format!(
                "mise env exited with {}: {}",
                output.status,
                first_line(&stderr)
            )));
        }
        parse_env_json(&output.stdout)
            .map(Outcome::Env)
            .map_err(|e| Error::Integration(e.to_string()))
    }
}

/// First non-empty stderr line, capped, for error messages.
fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.chars().take(300).collect()
}

#[async_trait]
impl Contributor for MiseContributor {
    fn id(&self) -> &'static str {
        "env-mise"
    }

    fn priority(&self) -> i32 {
        10
    }

    /// Adds mise's variables; untrusted configs and a missing binary become notes.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        let key = format!("env-mise:{}", cx.lane_path.display());
        let outcome = match cx.memo.get::<Outcome>(&key) {
            Some(outcome) => outcome,
            None => {
                let outcome = Arc::new(self.load(cx.lane_path, cx.client_env).await?);
                cx.memo.insert(key, outcome.clone());
                outcome
            }
        };
        match outcome.as_ref() {
            Outcome::Env(vars) => {
                // Through `env::insert` so mise's `PATH` replaces an earlier
                // contributor's `Path` on Windows.
                for (key, value) in vars {
                    env::insert(&mut draft.env, key.clone(), value.clone());
                }
            }
            Outcome::Note(note) => draft.notes.push(note.clone()),
            Outcome::Skip => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Captured `mise env --json` output (mise 2026.10) parses to strings.
    fn parses_captured_output() {
        let stdout = br#"{"PATH":"/home/dev/.local/share/mise/installs/node/24.11.0/bin:/usr/bin:/bin","NODE_ENV":"development","RETRIES":3,"EMPTY":null}"#;
        let env = parse_env_json(stdout).expect("parses");
        assert_eq!(env["NODE_ENV"], "development");
        assert_eq!(env["RETRIES"], "3");
        assert!(!env.contains_key("EMPTY"));
        assert!(env["PATH"].starts_with("/home/dev/.local/share/mise"));
        assert_eq!(parse_env_json(b"[]"), Err(MiseError::NotObject));
        assert!(matches!(parse_env_json(b"{"), Err(MiseError::Json(_))));
    }
}
