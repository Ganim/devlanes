//! Windows trust store: the current user's `Root` certificate store.
//!
//! Chrome and Edge verify against the Windows store, and Firefox imports its roots
//! (`security.enterprise_roots.enabled`, on by default), so `system`, `chrome` and
//! `firefox` all land in one store and no elevation is needed. Adding a root there
//! or deleting one makes Windows show a security warning that the user must
//! confirm, the counterpart of the macOS keychain dialog. Without a desktop (SSH,
//! services) the change fails with [`TrustError::InteractionRequired`]; a No in
//! the dialog is [`TrustError::Cancelled`].
//!
//! Lookups use the logical store, which also holds the machine's roots, so a CA an
//! administrator trusted for every user counts as installed. Deletes only touch
//! the user's own registry store. The Win32 calls live in `devlanes-sys`; this
//! crate has no `unsafe`. Empty on other platforms.
#![cfg(windows)]

mod firefox;

use std::io;

use async_trait::async_trait;
use devlanes_sys::windows::cert::{Access, CertError, CertStore, Thumbprint, View};
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore, TrustTarget};

pub use firefox::{
    ENTERPRISE_ROOTS_PREF, FirefoxStore, enterprise_roots_disabled, firefox_profile_roots,
};

/// The CurrentUser system store browsers trust as roots.
pub const ROOT: &str = "Root";

/// Store id shared by `system` and `chrome`, which both mean the user's root store.
pub const STORE_ID: &str = "windows-current-user-root";

/// Store id of the `firefox` target, which reports on the same store.
pub const FIREFOX_ID: &str = "windows-firefox";

/// Common name `devlanes-pki` gives its CA; older devlanes CAs are found by it.
pub const DEVLANES_CA_SUBJECT: &str = "devlanes local development CA";

/// What [`UserRootStore::location`] calls the real `Root` store.
pub const LOCATION: &str = "Windows user root store";

/// The dialogs Windows shows for `Root` changes, from crypt32's own strings.
const APPROVAL: &str = "Windows asks you to confirm each certificate devlanes adds to or \
     deletes from your user root store (a \"Security Warning\" to install, a \"Root \
     Certificate Store\" prompt to delete); choose Yes";

/// Builds the Windows store for each [`TrustTarget`].
pub struct WindowsTrust {
    store: String,
    subject: Option<String>,
    firefox_roots: Vec<std::path::PathBuf>,
}

impl WindowsTrust {
    /// Uses the CurrentUser store `store` (tests pass a scratch store) and looks
    /// for Firefox profiles under `firefox_roots`.
    pub fn new(store: impl Into<String>, firefox_roots: Vec<std::path::PathBuf>) -> Self {
        Self {
            store: store.into(),
            subject: None,
            firefox_roots,
        }
    }

    /// The real [`ROOT`] store and the Firefox profiles under `%APPDATA%`.
    pub fn from_env() -> Self {
        let roots = std::env::var_os("APPDATA")
            .map(|appdata| firefox_profile_roots(std::path::Path::new(&appdata)))
            .unwrap_or_default();
        Self::new(ROOT, roots)
    }

    /// Stores also remove certificates whose subject contains `subject`; see
    /// [`UserRootStore::with_subject`]. Pass [`DEVLANES_CA_SUBJECT`] for devlanes'
    /// own CA, never for mkcert's.
    #[must_use]
    pub fn with_ca_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// The store for `target`; mobile targets are [`TrustError::Unsupported`].
    pub fn store(&self, target: TrustTarget) -> Result<Box<dyn TrustStore>, TrustError> {
        let system = |target| {
            let store = UserRootStore::new(&self.store, target);
            match &self.subject {
                Some(subject) => store.with_subject(subject),
                None => store,
            }
        };
        match target {
            TrustTarget::System | TrustTarget::Chrome => Ok(Box::new(system(target))),
            TrustTarget::Firefox => Ok(Box::new(FirefoxStore::new(
                system(TrustTarget::Firefox),
                self.firefox_roots.clone(),
            ))),
            TrustTarget::IosSimulator | TrustTarget::AndroidEmulator => {
                Err(TrustError::Unsupported { target })
            }
        }
    }
}

