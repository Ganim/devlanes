//! fnox secrets: `fnox env --json` in the lane worktree
//! (<https://fnox.jdx.dev/reference/env-json.html>, schema 1, fnox 1.39+).
//!
//! fnox runs non-interactively because the daemon has no terminal. When a
//! provider needs a sign-in or unlock, the error document says so and the
//! result is [`SecretsError::Locked`]; `devlanes secrets unlock` then runs fnox in the
//! client's terminal. Values stay in memory: nothing here logs, prints or
//! persists them, and error messages carry fnox's message only.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use devlanes_core::contrib::{SecretEnv, SecretsError, SecretsProvider, SecretsRequest};
use devlanes_sys::NoWindow;
use serde::Deserialize;

/// Config files that make fnox apply to a worktree without an explicit `fnox:` key.
pub const CONFIG_FILES: &[&str] = &[
    "fnox.toml",
    ".fnox.toml",
    "fnox.local.toml",
    ".fnox.local.toml",
];

/// Lowercase fragments of fnox resolution errors that an interactive run fixes:
/// provider sign-ins (1Password, Bitwarden), keychain or passphrase prompts
/// refused under `--non-interactive`, and expired sessions.
const LOCKED_HINTS: &[&str] = &[
    "non-interactive",
    "interactive",
    "unlock",
    "locked",
    "sign in",
    "signin",
    "not signed",
    "authenticat",
    "session expired",
    "passphrase",
    "biometric",
    "prompt",
    "terminal",
];

/// The fnox env document; only the fields devlanes uses. No `Debug`: it holds values.
#[derive(Deserialize)]
struct Document {
    schema: u32,
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    remove: BTreeSet<String>,
    error: Option<ErrorDocument>,
}

/// `error` member of a failure document.
#[derive(Deserialize)]
struct ErrorDocument {
    kind: String,
    #[serde(default)]
    message: String,
}

/// Parses an `fnox env --json` document (success or error).
///
/// Resolution errors that mention a sign-in, unlock or prompt map to
/// [`SecretsError::Locked`]; other errors to [`SecretsError::Other`].
pub fn parse_document(stdout: &[u8]) -> Result<SecretEnv, SecretsError> {
    let document: Document = serde_json::from_slice(stdout).map_err(|e| {
        SecretsError::Other(format!(
            "fnox printed an unreadable document (line {} column {})",
            e.line(),
            e.column()
        ))
    })?;
    if document.schema != 1 {
        return Err(SecretsError::Other(format!(
            "fnox env schema {} is not supported; devlanes reads schema 1",
            document.schema
        )));
    }
    if let Some(error) = document.error {
        let message = format!("{}: {}", error.kind, cap(&error.message));
        let lower = error.message.to_lowercase();
        let locked = error.kind == "resolution" && LOCKED_HINTS.iter().any(|h| lower.contains(h));
        return Err(if locked {
            SecretsError::Locked { message }
        } else {
            SecretsError::Other(message)
        });
    }
    Ok(SecretEnv {
        set: document.set,
        remove: document.remove,
        files: document.files,
    })
}

/// Caps a message so a long provider report does not flood status output.
fn cap(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    line.chars().take(300).collect()
}

/// Returns whether `dir` holds an fnox config file.
pub async fn has_config(dir: &Path) -> bool {
    for file in CONFIG_FILES {
        if tokio::fs::try_exists(dir.join(file)).await.unwrap_or(false) {
            return true;
        }
    }
    false
}

/// [`SecretsProvider`] running the fnox CLI.
#[derive(Debug, Clone)]
pub struct FnoxProvider {
    program: PathBuf,
    timeout: Duration,
}

impl Default for FnoxProvider {
    /// Uses `fnox` from `PATH` with a 30 second limit (remote providers can be slow).
    fn default() -> Self {
        Self::new("fnox", Duration::from_secs(30))
    }
}

