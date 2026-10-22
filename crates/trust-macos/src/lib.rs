//! macOS trust stores: the login keychain for the system and Chrome, NSS for Firefox.
//!
//! The CA goes into the user's login keychain with user-domain trust settings, so no
//! sudo is needed. macOS still asks for the account password in a GUI dialog
//! ("security wants to make changes to your Certificate Trust Settings") on install
//! and removal; over SSH without a desktop session the change fails with
//! [`TrustError::InteractionRequired`]. Chrome and Safari read these settings.
//! Firefox 120 and later also imports user-added keychain roots, and the `firefox`
//! target additionally writes each profile's NSS database when `certutil` exists.
//!
//! Empty on Windows. It builds on Linux so the argv tests run there too.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use devlanes_trust::nss::{CertutilLookup, NssDatabases, NssStore};
use devlanes_trust::{
    CaCertificate, CommandExecutor, CommandLine, CommandOutput, SystemExecutor, TempPem,
    TrustError, TrustStatus, TrustStore, TrustTarget, path_arg, run,
};

/// Absolute path so a `security` earlier on `PATH` cannot intercept trust changes.
pub const SECURITY: &str = "/usr/bin/security";

/// Shown when the `firefox` target needs `certutil`.
pub const CERTUTIL_HINT: &str = "install NSS tools with `brew install nss`";

/// Shown if `/usr/bin/security` is somehow missing.
const SECURITY_HINT: &str = "it ships with macOS; check that /usr/bin/security exists";

/// `errSecItemNotFound`, returned by `find-certificate` when nothing matches.
const ITEM_NOT_FOUND: i32 = 44;

/// Common name `devlanes-pki` gives its CA, which macOS uses as the keychain label.
pub const DEVLANES_CA_LABEL: &str = "devlanes local development CA";

/// Store id shared by `system` and `chrome`, which both use the login keychain.
pub const KEYCHAIN_ID: &str = "macos-login-keychain";

/// Store id of the Firefox profiles.
pub const FIREFOX_ID: &str = "nss-firefox";

/// Builds the macOS store for each [`TrustTarget`].
pub struct MacosTrust {
    executor: Arc<dyn CommandExecutor>,
    home: PathBuf,
    label: Option<String>,
}

impl MacosTrust {
    /// Uses `executor` for every command and `home` for the keychain and profiles.
    pub fn new(executor: Arc<dyn CommandExecutor>, home: impl Into<PathBuf>) -> Self {
        Self {
            executor,
            home: home.into(),
            label: None,
        }
    }

    /// Keychain stores also remove certificates labelled `label` on uninstall; see
    /// [`KeychainStore::with_label`]. Pass [`DEVLANES_CA_LABEL`] for devlanes' own CA.
    #[must_use]
    pub fn with_ca_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Real commands and `$HOME`.
    pub fn from_env() -> Result<Self, TrustError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            TrustError::io(
                "finding the home directory",
                std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"),
            )
        })?;
        Ok(Self::new(Arc::new(SystemExecutor), PathBuf::from(home)))
    }

    /// The store for `target`; mobile targets are [`TrustError::Unsupported`].
    pub fn store(&self, target: TrustTarget) -> Result<Box<dyn TrustStore>, TrustError> {
        match target {
            TrustTarget::System | TrustTarget::Chrome => {
                let store =
                    KeychainStore::new(self.executor.clone(), login_keychain(&self.home), target);
                Ok(Box::new(match &self.label {
                    Some(label) => store.with_label(label),
                    None => store,
                }))
            }
            TrustTarget::Firefox => Ok(Box::new(NssStore::new(
                self.executor.clone(),
                TrustTarget::Firefox,
                FIREFOX_ID,
                NssDatabases::Profiles {
                    roots: firefox_profile_roots(&self.home),
                },
                certutil_lookup(),
            ))),
            TrustTarget::IosSimulator | TrustTarget::AndroidEmulator => {
                Err(TrustError::Unsupported { target })
            }
        }
    }
}

/// `~/Library/Keychains/login.keychain-db`.
pub fn login_keychain(home: &Path) -> PathBuf {
    home.join("Library/Keychains/login.keychain-db")
}