/// SHA-1 thumbprint of `ca`, the key Windows stores certificates under.
pub fn thumbprint(ca: &CaCertificate) -> Result<Thumbprint, TrustError> {
    let hex = ca.sha1_hex();
    let mut thumbprint: Thumbprint = [0; 20];
    for (index, byte) in thumbprint.iter_mut().enumerate() {
        *byte = hex
            .get(index * 2..index * 2 + 2)
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .ok_or_else(|| TrustError::InvalidCertificate(format!("bad SHA-1 `{hex}`")))?;
    }
    Ok(thumbprint)
}

/// A change that may show the confirmation dialog.
#[derive(Debug, Clone, Copy)]
enum Change {
    Add,
    Delete,
}

/// One CurrentUser certificate store, [`ROOT`] in production.
pub struct UserRootStore {
    name: String,
    target: TrustTarget,
    subject: Option<String>,
}

impl UserRootStore {
    /// Manages the CurrentUser store `name` for `target`.
    pub fn new(name: impl Into<String>, target: TrustTarget) -> Self {
        Self {
            name: name.into(),
            target,
            subject: None,
        }
    }

    /// Also removes certificates whose subject contains `subject` from the user's
    /// own store: older ones on install (a regenerated `$DEVLANES_HOME/ca` leaves
    /// them trusted otherwise), all of them on uninstall. Leave unset for CAs
    /// devlanes does not own, such as mkcert's.
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// The store name passed to `CertOpenStore`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Only the user's `Root` shows the confirmation dialog.
    fn prompts(&self) -> bool {
        self.name.eq_ignore_ascii_case(ROOT)
    }

    /// The PowerShell path of the store, for messages.
    fn path(&self) -> String {
        format!(r"Cert:\CurrentUser\{}", self.name)
    }

    /// Fails before a change the dialog could not be shown for.
    fn ensure_desktop(&self, change: Change) -> Result<(), TrustError> {
        if !self.prompts() {
            return Ok(());
        }
        match devlanes_sys::windows::cert::interactive_desktop() {
            Ok(false) => Err(self.no_desktop(change)),
            // Unknown: let Windows try; a dialog it cannot show comes back as an error.
            Ok(true) | Err(_) => Ok(()),
        }
    }

    /// [`TrustError::InteractionRequired`] for `change`.
    fn no_desktop(&self, change: Change) -> TrustError {
        let verb = match change {
            Change::Add => "add to",
            Change::Delete => "delete from",
        };
        TrustError::InteractionRequired {
            command: format!("{verb} {}", self.path()),
            detail: "Windows asks for confirmation in a security warning on the desktop; run \
                     this from a logged-in desktop session, not over SSH or from a service"
                .into(),
        }
    }

    /// Maps a failed change to the error the CLI shows.
    fn change_error(&self, change: Change, error: CertError) -> TrustError {
        let what = match change {
            Change::Add => format!("adding the CA to {}", self.path()),
            Change::Delete => format!("removing a devlanes CA from {}", self.path()),
        };
        match error {
            CertError::Cancelled => TrustError::Cancelled {
                detail: format!(
                    "{what} was declined in the Windows security warning; run the command \
                     again and choose Yes"
                ),
            },
            CertError::NoDesktop => self.no_desktop(change),
            CertError::Io(error) => TrustError::io(what, error),
        }
    }

    /// Whether the logical store (machine roots included) holds `thumbprint`;
    /// `Err` carries the reason it could not be read.
    async fn trusted(&self, thumbprint: Thumbprint) -> Result<Result<bool, String>, TrustError> {
        let name = self.name.clone();
        let path = self.path();
        blocking("reading the certificate store", move || {
            let store = match CertStore::open(&name, View::Logical, Access::Read) {
                Ok(store) => store,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(format!("{path}: {error}")),
            };
            store
                .contains(&thumbprint)
                .map_err(|error| format!("{path}: {error}"))
        })
        .await
    }

