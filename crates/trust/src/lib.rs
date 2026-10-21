//! Contract for installing the devlanes CA into OS and browser trust stores.
//!
//! Trust changes run in the CLI process because they prompt (the macOS keychain
//! dialog, a sudo password). The daemon never calls a [`TrustStore`].
//!
//! Platform crates (`devlanes-trust-macos`, `devlanes-trust-linux`) implement the
//! stores. Every external command goes through a [`CommandExecutor`], so tests
//! check argv without touching real stores. The NSS implementation used by
//! Firefox and Chromium lives here because both platforms share it.

mod cert;
mod error;
mod exec;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod nss;

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;

pub use cert::CaCertificate;
pub use error::TrustError;
pub use exec::{
    CommandExecutor, CommandLine, CommandOutput, SystemExecutor, TempPem, path_arg, run,
    run_checked,
};

/// A store a user can ask `devlanes trust` to install the CA into.
///
/// Mirrors `tls.trust` in the user config; the composition root maps between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrustTarget {
    /// The OS store: the login keychain on macOS, the distribution CA bundle on Linux.
    System,
    /// Every Firefox profile's NSS database.
    Firefox,
    /// Chrome and Chromium: the keychain on macOS, `~/.pki/nssdb` on Linux.
    Chrome,
    /// Excluded from v1; always [`TrustError::Unsupported`].
    IosSimulator,
    /// Excluded from v1; always [`TrustError::Unsupported`].
    AndroidEmulator,
}

impl TrustTarget {
    /// Every target, in config order.
    pub const ALL: [Self; 5] = [
        Self::System,
        Self::Firefox,
        Self::Chrome,
        Self::IosSimulator,
        Self::AndroidEmulator,
    ];

    /// The kebab-case name used in `tls.trust`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Firefox => "firefox",
            Self::Chrome => "chrome",
            Self::IosSimulator => "ios-simulator",
            Self::AndroidEmulator => "android-emulator",
        }
    }

    /// Whether v1 can install into this target on some platform.
    pub fn is_supported(self) -> bool {
        !matches!(self, Self::IosSimulator | Self::AndroidEmulator)
    }

    /// Fails with [`TrustError::Unsupported`] for targets excluded from v1.
    pub fn ensure_supported(self) -> Result<(), TrustError> {
        if self.is_supported() {
            Ok(())
        } else {
            Err(TrustError::Unsupported { target: self })
        }
    }
}

impl fmt::Display for TrustTarget {
    /// Writes the config name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TrustTarget {
    type Err = TrustError;

    /// Parses the config name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|target| target.as_str() == s)
            .ok_or_else(|| TrustError::UnknownTarget(s.to_owned()))
    }
}

/// Whether a store currently trusts the CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStatus {
    /// The CA is present and trusted everywhere this store covers.
    Installed,
    /// The CA is absent, untrusted, or a different certificate holds its name.
    Missing,
    /// The store could not be inspected, for example because `certutil` is missing.
    Unknown {
        /// Why the status could not be determined.
        reason: String,
    },
}

/// One trust store on this machine.
///
/// Implementations are idempotent: installing an installed CA and removing an
/// absent one both succeed without changes, so the CLI can rerun them freely.
#[async_trait]
pub trait TrustStore: Send + Sync {
    /// The target this store was built for.
    fn target(&self) -> TrustTarget;

    /// Stable identity of the underlying store.
    ///
    /// On macOS `system` and `chrome` share the login keychain; the CLI uses the id
    /// to install there once.
    fn id(&self) -> &'static str;

    /// Human description of what the store changes, for prompts and `doctor`.
    fn describe(&self) -> String;

    /// Adds the CA and marks it trusted for TLS servers.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError>;

    /// Removes the CA, including certificates left under its name by an older CA.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError>;

    /// Reports whether the CA is trusted, without prompting.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError>;
}

/// A fresh devlanes CA PEM for unit tests.
#[cfg(test)]
pub(crate) fn test_ca_pem() -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    // pki refuses group/world-readable parents, which a tempdir root may be.
    let ca = devlanes_pki::CertificateAuthority::load_or_create(&dir.path().join("ca"))
        .expect("create CA");
    ca.certificate_pem().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names_round_trip() {
        for target in TrustTarget::ALL {
            assert_eq!(target.as_str().parse::<TrustTarget>().ok(), Some(target));
        }
        assert!(matches!(
            "safari".parse::<TrustTarget>(),
            Err(TrustError::UnknownTarget(name)) if name == "safari"
        ));
    }

    #[test]
    fn mobile_targets_are_unsupported() {
        assert!(TrustTarget::System.ensure_supported().is_ok());
        for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
            assert!(matches!(
                target.ensure_supported(),
                Err(TrustError::Unsupported { target: t }) if t == target
            ));
        }
    }
}
