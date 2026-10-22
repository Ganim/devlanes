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
//! administrator trusted for every user counts as installed, unless the logical
//! `Disallowed` store also holds it ([`TrustStatus::Distrusted`]). Adds and deletes
//! only touch the user's own registry store, so an elevated run cannot change the
//! machine's. The Win32 calls live in `devlanes-sys`; this crate has no `unsafe`.
//! Empty on other platforms.
#![cfg(windows)]

mod firefox;

use std::io;

use async_trait::async_trait;
use devlanes_sys::windows::cert::{Access, CertError, CertStore, Thumbprint, View};
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore, TrustTarget};

pub use firefox::{ENTERPRISE_ROOTS_PREF, FirefoxDirs, FirefoxStore, enterprise_roots_disabled};

/// The CurrentUser system store browsers trust as roots.
pub const ROOT: &str = "Root";

/// The CurrentUser system store of explicitly distrusted certificates ("Untrusted
/// Certificates" in `certmgr.msc`); a certificate there is not trusted even when a
/// root store holds it.
pub const DISALLOWED: &str = "Disallowed";

/// Store id shared by `system`, `chrome` and `windows`, which all mean the
/// user's root store; also what the `firefox` target changes.
pub const STORE_ID: &str = "windows-current-user-root";

/// Store id of the `firefox` target, which reports on the same store on a line of
/// its own.
pub const FIREFOX_ID: &str = "windows-firefox";

/// Common name `devlanes-pki` gives its CA; older devlanes CAs are found by it.
pub const DEVLANES_CA_SUBJECT: &str = "devlanes local development CA";

/// What [`UserRootStore::location`] calls the real `Root` store.
pub const LOCATION: &str = "Windows user root store";

/// The confirmation every `Root` change needs, short enough for `doctor`'s fix line.
const APPROVAL: &str = "choose Yes in the Windows security warning";

/// Builds the Windows store for each [`TrustTarget`].
pub struct WindowsTrust {
    store: String,
    disallowed: String,
    subject: Option<String>,
    firefox: FirefoxDirs,
}

impl WindowsTrust {
    /// Uses the CurrentUser store `store` (tests pass a scratch store) and looks
    /// for Firefox profiles in `firefox`.
    pub fn new(store: impl Into<String>, firefox: FirefoxDirs) -> Self {
        Self {
            store: store.into(),
            disallowed: DISALLOWED.to_owned(),
            subject: None,
            firefox,
        }
    }

    /// The real [`ROOT`] store and the Firefox profiles of this user.
    pub fn from_env() -> Self {
        Self::new(ROOT, FirefoxDirs::from_env())
    }

    /// Stores also remove certificates whose subject contains `subject` on
    /// uninstall; see [`UserRootStore::with_subject`]. Pass [`DEVLANES_CA_SUBJECT`]
    /// for devlanes' own CA, never for mkcert's.
    #[must_use]
    pub fn with_ca_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Reads distrusted certificates from the CurrentUser store `name` instead of
    /// [`DISALLOWED`]; tests pass a scratch store.
    #[must_use]
    pub fn with_disallowed(mut self, name: impl Into<String>) -> Self {
        self.disallowed = name.into();
        self
    }

