//! Windows trust store: the current user's `Root` certificate store.
//!
//! Chrome and Edge verify against the Windows store, and Firefox 120 and later
//! reads user-added roots from it (`security.enterprise_roots.enabled`), so
//! `system`, `chrome` and `firefox` all land in one store, changed once per
//! command, and no elevation is needed. Adding a root there or deleting one
//! makes Windows show a security warning the user must confirm, the
//! counterpart of the macOS keychain dialog. Without a desktop (SSH, services)
//! a change fails with [`TrustError::InteractionRequired`] before any dialog;
//! a No in the dialog fails the same way, as on macOS.
//!
//! Lookups use the logical store, which also holds the machine's roots, so a
//! CA an administrator trusted for every user counts as installed, unless the
//! `Disallowed` store ("Untrusted Certificates") also holds it. Adds and
//! deletes only touch the user's own registry store, so an elevated run cannot
//! change the machine's. The Win32 calls live in `devlanes-sys-windows`; this
//! crate has no `unsafe`. Empty on other platforms.
#![cfg(windows)]

mod bundle;

use std::io;

use async_trait::async_trait;
use devlanes_sys_windows::cert::{Access, CertError, CertStore, Thumbprint, View};
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore, TrustTarget};

pub use bundle::{BundleRoots, bundle_roots};

/// The CurrentUser system store browsers trust as roots.
pub const ROOT: &str = "Root";

/// The CurrentUser system store of explicitly distrusted certificates
/// ("Untrusted Certificates" in `certmgr.msc`), which wins over `Root`.
pub const DISALLOWED: &str = "Disallowed";

/// Store id shared by `system`, `chrome` and `firefox`, which all mean the
/// user's root store.
pub const STORE_ID: &str = "windows-user-root";

/// Common name `devlanes-pki` gives its CA; older devlanes CAs are found by it.
pub const DEVLANES_CA_SUBJECT: &str = "devlanes local development CA";

/// Builds the Windows store for each [`TrustTarget`].
#[derive(Debug, Clone)]
pub struct WindowsTrust {
    root: String,
    disallowed: String,
    subject: Option<String>,
}

impl Default for WindowsTrust {
    /// The real [`ROOT`] and [`DISALLOWED`] stores.
    fn default() -> Self {
        Self::with_stores(ROOT, DISALLOWED)
    }
}

impl WindowsTrust {
    /// Uses the CurrentUser stores `root` and `disallowed`; tests pass scratch
    /// stores, which never show a dialog.
    pub fn with_stores(root: impl Into<String>, disallowed: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            disallowed: disallowed.into(),
            subject: None,
        }
    }

    /// Stores also remove, on uninstall, every certificate of the user's own
    /// store whose subject contains `subject`. Pass [`DEVLANES_CA_SUBJECT`] for
    /// devlanes' own CA, never for mkcert's.
    #[must_use]
    pub fn with_ca_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// The store for `target`; mobile targets are [`TrustError::Unsupported`].
    pub fn store(&self, target: TrustTarget) -> Result<Box<dyn TrustStore>, TrustError> {
        match target {
            TrustTarget::System | TrustTarget::Chrome | TrustTarget::Firefox => {
                Ok(Box::new(UserRootStore {
                    root: self.root.clone(),
                    disallowed: self.disallowed.clone(),
                    target,
                    subject: self.subject.clone(),
                }))
            }
            TrustTarget::IosSimulator | TrustTarget::AndroidEmulator => {
                Err(TrustError::Unsupported { target })
            }
        }
    }
}

/// SHA-1 thumbprint of `ca`, the key Windows files certificates under.
fn thumbprint(ca: &CaCertificate) -> Result<Thumbprint, TrustError> {
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

/// Where a CA stands in the logical stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    /// No root store this user trusts holds it.
    Missing,
    /// A root store holds it and `Disallowed` does not.
    Trusted,
    /// A root store holds it, and so does `Disallowed`.
    Distrusted,
}

/// A change to the root store, for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// The user's root store for one target.
pub struct UserRootStore {
    root: String,
    disallowed: String,
    target: TrustTarget,
    subject: Option<String>,
}

