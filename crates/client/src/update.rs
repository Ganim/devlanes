//! Release notices on the client side: how this binary was installed, the
//! command that upgrades it, and when a notice was last shown or dismissed. The
//! daemon finds new releases; clients only print what it reports.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// The repository that `cargo binstall --git` and `cargo install --git` build from.
const REPOSITORY: &str = "https://github.com/devlanes-run/devlanes";
/// `install.sh` of the latest stable release, served by the website.
const INSTALL_SCRIPT: &str = "https://devlanes.run/install.sh";
/// The notice log under `$DEVLANES_HOME`.
const NOTICES_FILE: &str = "notices.json";
/// A notice repeats at most once per day.
const REPEAT_MS: u64 = 24 * 3600 * 1000;

/// How this devlanes binary was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Npm,
    Pnpm,
    Bun,
    Yarn,
    CargoBinstall,
    CargoInstall,
    /// A `target/` directory of a checkout.
    Source,
    /// `install.sh`, or a copy placed by hand, in this directory.
    Script(PathBuf),
}

impl InstallMethod {
    /// Classifies a canonical executable path. `cargo_home` is `$CARGO_HOME` or
    /// `~/.cargo`; `binstall_lists_devlanes` says whether cargo binstall's
    /// `crates-v1.json` records devlanes.
    pub fn detect(exe: &Path, cargo_home: Option<&Path>, binstall_lists_devlanes: bool) -> Self {
        let text = exe.to_string_lossy();
        if text.contains("/Cellar/devlanes/") {
            return Self::Homebrew;
        }
        if text.contains("/node_modules/@devlanes/") || text.contains("/node_modules/devlanes/") {
            return if text.contains("/.bun/install/global/") {
                Self::Bun
            } else if text.contains("/.pnpm/") || text.contains("/pnpm/global/") {
                Self::Pnpm
            } else if text.contains("/yarn/global/") {
                Self::Yarn
            } else {
                Self::Npm
            };
        }
        let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        if cargo_home.is_some_and(|home| dir == home.join("bin")) {
            return if binstall_lists_devlanes {
                Self::CargoBinstall
            } else {
                Self::CargoInstall
            };
        }
        if text.contains("/target/debug/") || text.contains("/target/release/") {
            return Self::Source;
        }
        Self::Script(dir)
    }

    /// The method of the running binary. Reads the filesystem, so async code
    /// calls it from a blocking task.
    pub fn current() -> Self {
        let exe = std::env::current_exe()
            .and_then(dunce::canonicalize)
            .unwrap_or_else(|_| PathBuf::from("devlanes"));
        let cargo_home = std::env::var_os("CARGO_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::user_home().map(|home| home.join(".cargo")));
        let cargo_home = cargo_home.map(|home| dunce::canonicalize(&home).unwrap_or(home));
        let listed = cargo_home.as_deref().is_some_and(|home| {
            std::fs::read_to_string(home.join("binstall/crates-v1.json"))
                .is_ok_and(|text| binstall_lists_devlanes(&text))
        });
        Self::detect(&exe, cargo_home.as_deref(), listed)
    }

    /// A short name for doctor.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Bun => "Bun",
            Self::Yarn => "Yarn",
            Self::CargoBinstall => "cargo binstall",
            Self::CargoInstall => "cargo install",
            Self::Source => "a source checkout",
            Self::Script(_) => "install.sh",
        }
    }

    /// How doctor names the install: [`Self::label`], except that a binary outside
    /// install.sh's default directory (`~/.local/bin` under `home`) is named by its
    /// directory, since nothing says install.sh put it there.
    pub fn describe(&self, home: Option<&Path>) -> String {
        match self {
            Self::Script(dir) => {
                let default = home.map(|home| home.join(".local/bin"));
                let default = default
                    .as_deref()
                    .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
                if default.as_deref() == Some(dir.as_path()) {
                    "install.sh".into()
                } else {
                    format!("a standalone binary in {}", dir.display())
                }
            }
            other => other.label().into(),
        }
    }

    /// The command that installs `version` the same way.
    pub fn upgrade_command(&self, version: &str) -> String {
        match self {
            Self::Homebrew => "brew upgrade devlanes-run/tap/devlanes".into(),
            Self::Npm => "npm install -g devlanes@latest".into(),
            Self::Pnpm => "pnpm add -g devlanes@latest".into(),
            Self::Bun => "bun add -g devlanes@latest".into(),
            Self::Yarn => "yarn global add devlanes@latest".into(),
            Self::CargoBinstall => format!("cargo binstall --git {REPOSITORY} devlanes"),
            Self::CargoInstall => format!(
                "cargo install --locked --git {REPOSITORY} --tag v{version} devlanes devlanes-hosts-helper"
            ),
            Self::Source => format!("git pull, then cargo build --release (v{version})"),
            Self::Script(dir) => format!(
                "curl -fsSL {INSTALL_SCRIPT} | DEVLANES_INSTALL_DIR={} sh",
                shell_quote(&dir.to_string_lossy())
            ),
        }
    }
}

