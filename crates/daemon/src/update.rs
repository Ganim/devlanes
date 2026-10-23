//! Release checks. A background task reads the latest release's version once a
//! day, caches it in `$DEVLANES_HOME/update-check.json`, and `daemon.status` and
//! `status` report it once it is newer than this daemon and has been out for a
//! while. Nothing is downloaded or installed, and nothing restarts.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use devlanes_proto::release::{ReleaseCheck, UpdateInfo, Version, is_newer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::Inner;

/// The latest published release's manifest; GitHub's `latest` skips drafts and prereleases.
pub const MANIFEST_URL: &str =
    "https://github.com/devlanes-run/devlanes/releases/latest/download/release-manifest.json";

/// The cache file under `$DEVLANES_HOME`.
const CACHE_FILE: &str = "update-check.json";

/// Upper bound on the manifest; the real one is under 2 KiB.
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// When and where the daemon looks for releases.
#[derive(Debug, Clone)]
pub struct UpdateCheckOptions {
    /// The release manifest; tests point it at a local server.
    pub url: String,
    /// Why checks are off for this daemon (`debug_build`, `ci`, `env`), or `None`.
    pub off: Option<&'static str>,
    /// Wait after start before the first check, so it never competes with startup.
    pub first_delay: Duration,
    /// Time between checks.
    pub interval: Duration,
    /// How long a version must have been seen before it is reported. Homebrew and
    /// npm publish a few minutes after the GitHub release.
    pub min_age: Duration,
    /// Limit for one request, redirects included.
    pub timeout: Duration,
}

impl UpdateCheckOptions {
    /// Production timing; off in debug builds, under CI markers and with
    /// `DEVLANES_NO_UPDATE_CHECK` set to a true value.
    pub fn from_env() -> Self {
        let off = if cfg!(debug_assertions) {
            Some("debug_build")
        } else if ci_detected(|name| std::env::var(name).ok()) {
            Some("ci")
        } else if std::env::var("DEVLANES_NO_UPDATE_CHECK")
            .ok()
            .as_deref()
            .is_some_and(truthy)
        {
            Some("env")
        } else {
            None
        };
        Self {
            url: MANIFEST_URL.into(),
            off,
            first_delay: Duration::from_secs(180),
            interval: Duration::from_secs(24 * 3600),
            min_age: Duration::from_secs(2 * 3600),
            timeout: Duration::from_secs(5),
        }
    }
}

/// The CI markers the CLI's output-mode detection uses.
pub(crate) fn ci_detected(lookup: impl Fn(&str) -> Option<String>) -> bool {
    [
        "CI",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "BUILDKITE",
        "CIRCLECI",
        "TF_BUILD",
        "JENKINS_URL",
        "TEAMCITY_VERSION",
        "BITBUCKET_BUILD_NUMBER",
    ]
    .into_iter()
    .any(|name| lookup(name).as_deref().is_some_and(truthy))
}

/// Empty, `0` and `false` count as unset.
fn truthy(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
}

/// What the last successful check found, as stored in the cache file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cached {
    /// Milliseconds since the epoch.
    pub checked_at: u64,
    pub latest: String,
    /// When `latest` was first read, in milliseconds since the epoch.
    pub first_seen_at: u64,
}

/// The daemon's view of the latest release.
#[derive(Debug, Default)]
pub(crate) struct UpdateState {
    cached: Mutex<Option<Cached>>,
}

impl UpdateState {
    /// The cached result.
    fn get(&self) -> Option<Cached> {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replaces the cached result.
    fn set(&self, cached: Cached) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(cached);
    }
}