/// Where Firefox keeps profiles on macOS.
pub fn firefox_profile_roots(home: &Path) -> Vec<PathBuf> {
    vec![home.join("Library/Application Support/Firefox/Profiles")]
}

/// `certutil` lookup including Homebrew's `nss` prefixes, which are not always linked.
pub fn certutil_lookup() -> CertutilLookup {
    CertutilLookup {
        extra_dirs: vec![
            PathBuf::from("/opt/homebrew/opt/nss/bin"),
            PathBuf::from("/usr/local/opt/nss/bin"),
        ],
        hint: CERTUTIL_HINT.into(),
    }
}

/// What the keychain knows about the CA.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeychainState {
    /// Not in the keychain.
    Absent,
    /// In the keychain, but certificate evaluation does not trust it.
    Untrusted,
    /// In the keychain and trusted as a root.
    Trusted,
    /// The keychain could not be read.
    Unknown(String),
}

/// A keychain plus user-domain trust settings, driven by `/usr/bin/security`.
pub struct KeychainStore {
    executor: Arc<dyn CommandExecutor>,
    keychain: PathBuf,
    target: TrustTarget,
    label: Option<String>,
}

impl KeychainStore {
    /// Manages `keychain`; tests pass a throwaway keychain here.
    pub fn new(
        executor: Arc<dyn CommandExecutor>,
        keychain: impl Into<PathBuf>,
        target: TrustTarget,
    ) -> Self {
        Self {
            executor,
            keychain: keychain.into(),
            target,
            label: None,
        }
    }

    /// The keychain file this store changes.
    pub fn keychain(&self) -> &Path {
        &self.keychain
    }

    /// Runs `security` with `args`, mapping a missing binary to a hint.
    async fn security(&self, command: &CommandLine) -> Result<CommandOutput, TrustError> {
        run(self.executor.as_ref(), command, SECURITY_HINT).await
    }

    /// Also removes, on uninstall, every certificate in the keychain whose label
    /// contains `label` (for example older devlanes CAs left by a regenerated
    /// `$DEVLANES_HOME/ca`). Leave unset for CAs devlanes does not own, such as mkcert's.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// SHA-1 hashes of the keychain's certificates, optionally only those whose
    /// label contains `label`; `Err` carries the reason the keychain was unreadable.
    async fn hashes(&self, label: Option<&str>) -> Result<Result<Vec<String>, String>, TrustError> {
        let mut find = CommandLine::new(SECURITY).args(["find-certificate", "-a"]);
        if let Some(label) = label {
            find = find.arg("-c").arg(label);
        }
        let find = find.arg("-Z").arg(path_arg(&self.keychain)?);
        let output = self.security(&find).await?;
        Ok(match output.code {
            Some(0) => Ok(sha1_hashes(&output.stdout)),
            Some(ITEM_NOT_FOUND) => Ok(Vec::new()),
            _ => Err(format!(
                "{}: {}",
                self.keychain.display(),
                output.stderr.trim()
            )),
        })
    }

    /// Checks presence by SHA-1, then trust by evaluating the CA as its own chain.
    async fn state(&self, ca: &CaCertificate) -> Result<KeychainState, TrustError> {
        let present = match self.hashes(None).await? {
            Ok(hashes) => hashes
                .iter()
                .any(|hash| hash.eq_ignore_ascii_case(&ca.sha1_hex())),
            Err(reason) => return Ok(KeychainState::Unknown(reason)),
        };
        if !present {
            return Ok(KeychainState::Absent);
        }
        let pem = TempPem::create(ca).await?;
        // -l: the leaf is a CA; -L: never fetch issuers from the network; -q: quiet.
        let verify =
            CommandLine::new(SECURITY).args(["verify-cert", "-c", pem.arg(), "-l", "-L", "-q"]);
        let output = self.security(&verify).await?;
        pem.close().await;
        Ok(if output.success() {
            KeychainState::Trusted
        } else {
            KeychainState::Untrusted
        })
    }

    /// Runs a command that may show the authorization dialog.
    async fn change(&self, command: &CommandLine) -> Result<CommandOutput, TrustError> {
        let output = self.security(command).await?;
        if output.success() {
            return Ok(output);
        }
        if let Some(detail) = interaction_problem(&output.stderr) {
            return Err(TrustError::InteractionRequired {
                command: command.to_string(),
                detail: detail.into(),
            });
        }
        Ok(output)
    }
}