    /// The store for `target`; mobile targets are [`TrustError::Unsupported`].
    /// `windows`, which names this store from WSL, is the user root store here
    /// too, so a config shared with a WSL distro works on Windows unchanged.
    pub fn store(&self, target: TrustTarget) -> Result<Box<dyn TrustStore>, TrustError> {
        let system = |target| {
            let store = UserRootStore::new(&self.store, target).with_disallowed(&self.disallowed);
            match &self.subject {
                Some(subject) => store.with_subject(subject),
                None => store,
            }
        };
        match target {
            TrustTarget::System | TrustTarget::Chrome | TrustTarget::Windows => {
                Ok(Box::new(system(target)))
            }
            TrustTarget::Firefox => Ok(Box::new(FirefoxStore::new(
                system(TrustTarget::Firefox),
                self.firefox.clone(),
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

impl Change {
    /// The devlanes command that makes this change, for the user to rerun.
    fn command(self) -> &'static str {
        match self {
            Self::Add => "devlanes certs trust",
            Self::Delete => "devlanes certs untrust",
        }
    }

    /// The title of the dialog crypt32 shows for it.
    fn dialog(self) -> &'static str {
        match self {
            Self::Add => "\"Security Warning\"",
            Self::Delete => "\"Root Certificate Store\"",
        }
    }
}

/// Where a CA stands in the logical stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    /// No root store this user trusts holds it.
    Missing,
    /// A root store holds it and the `Disallowed` store does not.
    Trusted,
    /// A root store holds it, but so does the `Disallowed` store.
    Denied,
}

/// One CurrentUser certificate store, [`ROOT`] in production.
pub struct UserRootStore {
    name: String,
    disallowed: String,
    target: TrustTarget,
    subject: Option<String>,
}

impl UserRootStore {
    /// Manages the CurrentUser store `name` for `target`, with [`DISALLOWED`] as
    /// the deny list.
    pub fn new(name: impl Into<String>, target: TrustTarget) -> Self {
        Self {
            name: name.into(),
            disallowed: DISALLOWED.to_owned(),
            target,
            subject: None,
        }
    }

    /// On uninstall, also removes every certificate whose subject contains
    /// `subject` from the user's own store (older CAs left by a regenerated
    /// `$DEVLANES_HOME/ca`). Install never removes anything. Leave unset for CAs
    /// devlanes does not own, such as mkcert's.
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Reads the deny list from the CurrentUser store `name` instead of [`DISALLOWED`].
    #[must_use]
    pub fn with_disallowed(mut self, name: impl Into<String>) -> Self {
        self.disallowed = name.into();
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

    /// Whether a change would fail for want of a desktop to show the dialog on.
    /// An unreadable window station counts as a desktop: Windows then tries, and a
    /// dialog it cannot show comes back as an error.
    fn lacks_desktop(&self) -> bool {
        self.prompts()
            && matches!(
                devlanes_sys::windows::cert::interactive_desktop(),
                Ok(false)
            )
    }

    /// Fails before a change the dialog could not be shown for.
    fn ensure_desktop(&self, change: Change) -> Result<(), TrustError> {
        if self.lacks_desktop() {
            return Err(self.no_desktop(change));
        }
        Ok(())
    }

    /// What `change` does, for messages.
    fn action(&self, change: Change) -> String {
        match change {
            Change::Add => format!("adding the CA to {}", self.path()),
            Change::Delete => format!("removing a CA from {}", self.path()),
        }
    }

    /// [`TrustError::InteractionRequired`] for `change`.
    fn no_desktop(&self, change: Change) -> TrustError {
        TrustError::InteractionRequired {
            command: change.command().to_owned(),
            detail: format!(
                "{} shows a Windows {} dialog on the desktop; run it from a logged-in \
                 desktop session, not over SSH or from a service",
                self.action(change),
                change.dialog()
            ),
        }
    }

    /// Maps a failed change to the error the CLI shows.
    fn change_error(&self, change: Change, error: CertError) -> TrustError {
        let action = self.action(change);
        match error {
            CertError::Cancelled => TrustError::Cancelled {
                detail: format!(
                    "{action} was declined in the Windows {} dialog; run `{}` again and \
                     choose Yes",
                    change.dialog(),
                    change.command()
                ),
            },
            CertError::NoDesktop => self.no_desktop(change),
            CertError::Io(error) => TrustError::io(action, error),
        }
    }

    /// Where `thumbprint` stands in the logical stores, machine roots included;
    /// `Err` carries the reason a store could not be read.
    async fn lookup(&self, thumbprint: Thumbprint) -> Result<Result<Lookup, String>, TrustError> {
        let name = self.name.clone();
        let disallowed = self.disallowed.clone();
        blocking("reading the certificate store", move || {
            let holds = |store: &str| -> Result<bool, String> {
                let path = format!(r"Cert:\CurrentUser\{store}");
                match CertStore::open(store, View::Logical, Access::Read) {
                    Ok(opened) => opened
                        .contains(&thumbprint)
                        .map_err(|error| format!("{path}: {error}")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(format!("{path}: {error}")),
                }
            };
            Ok(if !holds(&name)? {
                Lookup::Missing
            } else if holds(&disallowed)? {
                Lookup::Denied
            } else {
                Lookup::Trusted
            })
        })
        .await
    }

    /// Certificates in the user's own store to delete: `ca` when present and, with
    /// a subject, every other match.
    async fn doomed(&self, ca: Thumbprint) -> Result<Vec<Thumbprint>, TrustError> {
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
            if store.contains(&ca)? {
                doomed.push(ca);
            }
            if let Some(subject) = subject {
                for found in store.thumbprints_with_subject(&subject)? {
                    if !doomed.contains(&found) {
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
    /// `system`, `chrome` or `windows`, as requested.
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

    /// Choosing Yes in the security warning, for `Root` only.
    fn approval(&self) -> Option<String> {
        self.prompts().then(|| APPROVAL.to_owned())
    }

    /// The dialog `install` or `uninstall` is about to show: none when the CA is
    /// already in a root store, when there is nothing to delete, or without a
    /// desktop (the change then fails with [`TrustError::InteractionRequired`]).
    async fn pending_approval(&self, ca: &CaCertificate, install: bool) -> Option<String> {
        if !self.prompts() || self.lacks_desktop() {
            return None;
        }
        let thumbprint = thumbprint(ca).ok()?;
        if install {
            // An unreadable store gets the add, so it gets the notice too.
            if let Ok(Ok(Lookup::Trusted | Lookup::Denied)) = self.lookup(thumbprint).await {
                return None;
            }
            return Some(format!(
                "Windows asks you to confirm adding the CA to your user root store in a \
                 {} dialog; choose Yes",
                Change::Add.dialog()
            ));
        }
        let what = match self.doomed(thumbprint).await {
            Ok(doomed) if doomed.is_empty() => return None,
            Ok(doomed) if doomed.len() > 1 => {
                format!("each of the {} CAs devlanes removes", doomed.len())
            }
            Ok(_) | Err(_) => "removing the CA".to_owned(),
        };
        Some(format!(
            "Windows asks you to confirm {what} from your user root store in a {} dialog; \
             choose Yes",
            Change::Delete.dialog()
        ))
    }

    /// Adds the CA to the user's own store unless a root store this user trusts
    /// already holds it. Older devlanes CAs stay until `uninstall`, as on macOS:
    /// replacing the CA runs `certs untrust` first.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let thumbprint = thumbprint(ca)?;
        if let Ok(Lookup::Trusted | Lookup::Denied) = self.lookup(thumbprint).await? {
            return Ok(());
        }
        self.ensure_desktop(Change::Add)?;
        let name = self.name.clone();
        let der = ca.der().to_vec();
        blocking(
            "changing the certificate store",
            move || -> Result<(), CertError> {
                // The registry view: when elevated, a write through the logical
                // store could land in the machine's `Root`.
                CertStore::open(&name, View::UserRegistry, Access::Write)?.add_der(&der)
            },
        )
        .await?
        .map_err(|error| self.change_error(Change::Add, error))
    }

    /// Deletes the CA and, with a subject, every devlanes CA from the user's own
    /// store. A CA still trusted through the logical store afterwards comes from a
    /// store only an administrator can change: [`TrustError::StillTrusted`].
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let thumbprint = thumbprint(ca)?;
        let doomed = self.doomed(thumbprint).await?;
        self.delete(doomed).await?;
        match self.lookup(thumbprint).await? {
            Ok(Lookup::Trusted) => Err(TrustError::StillTrusted {
                through: format!(r"Cert:\LocalMachine\{} or a group policy store", self.name),
            }),
            Ok(Lookup::Missing | Lookup::Denied) | Err(_) => Ok(()),
        }
    }

    /// Installed when a logical root store, machine roots included, holds the CA
    /// and the logical [`DISALLOWED`] store does not.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        Ok(match self.lookup(thumbprint(ca)?).await? {
            Ok(Lookup::Trusted) => TrustStatus::Installed,
            Ok(Lookup::Missing) => TrustStatus::Missing,
            Ok(Lookup::Denied) => TrustStatus::Distrusted {
                reason: format!(
                    r"Cert:\CurrentUser\{0} (Untrusted Certificates in certmgr.msc) also holds it, which overrides the root store; remove it there, or have an administrator remove it from Cert:\LocalMachine\{0}",
                    self.disallowed
                ),
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A No names the dialog of that change and the command to rerun.
    #[test]
    fn declined_dialogs_name_their_dialog_and_command() {
        let root = UserRootStore::new(ROOT, TrustTarget::System);
        let detail = |change| match root.change_error(change, CertError::Cancelled) {
            TrustError::Cancelled { detail } => detail,
            other => panic!("{other:?}"),
        };
        let add = detail(Change::Add);
        assert!(add.contains("\"Security Warning\""), "{add}");
        assert!(add.contains("`devlanes certs trust`"), "{add}");
        let delete = detail(Change::Delete);
        assert!(delete.contains("\"Root Certificate Store\""), "{delete}");
        assert!(delete.contains("`devlanes certs untrust`"), "{delete}");
    }

    /// Without a desktop the error carries the devlanes command to rerun.
    #[test]
    fn no_desktop_names_the_devlanes_command() {
        let root = UserRootStore::new(ROOT, TrustTarget::System);
        for (change, command, dialog) in [
            (Change::Add, "devlanes certs trust", "\"Security Warning\""),
            (
                Change::Delete,
                "devlanes certs untrust",
                "\"Root Certificate Store\"",
            ),
        ] {
            let error = root.change_error(change, CertError::NoDesktop);
            let TrustError::InteractionRequired {
                command: got,
                detail,
            } = &error
            else {
                panic!("{error:?}");
            };
            assert_eq!(got, command);
            assert!(detail.contains(dialog), "{detail}");
            assert!(error.to_string().starts_with(&format!("`{command}`")));
        }
    }
}
