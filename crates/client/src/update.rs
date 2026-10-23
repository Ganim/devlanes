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
/// The notice log under `$DEVLANES_HOME`.
const NOTICES_FILE: &str = "notices.json";
/// A notice repeats at most once per day.
const REPEAT_MS: u64 = 24 * 3600 * 1000;
/// The Windows executables a release ships side by side. `install.ps1` renames
/// one that is running to `<name>.old-<timestamp>`; see [`remove_old_copies`].
const SHIPPED_EXECUTABLES: [&str; 3] = [
    "devlanes.exe",
    "devlanesw.exe",
    "devlanes-hosts-service.exe",
];

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
            } else if has("/yarn/global/") || has("/Yarn/Data/global/") {
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
    /// `AppData\Local\Programs\devlanes` on Windows) is named by its directory,
    /// since nothing says the script put it there.
    pub fn describe(&self, home: Option<&Path>) -> String {
        match self {
            Self::Script(dir) => {
                let default = home.map(os::default_script_dir);
                let default = default
                    .as_deref()
                    .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
                if default.as_deref() == Some(dir.as_path()) {
                    os::SCRIPT.into()
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
            Self::CargoBinstall => os::cargo_binstall(),
            Self::CargoInstall => format!(
                "cargo install --locked --git {REPOSITORY} --tag v{version} {}",
                os::CARGO_PACKAGES
            ),
            Self::Source => format!("git pull, then cargo build --release (v{version})"),
            Self::Script(dir) => os::script_upgrade(dir),
        }
    }

    /// What to run to upgrade to `version`: [`Self::upgrade_command`], after
    /// `devlanes daemon stop` when that command cannot replace the executable
    /// the daemon runs from (Windows locks a running executable).
    pub fn upgrade_advice(&self, version: &str) -> String {
        let command = self.upgrade_command(version);
        if os::replaces_running_executable(self) {
            command
        } else {
            format!("devlanes daemon stop (it stops services), then {command}")
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

/// Copies of the shipped executables an upgrade left in a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OldCopies {
    /// Deleted now.
    pub removed: Vec<PathBuf>,
    /// Still held by a running process (usually the daemon), so kept.
    pub in_use: Vec<PathBuf>,
}

/// Deletes the `devlanes.exe.old-*` (and `devlanesw.exe`,
/// `devlanes-hosts-service.exe`) files `install.ps1` leaves next to an
/// executable that was running during an upgrade. Windows refuses to delete
/// one that still runs; it is reported in `in_use` and tried again next time.
/// Blocking filesystem work.
pub fn remove_old_copies(dir: &Path) -> OldCopies {
    let mut copies = OldCopies::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return copies;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            SHIPPED_EXECUTABLES.iter().any(|exe| {
                name.strip_prefix(exe)
                    .and_then(|rest| rest.strip_prefix(".old-"))
                    .is_some_and(|stamp| !stamp.is_empty())
            })
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

/// macOS and Linux: `install.sh`, POSIX shell quoting, and executables that can
/// be replaced while they run.
#[cfg(not(windows))]
mod os {
    use std::path::{Path, PathBuf};

    use super::{InstallMethod, REPOSITORY};

    /// `install.sh` of the latest stable release, served by the website.
    const INSTALL_SCRIPT: &str = "https://devlanes.run/install.sh";
    /// The install script's name in doctor.
    pub(super) const SCRIPT: &str = "install.sh";
    /// The packages `cargo install` builds.
    pub(super) const CARGO_PACKAGES: &str = "devlanes devlanes-hosts-helper";

    /// Paths are matched as they are.
    pub(super) fn normalise(path: &str) -> String {
        path.to_owned()
    }

    /// `install.sh`'s default directory.
    pub(super) fn default_script_dir(home: &Path) -> PathBuf {
        home.join(".local/bin")
    }

    /// `install.sh` into `dir`.
    pub(super) fn script_upgrade(dir: &Path) -> String {
        format!(
            "curl -fsSL {INSTALL_SCRIPT} | DEVLANES_INSTALL_DIR={} sh",
            shell_quote(&dir.to_string_lossy())
        )
    }

    /// cargo binstall installs devlanes and nothing else is needed.
    pub(super) fn cargo_binstall() -> String {
        format!("cargo binstall --git {REPOSITORY} devlanes")
    }

    /// Every installer replaces a running binary by renaming over it.
    pub(super) fn replaces_running_executable(_method: &InstallMethod) -> bool {
        true
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

/// Windows: `install.ps1`, PowerShell quoting, case-insensitive `\` paths, and
/// three executables that must sit side by side.
#[cfg(windows)]
mod os {
    use std::path::{Path, PathBuf};

    use super::{InstallMethod, REPOSITORY};

    /// `install.ps1` of the latest release; every release attaches it.
    const INSTALL_SCRIPT: &str =
        "https://github.com/devlanes-run/devlanes/releases/latest/download/install.ps1";
    /// The install script's name in doctor.
    pub(super) const SCRIPT: &str = "install.ps1";
    /// The packages `cargo install` builds: `devlanes.exe` finds the logon
    /// launcher and the hosts service next to itself.
    pub(super) const CARGO_PACKAGES: &str =
        "devlanes devlanes-logon-launcher devlanes-hosts-service";

    /// `\` as `/` and lower case, since Windows paths ignore case and the
    /// patterns use `/`.
    pub(super) fn normalise(path: &str) -> String {
        path.replace('\\', "/").to_lowercase()
    }

    /// `install.ps1`'s default directory, `%LOCALAPPDATA%\Programs\devlanes`
    /// for the usual `%LOCALAPPDATA%`.
    pub(super) fn default_script_dir(home: &Path) -> PathBuf {
        home.join(r"AppData\Local\Programs\devlanes")
    }

    /// `install.ps1` into `dir`, as one PowerShell line. Named as PowerShell,
    /// since cmd.exe and Git Bash would fail on it.
    pub(super) fn script_upgrade(dir: &Path) -> String {
        format!(
            "in PowerShell: $env:DEVLANES_INSTALL_DIR='{}'; irm {INSTALL_SCRIPT} | iex",
            dir.to_string_lossy().replace('\'', "''")
        )
    }

    /// binstall takes one crate per `--git` run, and each executable is a
    /// crate of its own.
    pub(super) fn cargo_binstall() -> String {
        format!(
            "cargo binstall --git {REPOSITORY} devlanes, then the same for devlanes-logon-launcher and devlanes-hosts-service"
        )
    }

    /// Windows refuses to overwrite or delete a running executable. Only
    /// `install.ps1` renames it out of the way; npm, cargo and a rebuild fail
    /// while the daemon runs from the installed file.
    pub(super) fn replaces_running_executable(method: &InstallMethod) -> bool {
        matches!(method, InstallMethod::Script(_))
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

    /// Windows paths as `std::env::current_exe` and canonicalisation give them.
    #[cfg(windows)]
    #[test]
    fn detects_windows_install_methods() {
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
                r"C:\Users\a\AppData\Local\pnpm\global\5\.pnpm\@devlanes+win32-x64@0.3.0\node_modules\@devlanes\win32-x64\bin\devlanes.exe"
            ),
            InstallMethod::Pnpm
        );
        assert_eq!(
            detect(
                r"C:\Users\a\.bun\install\global\node_modules\@devlanes\win32-arm64\bin\devlanes.exe"
            ),
            InstallMethod::Bun
        );
        assert_eq!(
            detect(
                r"C:\Users\a\AppData\Local\Yarn\Data\global\node_modules\@devlanes\win32-x64\bin\devlanes.exe"
            ),
            InstallMethod::Yarn
        );
        assert_eq!(
            detect(r"C:\Users\a\.cargo\bin\devlanes.exe"),
            InstallMethod::CargoInstall
        );
        assert_eq!(
            detect(r"D:\dev\devlanes\Target\Release\devlanes.exe"),
            InstallMethod::Source
        );
        assert_eq!(
            detect(r"C:\Users\a\AppData\Local\Programs\devlanes\devlanes.exe"),
            InstallMethod::Script(PathBuf::from(r"C:\Users\a\AppData\Local\Programs\devlanes"))
        );
    }

    /// Only the install script's default directory is named after the script.
    #[test]
    fn standalone_binaries_are_named_by_directory() {
        let home = Path::new("/nonexistent-home");
        #[cfg(not(windows))]
        assert_eq!(
            InstallMethod::Script(home.join(".local/bin")).describe(Some(home)),
            "install.sh"
        );
        #[cfg(windows)]
        assert_eq!(
            InstallMethod::Script(home.join(r"AppData\Local\Programs\devlanes"))
                .describe(Some(home)),
            "install.ps1"
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
    }

    #[cfg(windows)]
    #[test]
    fn windows_upgrades_stop_the_daemon_unless_install_ps1_runs() {
        assert_eq!(
            InstallMethod::CargoInstall.upgrade_command("0.3.0"),
            "cargo install --locked --git https://github.com/devlanes-run/devlanes --tag v0.3.0 devlanes devlanes-logon-launcher devlanes-hosts-service"
        );
        let script = InstallMethod::Script(PathBuf::from(r"C:\Users\o'neil\Tools"));
        assert_eq!(
            script.upgrade_advice("0.3.0"),
            "in PowerShell: $env:DEVLANES_INSTALL_DIR='C:\\Users\\o''neil\\Tools'; irm https://github.com/devlanes-run/devlanes/releases/latest/download/install.ps1 | iex"
        );
        assert_eq!(
            InstallMethod::Npm.upgrade_advice("0.3.0"),
            "devlanes daemon stop (it stops services), then npm install -g devlanes@latest"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_upgrade_commands() {
        assert_eq!(
            InstallMethod::Npm.upgrade_advice("0.3.0"),
            "npm install -g devlanes@latest"
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
    fn old_copies_are_removed_and_other_files_kept() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        for name in [
            "devlanes.exe",
            "devlanes.exe.old-20261022120000",
            "devlanesw.exe.old-20261022120000-1",
            "devlanes-hosts-service.exe.old-20261022120000",
            "devlanes.exe.old-",
            "other.exe.old-20261022120000",
            "LICENSE",
        ] {
            std::fs::write(dir.path().join(name), "x")?;
        }
        let copies = remove_old_copies(dir.path());
        assert_eq!(
            copies.removed,
            [
                dir.path()
                    .join("devlanes-hosts-service.exe.old-20261022120000"),
                dir.path().join("devlanes.exe.old-20261022120000"),
                dir.path().join("devlanesw.exe.old-20261022120000-1"),
            ]
        );
        assert!(copies.in_use.is_empty());
        let mut left: Vec<String> = std::fs::read_dir(dir.path())?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        left.sort();
        assert_eq!(
            left,
            [
                "LICENSE",
                "devlanes.exe",
                "devlanes.exe.old-",
                "other.exe.old-20261022120000"
            ]
        );
        assert_eq!(
            remove_old_copies(&dir.path().join("missing")),
            OldCopies::default()
        );
        Ok(())
    }

    /// A copy held open without delete sharing, as a running image is, stays
    /// and is reported.
    #[cfg(windows)]
    #[test]
    fn old_copies_in_use_are_kept() -> std::io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir()?;
        let old = dir.path().join("devlanes.exe.old-20261022120000");
        std::fs::write(&old, "x")?;
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&old)?;
        let copies = remove_old_copies(dir.path());
        assert_eq!(copies.in_use, std::slice::from_ref(&old));
        assert!(old.exists());
        drop(held);
        assert_eq!(remove_old_copies(dir.path()).removed, [old]);
        Ok(())
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