#[async_trait]
impl TrustStore for KeychainStore {
    /// `system` or `chrome`, as requested.
    fn target(&self) -> TrustTarget {
        self.target
    }

    /// [`KEYCHAIN_ID`] for both targets.
    fn id(&self) -> &'static str {
        KEYCHAIN_ID
    }

    /// Names the keychain and warns about the password dialog.
    fn describe(&self) -> String {
        format!(
            "keychain {} (macOS asks for your password in a dialog)",
            self.keychain.display()
        )
    }

    /// `security add-trusted-cert -r trustRoot -k <keychain> <pem>`.
    ///
    /// Without `-d` the trust settings go to the user domain, so no sudo is needed.
    /// A certificate already in the keychain only gets its trust settings fixed.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let state = self.state(ca).await?;
        let mut command = CommandLine::new(SECURITY).args(["add-trusted-cert", "-r", "trustRoot"]);
        match state {
            KeychainState::Trusted => return Ok(()),
            KeychainState::Untrusted => {}
            // An unreadable keychain still gets the attempt: add-trusted-cert reports
            // the real problem better than the probe did.
            KeychainState::Absent | KeychainState::Unknown(_) => {
                command = command.arg("-k").arg(path_arg(&self.keychain)?);
            }
        }
        let pem = TempPem::create(ca).await?;
        let command = command.arg(pem.arg());
        self.change(&command).await?.into_checked(&command)?;
        pem.close().await;
        Ok(())
    }

    /// `security delete-certificate -Z <sha1> -t <keychain>` for the CA and, with a
    /// label, every other certificate whose label contains it (found with
    /// `find-certificate -a -c <label> -Z`). `-t` also drops the user trust
    /// settings, so nothing stays trusted after the certificate is gone.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let mut doomed = BTreeSet::new();
        match self.hashes(None).await? {
            Ok(hashes)
                if hashes
                    .iter()
                    .any(|h| h.eq_ignore_ascii_case(&ca.sha1_hex())) =>
            {
                doomed.insert(ca.sha1_hex());
            }
            Ok(_) => {}
            // Let delete-certificate report the real problem.
            Err(_) => {
                doomed.insert(ca.sha1_hex());
            }
        }
        if let Some(label) = &self.label {
            match self.hashes(Some(label)).await? {
                Ok(hashes) => doomed.extend(hashes.into_iter().map(|h| h.to_ascii_uppercase())),
                Err(reason) => {
                    return Err(TrustError::CommandFailed {
                        command: format!("{SECURITY} find-certificate -a -c '{label}' -Z"),
                        code: None,
                        stderr: reason,
                    });
                }
            }
        }
        for hash in doomed {
            let command = CommandLine::new(SECURITY)
                .args(["delete-certificate", "-Z"])
                .arg(hash)
                .arg("-t")
                .arg(path_arg(&self.keychain)?);
            let output = self.change(&command).await?;
            // Deleted between the probe and now: the goal is reached.
            if !output.success()
                && !output
                    .stderr
                    .contains("Unable to delete certificate matching")
            {
                output.into_checked(&command)?;
            }
        }
        Ok(())
    }

    /// Installed only when the CA is in the keychain and evaluates as trusted.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        Ok(match self.state(ca).await? {
            KeychainState::Trusted => TrustStatus::Installed,
            KeychainState::Absent | KeychainState::Untrusted => TrustStatus::Missing,
            KeychainState::Unknown(reason) => TrustStatus::Unknown { reason },
        })
    }
}

/// SHA-1 hashes listed by `find-certificate -Z`.
fn sha1_hashes(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("SHA-1 hash:"))
        .map(|hash| hash.trim().to_owned())
        .filter(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect()
}

/// Explains failures caused by the authorization dialog.
fn interaction_problem(stderr: &str) -> Option<&'static str> {
    if stderr.contains("no user interaction was possible") {
        Some("macOS needs your password in a GUI dialog; run this from a logged-in desktop session")
    } else if stderr.contains("canceled by the user") || stderr.contains("cancelled by the user") {
        Some("the password dialog was cancelled")
    } else {
        None
    }
}
