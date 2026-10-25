//! Release notices on the client side: how this binary was installed, the
//! command that upgrades it, and when a notice was last shown or dismissed. The
//! daemon finds new releases; clients only print what it reports.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use devlanes_version::REPOSITORY;
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
    /// `install.sh` (`install.ps1` on Windows), or a copy placed by hand, in
    /// this directory.
    Script(PathBuf),
}

impl InstallMethod {
    /// Classifies a canonical executable path. `cargo_home` is `$CARGO_HOME` or
    /// `~/.cargo`; `binstall_lists_devlanes` says whether cargo binstall's
    /// `crates-v1.json` records devlanes.
    pub fn detect(exe: &Path, cargo_home: Option<&Path>, binstall_lists_devlanes: bool) -> Self {
        let text = os::normalise(&exe.to_string_lossy());
        let has = |needle: &str| text.contains(&os::normalise(needle));
        if has("/Cellar/devlanes/") {
            return Self::Homebrew;
        }
        if has("/node_modules/@devlanes/") || has("/node_modules/devlanes/") {
            return if has("/.bun/install/global/") {
                Self::Bun
            } else if has("/.pnpm/") || has("/pnpm/global/") {
                Self::Pnpm
            } else if os::YARN_GLOBAL.iter().any(|needle| has(needle)) {
                Self::Yarn
            } else {
                Self::Npm
            };
        }
        let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        if cargo_home.is_some_and(|home| os::same_dir(&dir, &home.join("bin"))) {
            return if binstall_lists_devlanes {
                Self::CargoBinstall
            } else {
                Self::CargoInstall
            };
        }
        if has("/target/debug/") || has("/target/release/") {
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
            Self::Script(_) => os::SCRIPT,
        }
    }

    /// How doctor names the install: [`Self::label`], except that a binary outside
    /// the install script's default directory (`~/.local/bin` under `home`, or
    /// `%LOCALAPPDATA%\Programs\devlanes` on Windows) is named by its directory,
    /// since nothing says the script put it there.
    pub fn describe(&self, home: Option<&Path>) -> String {
        match self {
            Self::Script(dir) => {
                let default = os::default_script_dir(home);
                let default = default
                    .as_deref()
                    .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
                if default.is_some_and(|default| os::same_dir(&default, dir)) {
                    os::SCRIPT.into()
                } else {
                    format!("a standalone binary in {}", dir.display())
                }
            }
            other => other.label().into(),
        }
    }