    /// Certificates in the user's own store to delete: `ca` when present and, with
    /// a subject, every match except `keep`.
    async fn doomed(
        &self,
        ca: Option<Thumbprint>,
        keep: Option<Thumbprint>,
    ) -> Result<Vec<Thumbprint>, TrustError> {
        let name = self.name.clone();
        let subject = self.subject.clone();
        let path = self.path();
        blocking("reading the certificate store", move || -> io::Result<_> {
            let store = match CertStore::open(&name, View::UserRegistry, Access::Read) {
                Ok(store) => store,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            let mut doomed = Vec::new();
            if let Some(ca) = ca
                && store.contains(&ca)?
            {
                doomed.push(ca);
            }
            if let Some(subject) = subject {
                for found in store.thumbprints_with_subject(&subject)? {
                    if Some(found) != keep && !doomed.contains(&found) {
                        doomed.push(found);
                    }
                }
            }
            Ok(doomed)
        })
        .await?
        .map_err(|error| TrustError::io(format!("reading {path}"), error))
    }

    /// Deletes `doomed` from the user's own store, one dialog each for `Root`.
    async fn delete(&self, doomed: Vec<Thumbprint>) -> Result<(), TrustError> {
        if doomed.is_empty() {
            return Ok(());
        }
        self.ensure_desktop(Change::Delete)?;
        let name = self.name.clone();
        blocking(
            "changing the certificate store",
            move || -> Result<(), CertError> {
                let store = CertStore::open(&name, View::UserRegistry, Access::Write)?;
                for thumbprint in &doomed {
                    // Absent means it went away since the listing: the goal is reached.
                    store.delete(thumbprint)?;
                }
                Ok(())
            },
        )
        .await?
        .map_err(|error| self.change_error(Change::Delete, error))
    }
}

#[async_trait]
impl TrustStore for UserRootStore {
    /// `system` or `chrome`, as requested.
    fn target(&self) -> TrustTarget {
        self.target
    }

    /// [`STORE_ID`] for both targets.
    fn id(&self) -> &'static str {
        STORE_ID
    }

    /// Names the store and, for `Root`, the dialog.
    fn describe(&self) -> String {
        if self.prompts() {
            format!(
                "{LOCATION} ({}; Windows shows a security warning to confirm each change)",
                self.path()
            )
        } else {
            self.path()
        }
    }

    /// [`LOCATION`] for `Root`, the store path otherwise.
    fn location(&self) -> Option<String> {
        Some(if self.prompts() {
            LOCATION.to_owned()
        } else {
            self.path()
        })
    }

    /// The security warning and the delete prompt, for `Root` only.
    fn approval(&self) -> Option<String> {
        self.prompts().then(|| APPROVAL.to_owned())
    }

    /// Adds the CA unless the store already trusts it, then removes older CAs
    /// with the same subject. The add comes first, so a declined dialog changes
    /// nothing, and older CAs only go once the new one is trusted.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let thumbprint = thumbprint(ca)?;
        if self.trusted(thumbprint).await? != Ok(true) {
            self.ensure_desktop(Change::Add)?;
            let name = self.name.clone();
            let der = ca.der().to_vec();
            blocking(
                "changing the certificate store",
                move || -> Result<(), CertError> {
                    CertStore::open(&name, View::Logical, Access::Write)?.add_der(&der)
                },
            )
            .await?
            .map_err(|error| self.change_error(Change::Add, error))?;
        }
        if self.subject.is_some() {
            let older = self.doomed(None, Some(thumbprint)).await?;
            self.delete(older).await?;
        }
        Ok(())
    }

    /// Deletes the CA and, with a subject, every devlanes CA from the user's own
    /// store. A CA that stays trusted through the machine's `Root` is an error:
    /// only an administrator can remove it there.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let thumbprint = thumbprint(ca)?;
        let doomed = self.doomed(Some(thumbprint), None).await?;
        self.delete(doomed).await?;
        match self.trusted(thumbprint).await? {
            Ok(true) => Err(TrustError::io(
                format!("removing the CA from {}", self.path()),
                io::Error::other(
                    r"it is still trusted through the machine root store (Cert:\LocalMachine\Root), which only an administrator can change",
                ),
            )),
            Ok(false) | Err(_) => Ok(()),
        }
    }

    /// Installed when the logical store, machine roots included, holds the CA.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        Ok(match self.trusted(thumbprint(ca)?).await? {
            Ok(true) => TrustStatus::Installed,
            Ok(false) => TrustStatus::Missing,
            Err(reason) => TrustStatus::Unknown { reason },
        })
    }
}

/// Runs crypt32 or file work on a blocking thread.
async fn blocking<T: Send + 'static>(
    what: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, TrustError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| TrustError::io(what, io::Error::other(error)))
}