/// Whether binstall's `crates-v1.json` (concatenated JSON objects) has a
/// `devlanes` entry.
pub fn binstall_lists_devlanes(text: &str) -> bool {
    serde_json::Deserializer::from_str(text)
        .into_iter::<serde_json::Value>()
        .map_while(Result::ok)
        .any(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some("devlanes"))
}

/// Quotes `text` for a POSIX shell when it needs it.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+~".contains(&b));
    if plain {
        text.into()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The notice for a newer release.
pub fn update_line(latest: &str, current: &str, command: &str) -> String {
    format!("devlanes {latest} is available (this is {current}): {command}")
}

/// The notice for a daemon that runs another version than this client.
pub fn skew_line(daemon: &str, client: &str) -> String {
    format!(
        "daemon runs {daemon}, this CLI is {client}; run devlanes daemon restart when convenient (it stops services)"
    )
}

/// Notice key for a release.
pub fn update_key(latest: &str) -> String {
    format!("update:{latest}")
}

/// Notice key for a version pair.
pub fn skew_key(daemon: &str, client: &str) -> String {
    format!("skew:{daemon}:{client}")
}

/// When each notice was last shown, and which ones the TUI dismissed, in
/// `$DEVLANES_HOME/notices.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeLog {
    /// Notice key to milliseconds since the epoch.
    #[serde(default)]
    pub shown: BTreeMap<String, u64>,
    /// Keys the user dismissed in the TUI.
    #[serde(default)]
    pub dismissed: Vec<String>,
}

impl NoticeLog {
    /// Reads the log under `home`; a missing or broken file is an empty log.
    pub fn load(home: &Path) -> Self {
        std::fs::read_to_string(home.join(NOTICES_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes the log through a temporary file and a rename.
    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let path = home.join(NOTICES_FILE);
        let temporary = home.join(format!(".{NOTICES_FILE}.{}", std::process::id()));
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)
    }

    /// Whether `key` may be shown at `now`: never shown, or shown a day ago.
    pub fn due(&self, key: &str, now: u64) -> bool {
        self.shown
            .get(key)
            .is_none_or(|shown| now.saturating_sub(*shown) >= REPEAT_MS)
    }

    /// Records `key` as shown at `now`, keeping only the newest entries.
    pub fn mark_shown(&mut self, key: &str, now: u64) {
        self.shown.insert(key.into(), now);
        while self.shown.len() > 32 {
            let oldest = self
                .shown
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(oldest) => self.shown.remove(&oldest),
                None => break,
            };
        }
    }

    /// Whether the TUI user dismissed `key`.
    pub fn is_dismissed(&self, key: &str) -> bool {
        self.dismissed.iter().any(|dismissed| dismissed == key)
    }

    /// Dismisses `key`, keeping the most recent dismissals.
    pub fn dismiss(&mut self, key: &str) {
        if !self.is_dismissed(key) {
            self.dismissed.push(key.into());
        }
        let excess = self.dismissed.len().saturating_sub(32);
        self.dismissed.drain(..excess);
    }
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(path: &str) -> InstallMethod {
        InstallMethod::detect(Path::new(path), Some(Path::new("/Users/a/.cargo")), false)
    }