impl UserRootStore {
    /// Only the real `Root` shows the confirmation dialog.
    fn prompts(&self) -> bool {
        self.root.eq_ignore_ascii_case(ROOT)
    }

    /// The PowerShell path of a CurrentUser store, for messages.
    fn path(store: &str) -> String {
        format!(r"Cert:\CurrentUser\{store}")
    }

    /// Fails before a change whose dialog could not be shown. An unreadable
    /// window station counts as a desktop: Windows then tries, and a dialog it
    /// cannot show comes back as an error.
    fn ensure_desktop(&self, change: Change) -> Result<(), TrustError> {
        if self.prompts() && matches!(devlanes_sys_windows::cert::interactive_desktop(), Ok(false))
        {
            return Err(self.change_error(change, CertError::NoDesktop));
        }
        Ok(())
    }

    /// Maps a failed change to the error the CLI shows.
    fn change_error(&self, change: Change, error: CertError) -> TrustError {
        let path = Self::path(&self.root);
        let detail = match error {
            CertError::NoDesktop => format!(
                "Windows confirms changes to {path} in a security warning on the desktop; run it from a logged-in desktop session, not over SSH or from a service"
            ),
            CertError::Cancelled => format!(
                "the Windows security warning for {path} was declined; run `{}` again and choose Yes",
                change.command()
            ),
            CertError::Io(error) => {
                let action = match change {
                    Change::Add => "adding the CA to",
                    Change::Delete => "removing the CA from",
                };
                return TrustError::io(format!("{action} {path}"), error);
            }
        };
        TrustError::InteractionRequired {
            command: change.command().to_owned(),
            detail,
        }
    }

    /// Where `thumbprint` stands in the logical stores, machine roots
    /// included; `Err` carries the reason a store could not be read.
    async fn lookup(&self, thumbprint: Thumbprint) -> Result<Result<Lookup, String>, TrustError> {
        let (root, disallowed) = (self.root.clone(), self.disallowed.clone());
        blocking("reading the certificate store", move || {
            let holds = |store: &str| -> Result<bool, String> {
                match CertStore::open(store, View::Logical, Access::Read) {
                    Ok(opened) => opened
                        .contains(&thumbprint)
                        .map_err(|error| format!("{}: {error}", Self::path(store))),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(format!("{}: {error}", Self::path(store))),
                }
            };
            Ok(if !holds(&root)? {
                Lookup::Missing
            } else if holds(&disallowed)? {
                Lookup::Distrusted
            } else {
                Lookup::Trusted
            })
        })
        .await
    }