impl FnoxProvider {
    /// Uses `program` (a name looked up on `PATH`, or a path) with `timeout`.
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }

    /// Arguments for one request: global flags first, then `env --json`.
    pub fn args(profile: Option<&str>) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(profile) = profile {
            args.extend(["--profile".to_owned(), profile.to_owned()]);
        }
        args.extend(["env", "--json", "--non-interactive"].map(str::to_owned));
        args
    }
}

#[async_trait]
impl SecretsProvider for FnoxProvider {
    fn id(&self) -> &'static str {
        "fnox"
    }

    /// Runs fnox unless the worktree has no fnox config and the project does not
    /// name fnox explicitly.
    async fn env(&self, request: &SecretsRequest<'_>) -> Result<SecretEnv, SecretsError> {
        if !request.explicit && !has_config(request.worktree).await {
            return Ok(SecretEnv::default());
        }
        let mut command = tokio::process::Command::new(&self.program);
        command
            .no_window()
            .args(Self::args(request.profile))
            .current_dir(request.worktree)
            .env_clear()
            .envs(request.base_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SecretsError::Other(
                    "fnox is not installed (or the lane worktree is missing)".into(),
                )
            } else {
                SecretsError::Other(format!("cannot run fnox: {e}"))
            }
        })?;
        // Dropping the child on timeout kills it (kill_on_drop).
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| SecretsError::Other(format!("fnox timed out after {:?}", self.timeout)))?
            .map_err(|e| SecretsError::Other(format!("fnox failed: {e}")))?;
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            // Usage errors (exit 2) and fnox versions without `env --json` print no document.
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SecretsError::Other(format!(
                "fnox exited with {} without a document (fnox 1.39+ is required): {}",
                output.status,
                cap(&stderr)
            )));
        }
        parse_document(&output.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// A captured success document (fnox 1.40).
    fn parses_success() {
        let stdout = br#"{"schema":1,"fnox_version":"1.40.0","scope":"exec","profile":["dev"],"set":{"DATABASE_URL":"postgres://u:p@localhost/db"},"files":{"GCP_SA_JSON":"{}"},"remove":["FNOX_AGE_KEY"],"missing":["OPTIONAL"],"leases":[],"future_field":true}
"#;
        let env = parse_document(stdout).expect("parses");
        assert_eq!(env.set["DATABASE_URL"], "postgres://u:p@localhost/db");
        assert!(env.remove.contains("FNOX_AGE_KEY"));
        assert!(env.files.contains_key("GCP_SA_JSON"));
    }

    #[test]
    /// Sign-in failures are locked; config errors and other schemas are not.
    fn classifies_errors() {
        let locked = br#"{"schema":1,"error":{"kind":"resolution","message":"1Password: not signed in; run `op signin` or remove --non-interactive"}}"#;
        assert!(matches!(
            parse_document(locked),
            Err(SecretsError::Locked { message }) if message.starts_with("resolution: 1Password")
        ));
        let config = br#"{"schema":1,"error":{"kind":"config","message":"unknown profile `dev`"}}"#;
        assert_eq!(
            parse_document(config).err(),
            Some(SecretsError::Other("config: unknown profile `dev`".into()))
        );
        let provider =
            br#"{"schema":1,"error":{"kind":"resolution","message":"vault: connection refused"}}"#;
        assert!(matches!(
            parse_document(provider),
            Err(SecretsError::Other(_))
        ));
        assert!(matches!(
            parse_document(br#"{"schema":2,"set":{}}"#),
            Err(SecretsError::Other(m)) if m.contains("schema 2")
        ));
        assert!(matches!(
            parse_document(b"not json"),
            Err(SecretsError::Other(_))
        ));
    }

    #[test]
    /// Global flags go before the subcommand.
    fn argument_order() {
        assert_eq!(
            FnoxProvider::args(Some("dev")),
            ["--profile", "dev", "env", "--json", "--non-interactive"]
        );
        assert_eq!(
            FnoxProvider::args(None),
            ["env", "--json", "--non-interactive"]
        );
    }
}