    /// The command that installs `version` the same way. On Windows, methods
    /// that overwrite the executable the daemon runs from say to stop it first.
    pub fn upgrade_command(&self, version: &str) -> String {
        let command = match self {
            Self::Homebrew => "brew upgrade devlanes-run/tap/devlanes".into(),
            Self::Npm => "npm install -g devlanes@latest".into(),
            Self::Pnpm => "pnpm add -g devlanes@latest".into(),
            Self::Bun => "bun add -g devlanes@latest".into(),
            Self::Yarn => "yarn global add devlanes@latest".into(),
            Self::CargoBinstall => format!("cargo binstall --git {REPOSITORY} devlanes"),
            Self::CargoInstall => format!(
                "cargo install --locked --git {REPOSITORY} --tag v{version} {}",
                os::CARGO_PACKAGES
            ),
            Self::Source => format!("git pull, then cargo build --release (v{version})"),
            Self::Script(dir) => os::script_upgrade(dir),
        };
        if os::needs_daemon_stop(self) {
            format!("devlanes daemon stop (it stops services), then {command}")
        } else {
            command
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

/// macOS and Linux: `install.sh`, POSIX shell quoting, paths matched as they
/// are, and installers that replace a running executable by renaming over it.
#[cfg(unix)]
mod os {
    use std::path::{Path, PathBuf};

    use super::InstallMethod;

    /// `install.sh` of the latest stable release, served by the website.
    const INSTALL_SCRIPT: &str = "https://devlanes.run/install.sh";
    /// The install script's name in doctor.
    pub(super) const SCRIPT: &str = "install.sh";
    /// The packages `cargo install` builds.
    pub(super) const CARGO_PACKAGES: &str = "devlanes devlanes-hosts-helper";
    /// Path fragments of Yarn's global directory.
    pub(super) const YARN_GLOBAL: &[&str] = &["/yarn/global/"];

    /// Paths are matched as they are.
    pub(super) fn normalise(path: &str) -> String {
        path.to_owned()
    }

    /// Whether two directories are the same path.
    pub(super) fn same_dir(a: &Path, b: &Path) -> bool {
        a == b
    }

    /// `install.sh`'s default directory, `~/.local/bin`.
    pub(super) fn default_script_dir(home: Option<&Path>) -> Option<PathBuf> {
        home.map(|home| home.join(".local/bin"))
    }

    /// `install.sh` into `dir`.
    pub(super) fn script_upgrade(dir: &Path) -> String {
        format!(
            "curl -fsSL {INSTALL_SCRIPT} | DEVLANES_INSTALL_DIR={} sh",
            shell_quote(&dir.to_string_lossy())
        )
    }

    /// Every installer renames over a running executable.
    pub(super) fn needs_daemon_stop(_method: &InstallMethod) -> bool {
        false
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
}

/// Windows: `install.ps1`, PowerShell quoting, paths compared with `/` for `\`
/// and without case, and executables that cannot be replaced while they run.
#[cfg(windows)]
mod os {
    use std::path::{Path, PathBuf};

    use super::InstallMethod;

    /// `install.ps1` of the latest stable release, served by the website like
    /// `install.sh`.
    const INSTALL_SCRIPT: &str = "https://devlanes.run/install.ps1";
    /// The install script's name in doctor.
    pub(super) const SCRIPT: &str = "install.ps1";
    /// The packages `cargo install` builds; `devlanes-hosts-helper` is Unix-only.
    pub(super) const CARGO_PACKAGES: &str = "devlanes";
    /// Path fragments of Yarn's global directory: Yarn 1 keeps it under
    /// `%LOCALAPPDATA%\Yarn\Data\global` on Windows.
    pub(super) const YARN_GLOBAL: &[&str] = &["/yarn/global/", "/Yarn/Data/global/"];

    /// `\` as `/` and lower case, since Windows paths ignore case and the
    /// patterns use `/`.
    pub(super) fn normalise(path: &str) -> String {
        path.replace('\\', "/").to_lowercase()
    }

    /// Whether two directories are the same path, ignoring case and separators.
    pub(super) fn same_dir(a: &Path, b: &Path) -> bool {
        normalise(&a.to_string_lossy()).trim_end_matches('/')
            == normalise(&b.to_string_lossy()).trim_end_matches('/')
    }

    /// `install.ps1`'s default directory, `%LOCALAPPDATA%\Programs\devlanes`.
    /// The profile directory in `home` does not locate it.
    pub(super) fn default_script_dir(_home: Option<&Path>) -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .filter(|dir| !dir.is_empty())
            .map(|dir| script_dir_in(Path::new(&dir)))
    }

    /// `install.ps1`'s default directory under a `%LOCALAPPDATA%` directory.
    pub(super) fn script_dir_in(local_app_data: &Path) -> PathBuf {
        local_app_data.join("Programs").join("devlanes")
    }

    /// `install.ps1` into `dir`, as one PowerShell line. Named as PowerShell,
    /// since cmd.exe and Git Bash would fail on it.
    pub(super) fn script_upgrade(dir: &Path) -> String {
        format!(
            "in PowerShell: $env:DEVLANES_INSTALL_DIR='{}'; irm {INSTALL_SCRIPT} | iex",
            dir.to_string_lossy().replace('\'', "''")
        )
    }

    /// Windows refuses to overwrite or delete a running executable, and the
    /// daemon runs from the installed one. `install.ps1` renames it out of the
    /// way; npm, cargo and a rebuild fail while it runs. Homebrew has no Windows
    /// build, so its advice stays as on every other OS.
    pub(super) fn needs_daemon_stop(method: &InstallMethod) -> bool {
        !matches!(method, InstallMethod::Script(_) | InstallMethod::Homebrew)
    }
}

/// Copies of `devlanes.exe` that an upgrade left in a directory.
#[cfg(windows)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OldCopies {
    /// Deleted now.
    pub removed: Vec<PathBuf>,
    /// Still held by a running process (usually the daemon), so kept.
    pub in_use: Vec<PathBuf>,
}

/// Deletes the `devlanes.exe.old-*` files `install.ps1` leaves next to an
/// executable that was running during an upgrade. Windows refuses to delete
/// one that still runs; it is reported in `in_use` and tried again next time.
/// Blocking filesystem work.
#[cfg(windows)]
pub fn remove_old_copies(dir: &Path) -> OldCopies {
    let mut copies = OldCopies::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return copies;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            name.strip_prefix("devlanes.exe.old-")
                .is_some_and(|stamp| !stamp.is_empty())
        })
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::remove_file(&path) {
            Ok(()) => copies.removed.push(path),
            Err(_) => copies.in_use.push(path),
        }
    }
    copies
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
    #[cfg(unix)]
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

    /// Committed TUI screens show Homebrew's advice, so it reads the same on every OS.
    #[test]
    fn homebrew_advice_is_the_same_everywhere() {
        assert_eq!(
            InstallMethod::Homebrew.upgrade_command("0.3.0"),
            "brew upgrade devlanes-run/tap/devlanes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_commands() {
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

    #[cfg(windows)]
    #[test]
    fn detects_windows_install_methods_from_paths() {
        let cargo = Path::new(r"C:\Users\a\.cargo");
        let detect = |path: &str| InstallMethod::detect(Path::new(path), Some(cargo), false);
        assert_eq!(
            detect(
                r"C:\Users\a\AppData\Roaming\npm\node_modules\devlanes\node_modules\@devlanes\win32-x64\bin\devlanes.exe"
            ),
            InstallMethod::Npm
        );
        assert_eq!(
            detect(
                r"C:\Users\a\AppData\Local\pnpm\global\5\.pnpm\@devlanes+win32-arm64@0.4.0\node_modules\@devlanes\win32-arm64\bin\devlanes.exe"
            ),
            InstallMethod::Pnpm
        );
        assert_eq!(
            detect(
                r"C:\Users\a\.bun\install\global\node_modules\@devlanes\win32-x64\bin\devlanes.exe"
            ),
            InstallMethod::Bun
        );
        assert_eq!(
            detect(
                r"C:\Users\a\AppData\Local\Yarn\Data\global\node_modules\@devlanes\win32-x64\bin\devlanes.exe"
            ),
            InstallMethod::Yarn
        );
        // Canonical paths keep the case on disk; CARGO_HOME may be spelled otherwise.
        assert_eq!(
            detect(r"c:\users\A\.Cargo\bin\devlanes.exe"),
            InstallMethod::CargoInstall
        );
        assert_eq!(
            InstallMethod::detect(
                Path::new(r"C:\Users\a\.cargo\bin\devlanes.exe"),
                Some(cargo),
                true
            ),
            InstallMethod::CargoBinstall
        );
        assert_eq!(
            detect(r"D:\src\devlanes\Target\Release\devlanes.exe"),
            InstallMethod::Source
        );
        assert_eq!(
            detect(r"C:\Users\a\AppData\Local\Programs\devlanes\devlanes.exe"),
            InstallMethod::Script(PathBuf::from(r"C:\Users\a\AppData\Local\Programs\devlanes"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn install_ps1_directory_comes_from_local_app_data() {
        let dir = os::script_dir_in(Path::new(r"C:\Users\a\AppData\Local"));
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\a\AppData\Local\Programs\devlanes")
        );
        assert!(os::same_dir(
            &dir,
            Path::new(r"c:\users\a\appdata\local\programs\devlanes\")
        ));
        assert!(!os::same_dir(&dir, Path::new(r"C:\tools")));
        // The real %LOCALAPPDATA% decides what doctor calls install.ps1.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let default = os::script_dir_in(Path::new(&local));
            assert_eq!(InstallMethod::Script(default).describe(None), "install.ps1");
        }
        assert_eq!(
            InstallMethod::Script(PathBuf::from(r"C:\tools")).describe(None),
            r"a standalone binary in C:\tools"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_upgrades_stop_the_daemon_unless_install_ps1_renames() {
        assert_eq!(
            InstallMethod::Npm.upgrade_command("0.4.0"),
            "devlanes daemon stop (it stops services), then npm install -g devlanes@latest"
        );
        assert_eq!(
            InstallMethod::CargoBinstall.upgrade_command("0.4.0"),
            "devlanes daemon stop (it stops services), then cargo binstall --git https://github.com/devlanes-run/devlanes devlanes"
        );
        assert_eq!(
            InstallMethod::CargoInstall.upgrade_command("0.4.0"),
            "devlanes daemon stop (it stops services), then cargo install --locked --git https://github.com/devlanes-run/devlanes --tag v0.4.0 devlanes"
        );
        assert_eq!(
            InstallMethod::Script(PathBuf::from(r"C:\Users\o'neil\Programs\devlanes"))
                .upgrade_command("0.4.0"),
            "in PowerShell: $env:DEVLANES_INSTALL_DIR='C:\\Users\\o''neil\\Programs\\devlanes'; irm https://devlanes.run/install.ps1 | iex"
        );
    }

    #[cfg(windows)]
    #[test]
    fn old_copies_are_removed_unless_running() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("devlanes.exe"), "current")?;
        std::fs::write(dir.path().join("devlanes.exe.old-20261023101500"), "old")?;
        std::fs::write(dir.path().join("devlanes.exe.old-"), "no stamp")?;
        std::fs::write(dir.path().join("other.exe.old-20261023101500"), "other")?;
        // An open handle without delete sharing stands in for a running executable.
        let held = dir.path().join("devlanes.exe.old-20261023101600");
        std::fs::write(&held, "running")?;
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&held)?
        };
        let copies = remove_old_copies(dir.path());
        assert_eq!(
            copies.removed,
            vec![dir.path().join("devlanes.exe.old-20261023101500")]
        );
        assert_eq!(copies.in_use, vec![held.clone()]);
        drop(handle);
        assert_eq!(remove_old_copies(dir.path()).removed, vec![held]);
        for kept in [
            "devlanes.exe",
            "devlanes.exe.old-",
            "other.exe.old-20261023101500",
        ] {
            assert!(dir.path().join(kept).exists(), "{kept}");
        }
        Ok(())
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