    /// Certificates in the user's own store to delete: the CA when present
    /// and, with a subject, every other match.
    async fn doomed(&self, ca: Thumbprint) -> Result<Vec<Thumbprint>, TrustError> {
        let root = self.root.clone();
        let subject = self.subject.clone();
        blocking("reading the certificate store", move || -> io::Result<_> {
            let store = match CertStore::open(&root, View::UserRegistry, Access::Read) {
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
        .map_err(|error| TrustError::io(format!("reading {}", Self::path(&self.root)), error))
    }
}

#[async_trait]
impl TrustStore for UserRootStore {
    /// The target it was built for.
    fn target(&self) -> TrustTarget {
        self.target
    }

    /// [`STORE_ID`] for every target.
    fn id(&self) -> &'static str {
        STORE_ID
    }

    /// Names the store, the browsers that read it and, for `Root`, the dialog.
    fn describe(&self) -> String {
        let path = Self::path(&self.root);
        if self.prompts() {
            format!(
                "Windows user root store {path}, read by Chrome, Edge and Firefox (Windows asks you to confirm each change)"
            )
        } else {
            path
        }
    }

    /// Adds the CA to the user's own store unless a root store this user
    /// trusts already holds it. Older devlanes CAs stay until `uninstall`, as
    /// on macOS.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let thumbprint = thumbprint(ca)?;
        match self.lookup(thumbprint).await? {
            Ok(Lookup::Trusted) => return Ok(()),
            // Adding it again would change nothing: Disallowed wins.
            Ok(Lookup::Distrusted) => {
                return Err(TrustError::io(
                    format!("adding the CA to {}", Self::path(&self.root)),
                    io::Error::other(format!(
                        r"{0} (Untrusted Certificates in certmgr.msc) holds it, which overrides the root store; remove it there, or have an administrator remove it from Cert:\LocalMachine\{1}",
                        Self::path(&self.disallowed),
                        self.disallowed
                    )),
                ));
            }
            // An unreadable store still gets the add, which reports the real problem.
            Ok(Lookup::Missing) | Err(_) => {}
        }
        self.ensure_desktop(Change::Add)?;
        let root = self.root.clone();
        let der = ca.der().to_vec();
        blocking(
            "changing the certificate store",
            move || -> Result<(), CertError> {
                // The registry view: in an elevated process a write through the
                // logical store could land in the machine's `Root`.
                CertStore::open(&root, View::UserRegistry, Access::Write)?.add_der(&der)
            },
        )
        .await?
        .map_err(|error| self.change_error(Change::Add, error))
    }

    /// Deletes the CA and, with a subject, every devlanes CA from the user's
    /// own store, one dialog each. A CA still trusted afterwards comes from a
    /// store only an administrator can change.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let thumbprint = thumbprint(ca)?;
        let doomed = self.doomed(thumbprint).await?;
        if !doomed.is_empty() {
            self.ensure_desktop(Change::Delete)?;
            let root = self.root.clone();
            blocking(
                "changing the certificate store",
                move || -> Result<(), CertError> {
                    let store = CertStore::open(&root, View::UserRegistry, Access::Write)?;
                    for thumbprint in &doomed {
                        // Absent means it went away since the listing: the goal is reached.
                        store.delete(thumbprint)?;
                    }
                    Ok(())
                },
            )
            .await?
            .map_err(|error| self.change_error(Change::Delete, error))?;
        }
        if let Ok(Lookup::Trusted) = self.lookup(thumbprint).await? {
            return Err(TrustError::io(
                format!("removing the CA from {}", Self::path(&self.root)),
                io::Error::other(format!(
                    r"it is still trusted through Cert:\LocalMachine\{} or a group policy, which only an administrator can change",
                    self.root
                )),
            ));
        }
        Ok(())
    }

    /// Installed when a logical root store, machine roots included, holds the
    /// CA and `Disallowed` does not; a distrusted CA is missing.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        Ok(match self.lookup(thumbprint(ca)?).await? {
            Ok(Lookup::Trusted) => TrustStatus::Installed,
            Ok(Lookup::Missing | Lookup::Distrusted) => TrustStatus::Missing,
            Err(reason) => TrustStatus::Unknown { reason },
        })
    }
}

/// Runs crypt32 work on a blocking thread.
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

    /// Without a desktop or after a No the error names the devlanes command.
    #[test]
    fn dialog_problems_name_the_devlanes_command() {
        let store = UserRootStore {
            root: ROOT.into(),
            disallowed: DISALLOWED.into(),
            target: TrustTarget::System,
            subject: None,
        };
        for change in [Change::Add, Change::Delete] {
            for error in [CertError::NoDesktop, CertError::Cancelled] {
                let error = store.change_error(change, error);
                let TrustError::InteractionRequired { command, detail } = &error else {
                    panic!("{error:?}");
                };
                assert_eq!(command, change.command());
                assert!(detail.contains(r"Cert:\CurrentUser\Root"), "{detail}");
            }
        }
        let error = store.change_error(Change::Add, CertError::Io(io::Error::other("boom")));
        assert!(matches!(error, TrustError::Io { .. }), "{error:?}");
    }

    /// Mobile targets are refused; the browser targets share one store.
    #[test]
    fn targets_share_the_user_root_store() {
        let trust = WindowsTrust::default();
        for target in [
            TrustTarget::System,
            TrustTarget::Chrome,
            TrustTarget::Firefox,
        ] {
            let store = trust.store(target).expect("store");
            assert_eq!(store.id(), STORE_ID);
            assert_eq!(store.target(), target);
        }
        for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
            assert!(matches!(
                trust.store(target),
                Err(TrustError::Unsupported { .. })
            ));
        }
    }
}
