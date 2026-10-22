//! direnv environments: runs `direnv export json` in the lane worktree when it
//! has an allowed `.envrc`, and applies the diff to every unit.
//!
//! direnv computes the diff against the environment it runs in, so it runs with
//! the client environment: variables direnv unsets are removed from the client
//! layer, the rest are set at the contributor layer. direnv's own bookkeeping
//! variables (`DIRENV_*`) are dropped so shells inside units do not think the
//! `.envrc` is already loaded. A blocked `.envrc` is a note, never an error:
//! devlanes does not allow files on the user's behalf.

use std::{
    collections::{BTreeMap, BTreeSet},
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

/// The file direnv loads.
pub const ENVRC: &str = ".envrc";

/// Changes reported by `direnv export json`.
#[derive(Default, PartialEq, Eq)]
pub struct DirenvDiff {
    pub set: BTreeMap<String, String>,
    pub unset: BTreeSet<String>,
}

/// Parse failures of `direnv export json` output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirenvError {
    #[error("direnv export printed invalid JSON: {0}")]
    Json(String),
    #[error("direnv export printed a non-object JSON document")]
    NotObject,
}

/// Parses `direnv export json`: an object of names to a string (set) or `null`
/// (unset). Empty output means nothing changed. Drops `DIRENV_*` variables.
pub fn parse_export_json(stdout: &[u8]) -> Result<DirenvDiff, DirenvError> {
    let mut diff = DirenvDiff::default();
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(diff);
    }
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|e| DirenvError::Json(format!("line {} column {}", e.line(), e.column())))?;
    let serde_json::Value::Object(map) = value else {
        return Err(DirenvError::NotObject);
    };
    for (key, value) in map {
        if key.starts_with("DIRENV_") {
            continue;
        }
        match value {
            serde_json::Value::Null => {
                diff.unset.insert(key);
            }
            serde_json::Value::String(text) => {
                diff.set.insert(key, text);
            }
            other => {
                diff.set.insert(key, other.to_string());
            }
        }
    }
    Ok(diff)
}

/// Outcome cached per lane.
enum Outcome {
    Diff(DirenvDiff),
    Note(String),
    Skip,
}

/// Contributor running `direnv export json`.
#[derive(Debug, Clone)]
pub struct DirenvContributor {
    program: PathBuf,
    timeout: Duration,
}

impl Default for DirenvContributor {
    /// Uses `direnv` from `PATH` with a 15 second limit.
    fn default() -> Self {
        Self::new("direnv", Duration::from_secs(15))
    }
}

impl DirenvContributor {
    /// Uses `program` (a name looked up on `PATH`, or a path) with `timeout`.
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }

    /// Runs direnv for `lane` with `env`, mapping expected failures to notes.
    async fn load(&self, lane: &Path, env: &BTreeMap<String, String>) -> Result<Outcome, Error> {
        if !tokio::fs::try_exists(lane.join(ENVRC))
            .await
            .unwrap_or(false)
        {
            return Ok(Outcome::Skip);
        }
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(["export", "json"])
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
                    "direnv is not installed; .envrc was skipped".into(),
                ));
            }
            Err(e) => return Err(Error::Integration(format!("cannot run direnv: {e}"))),
        };
        // Dropping the child on timeout kills it (kill_on_drop).
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                Error::Integration(format!("direnv export timed out after {:?}", self.timeout))
            })?
            .map_err(|e| Error::Integration(format!("direnv export failed: {e}")))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        // direnv reports a blocked file on stderr; depending on the version the
        // exit status is 0 or 1.
        if stderr.contains("is blocked") {
            return Ok(Outcome::Note(format!(
                "{} is blocked; run `direnv allow` in {}",
                ENVRC,
                lane.display()
            )));
        }
        if !output.status.success() {
            return Err(Error::Integration(format!(
                "direnv export exited with {}: {}",
                output.status,
                last_error_line(&stderr)
            )));
        }
        parse_export_json(&output.stdout)
            .map(Outcome::Diff)
            .map_err(|e| Error::Integration(e.to_string()))
    }
}

/// Last `direnv: error` line (or the last line), capped, for error messages.
/// Earlier lines are `direnv: loading` progress output.
fn last_error_line(text: &str) -> String {
    let lines: Vec<_> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let line = lines
        .iter()
        .rev()
        .find(|l| l.contains("error"))
        .or(lines.last())
        .copied()
        .unwrap_or("");
    line.chars().take(300).collect()
}

#[async_trait]
impl Contributor for DirenvContributor {
    fn id(&self) -> &'static str {
        "env-direnv"
    }

    /// After mise, so an `.envrc` that also uses mise has the last word.
    fn priority(&self) -> i32 {
        20
    }

    /// Applies the diff; blocked files and a missing binary become notes.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        let key = format!("env-direnv:{}", cx.lane_path.display());
        let outcome = match cx.memo.get::<Outcome>(&key) {
            Some(outcome) => outcome,
            None => {
                let outcome = Arc::new(self.load(cx.lane_path, cx.client_env).await?);
                cx.memo.insert(key, outcome.clone());
                outcome
            }
        };
        match outcome.as_ref() {
            Outcome::Diff(diff) => {
                // Through the `env` helpers so names match ignoring case on Windows.
                for key in &diff.unset {
                    env::remove(&mut draft.env, key);
                    draft.remove.insert(key.clone());
                }
                for (key, value) in &diff.set {
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
    /// Captured `direnv export json` output (direnv 2.37) with sets, unsets and bookkeeping.
    fn parses_captured_output() {
        let stdout = br#"{
  "DATABASE_URL": "postgres://localhost/app",
  "DIRENV_DIFF": "eJ...",
  "DIRENV_DIR": "-/work/app",
  "DIRENV_FILE": "/work/app/.envrc",
  "DIRENV_WATCHES": "eJ...",
  "OLD_FLAG": null,
  "PATH": "/work/app/bin:/usr/bin:/bin"
}"#;
        let diff = parse_export_json(stdout).expect("parses");
        assert_eq!(
            diff.set.keys().collect::<Vec<_>>(),
            ["DATABASE_URL", "PATH"]
        );
        assert!(diff.unset.contains("OLD_FLAG"));
        assert!(parse_export_json(b"\n").expect("empty").set.is_empty());
        assert!(matches!(
            parse_export_json(b"[1]"),
            Err(DirenvError::NotObject)
        ));
    }
}