    #[test]
    fn detects_install_methods_from_paths() {
        assert_eq!(
            detect("/opt/homebrew/Cellar/devlanes/0.2.0/bin/devlanes"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            detect("/home/linuxbrew/.linuxbrew/Cellar/devlanes/0.2.0/bin/devlanes"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            detect(
                "/usr/local/lib/node_modules/devlanes/node_modules/@devlanes/darwin-arm64/bin/devlanes"
            ),
            InstallMethod::Npm
        );
        assert_eq!(
            detect(
                "/Users/a/.nvm/versions/node/v24.1.0/lib/node_modules/@devlanes/linux-x64/bin/devlanes"
            ),
            InstallMethod::Npm
        );
        assert_eq!(
            detect(
                "/Users/a/Library/pnpm/global/5/.pnpm/@devlanes+darwin-arm64@0.2.0/node_modules/@devlanes/darwin-arm64/bin/devlanes"
            ),
            InstallMethod::Pnpm
        );
        assert_eq!(
            detect("/Users/a/.bun/install/global/node_modules/@devlanes/darwin-arm64/bin/devlanes"),
            InstallMethod::Bun
        );
        assert_eq!(
            detect("/Users/a/.config/yarn/global/node_modules/@devlanes/linux-arm64/bin/devlanes"),
            InstallMethod::Yarn
        );
        assert_eq!(
            detect("/Users/a/.cargo/bin/devlanes"),
            InstallMethod::CargoInstall
        );
        assert_eq!(
            InstallMethod::detect(
                Path::new("/Users/a/.cargo/bin/devlanes"),
                Some(Path::new("/Users/a/.cargo")),
                true
            ),
            InstallMethod::CargoBinstall
        );
        assert_eq!(
            detect("/Users/a/dev/devlanes/target/release/devlanes"),
            InstallMethod::Source
        );
        assert_eq!(
            detect("/Users/a/.local/bin/devlanes"),
            InstallMethod::Script(PathBuf::from("/Users/a/.local/bin"))
        );
        assert_eq!(
            InstallMethod::detect(Path::new("/opt/tools/devlanes"), None, false),
            InstallMethod::Script(PathBuf::from("/opt/tools"))
        );
    }

    /// Only install.sh's default directory is named install.sh.
    #[test]
    fn standalone_binaries_are_named_by_directory() {
        let home = Path::new("/nonexistent-home");
        assert_eq!(
            InstallMethod::Script(home.join(".local/bin")).describe(Some(home)),
            "install.sh"
        );
        assert_eq!(
            InstallMethod::Script(PathBuf::from("/opt/tools")).describe(Some(home)),
            "a standalone binary in /opt/tools"
        );
        assert_eq!(InstallMethod::Homebrew.describe(Some(home)), "Homebrew");
    }

    #[test]
    fn upgrade_commands() {
        assert_eq!(
            InstallMethod::Homebrew.upgrade_command("0.3.0"),
            "brew upgrade devlanes-run/tap/devlanes"
        );
        assert_eq!(
            InstallMethod::Pnpm.upgrade_command("0.3.0"),
            "pnpm add -g devlanes@latest"
        );
        assert_eq!(
            InstallMethod::CargoInstall.upgrade_command("0.3.0"),
            "cargo install --locked --git https://github.com/devlanes-run/devlanes --tag v0.3.0 devlanes devlanes-hosts-helper"
        );
        assert_eq!(
            InstallMethod::Script(PathBuf::from("/Users/a/.local/bin")).upgrade_command("0.3.0"),
            "curl -fsSL https://devlanes.run/install.sh | DEVLANES_INSTALL_DIR=/Users/a/.local/bin sh"
        );
        assert_eq!(
            InstallMethod::Script(PathBuf::from("/Users/a/My Tools")).upgrade_command("0.3.0"),
            "curl -fsSL https://devlanes.run/install.sh | DEVLANES_INSTALL_DIR='/Users/a/My Tools' sh"
        );
    }

    #[test]
    fn binstall_manifest() {
        let text =
            r#"{"name":"cargo-nextest","version_req":"*"}{"name":"devlanes","version_req":"*"}"#;
        assert!(binstall_lists_devlanes(text));
        assert!(!binstall_lists_devlanes(
            r#"{"name":"devlanes-hosts-helper"}"#
        ));
        assert!(!binstall_lists_devlanes("garbage"));
    }

    #[test]
    fn notices_repeat_daily_and_persist() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut log = NoticeLog::load(dir.path());
        assert!(log.due("update:0.3.0", 1_000));
        log.mark_shown("update:0.3.0", 1_000);
        assert!(!log.due("update:0.3.0", 1_000 + REPEAT_MS - 1));
        assert!(log.due("update:0.3.0", 1_000 + REPEAT_MS));
        assert!(log.due("update:0.4.0", 1_001));
        log.dismiss("update:0.3.0");
        log.save(dir.path())?;
        let loaded = NoticeLog::load(dir.path());
        assert!(loaded.is_dismissed("update:0.3.0"));
        assert_eq!(loaded.shown.get("update:0.3.0"), Some(&1_000));
        std::fs::write(dir.path().join(NOTICES_FILE), "broken")?;
        assert_eq!(NoticeLog::load(dir.path()), NoticeLog::default());
        Ok(())
    }
}