impl Inner {
    /// Why checks are off, from the daemon's environment or the user config.
    fn update_off(&self) -> Option<&'static str> {
        self.opts
            .update_check
            .off
            .or((!self.opts.user.updates.check).then_some("config"))
    }

    /// The newer release to report, if any.
    pub(crate) fn update_report(&self) -> Option<UpdateInfo> {
        if self.update_off().is_some() {
            return None;
        }
        let cached = self.update.get()?;
        let min_age = u64::try_from(self.opts.update_check.min_age.as_millis()).unwrap_or(u64::MAX);
        let aged = crate::units::now_ms().saturating_sub(cached.first_seen_at) >= min_age;
        (aged && is_newer(&cached.latest, env!("CARGO_PKG_VERSION"))).then_some(UpdateInfo {
            latest: cached.latest,
            checked_at: cached.checked_at,
        })
    }

    /// The check's state for `doctor`.
    pub(crate) fn release_check(&self) -> ReleaseCheck {
        if let Some(reason) = self.update_off() {
            return ReleaseCheck {
                state: "off".into(),
                reason: Some(reason.into()),
                ..ReleaseCheck::default()
            };
        }
        match self.update.get() {
            Some(cached) => ReleaseCheck {
                state: "ok".into(),
                reason: None,
                latest: Some(cached.latest),
                checked_at: Some(cached.checked_at),
            },
            None => ReleaseCheck {
                state: "pending".into(),
                ..ReleaseCheck::default()
            },
        }
    }

    /// Adds `update` to an object result when a newer release is reported.
    pub(crate) fn with_update(&self, mut result: Value) -> Value {
        if let (Some(update), Some(map)) = (self.update_report(), result.as_object_mut())
            && let Ok(update) = serde_json::to_value(update)
        {
            map.insert("update".into(), update);
        }
        result
    }

    /// Loads the cache, then checks after the first delay and every interval.
    /// Returns at once when checks are off.
    pub(crate) async fn update_check_loop(self: Arc<Self>) {
        if self.update_off().is_some() {
            return;
        }
        let path = self.opts.home.join(CACHE_FILE);
        let cached = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || read_cache(&path))
                .await
                .ok()
                .flatten()
        };
        let options = self.opts.update_check.clone();
        let mut wait = options.first_delay;
        if let Some(cached) = cached {
            // A recent answer survives daemon restarts without another request.
            let due = cached
                .checked_at
                .saturating_add(u64::try_from(options.interval.as_millis()).unwrap_or(u64::MAX));
            let left = Duration::from_millis(due.saturating_sub(crate::units::now_ms()));
            wait = wait.max(left);
            self.update.set(cached);
        }
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            wait = options.interval;
            match fetch_latest(&options.url, options.timeout).await {
                Ok(latest) => {
                    let now = crate::units::now_ms();
                    let first_seen_at = match self.update.get() {
                        Some(previous) if previous.latest == latest => previous.first_seen_at,
                        _ => now,
                    };
                    let cached = Cached {
                        checked_at: now,
                        latest,
                        first_seen_at,
                    };
                    // Cached on disk before it is reported, so a status that shows
                    // the release also survives a restart.
                    let (path, written_cached) = (path.clone(), cached.clone());
                    let written =
                        tokio::task::spawn_blocking(move || write_cache(&path, &written_cached))
                            .await
                            .map_err(std::io::Error::other)
                            .and_then(|result| result);
                    if let Err(error) = written {
                        tracing::debug!(%error, "release check cache not written");
                    }
                    self.update.set(cached);
                }
                Err(error) => tracing::debug!(%error, "release check failed"),
            }
        }
    }
}

/// Reads the cache file; a missing or unreadable file is no cache.
fn read_cache(path: &Path) -> Option<Cached> {
    let text = std::fs::read_to_string(path).ok()?;
    let cached: Cached = serde_json::from_str(&text).ok()?;
    Version::parse(&cached.latest).map(|_| cached)
}

/// Writes the cache through a temporary file and a rename.
fn write_cache(path: &Path, cached: &Cached) -> std::io::Result<()> {
    let text = serde_json::to_string(cached).map_err(std::io::Error::other)?;
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&temporary, text)?;
    crate::files::rename(&temporary, path)
}

/// Fetches the manifest with curl, which follows GitHub's redirect to the asset
/// host and uses the system's trust store and proxy settings, and returns its
/// `version`.
async fn fetch_latest(url: &str, timeout: Duration) -> Result<String, String> {
    // Only a test URL is plain HTTP; real checks refuse anything but HTTPS.
    let protocols = if url.starts_with("https://") {
        "=https"
    } else {
        "=http,https"
    };
    let seconds = timeout.as_secs().max(1).to_string();
    let output = tokio::time::timeout(
        timeout + Duration::from_secs(1),
        tokio::process::Command::new("curl")
            .args(["-fsSL", "--max-time", &seconds, "--proto", protocols])
            .args(["--proto-redir", protocols, "--max-filesize", "65536", url])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "timed out".to_owned())?
    .map_err(|error| format!("curl: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    if output.stdout.len() > MAX_MANIFEST_BYTES {
        return Err("manifest too large".into());
    }
    let manifest: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    let version = manifest
        .get("version")
        .and_then(Value::as_str)
        .ok_or("manifest has no version")?;
    Version::parse(version)
        .map(|_| version.trim_start_matches('v').to_owned())
        .ok_or_else(|| format!("manifest version {version:?} is not a release version"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_markers() {
        assert!(ci_detected(|name| (name == "CI").then(|| "true".into())));
        assert!(ci_detected(
            |name| (name == "GITHUB_ACTIONS").then(|| "true".into())
        ));
        assert!(!ci_detected(|name| (name == "CI").then(|| "false".into())));
        assert!(!ci_detected(|_| None));
    }

    #[test]
    fn cache_round_trip_and_rejects_junk() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(CACHE_FILE);
        let cached = Cached {
            checked_at: 5,
            latest: "0.3.0".into(),
            first_seen_at: 4,
        };
        write_cache(&path, &cached)?;
        assert_eq!(read_cache(&path), Some(cached));
        std::fs::write(
            &path,
            r#"{"checked_at":1,"latest":"junk","first_seen_at":1}"#,
        )?;
        assert_eq!(read_cache(&path), None);
        std::fs::write(&path, "not json")?;
        assert_eq!(read_cache(&path), None);
        Ok(())
    }
}
