//! The user root store against scratch stores, which never show a dialog. The
//! real `Root` is only read; changing it shows Windows' security warning, so
//! that test is ignored and run by hand from a desktop session.
#![cfg(windows)]

use devlanes_sys_windows::cert::{Access, CertStore, ScratchStore, View};
use devlanes_trust::{CaCertificate, TrustStatus, TrustStore, TrustTarget};
use devlanes_trust_windows::{DEVLANES_CA_SUBJECT, WindowsTrust};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A fresh devlanes CA.
fn devlanes_ca() -> Result<CaCertificate> {
    let dir = tempfile::tempdir()?;
    let ca = devlanes_pki::CertificateAuthority::load_or_create(&dir.path().join("ca"))?;
    Ok(CaCertificate::from_pem(
        ca.certificate_pem(),
        "devlanes development CA",
    )?)
}

/// The system store over the scratch `root` and `disallowed` stores.
fn store(root: &ScratchStore, disallowed: &ScratchStore) -> Result<Box<dyn TrustStore>> {
    Ok(WindowsTrust::with_stores(root.name(), disallowed.name())
        .with_ca_subject(DEVLANES_CA_SUBJECT)
        .store(TrustTarget::System)?)
}

/// Install, status, repeat and uninstall round-trip; removing an absent CA
/// succeeds.
#[tokio::test]
async fn install_and_uninstall_round_trip() -> Result {
    let (root, disallowed) = (ScratchStore::new(), ScratchStore::new());
    let store = store(&root, &disallowed)?;
    let ca = devlanes_ca()?;
    assert_eq!(store.status(&ca).await?, TrustStatus::Missing);
    store.install(&ca).await?;
    assert_eq!(store.status(&ca).await?, TrustStatus::Installed);
    store.install(&ca).await?;
    store.uninstall(&ca).await?;
    assert_eq!(store.status(&ca).await?, TrustStatus::Missing);
    store.uninstall(&ca).await?;
    Ok(())
}

/// Uninstall also removes older devlanes CAs, found by subject, and leaves
/// other roots alone.
#[tokio::test]
async fn uninstall_removes_older_devlanes_cas() -> Result {
    let (root, disallowed) = (ScratchStore::new(), ScratchStore::new());
    let store = store(&root, &disallowed)?;
    let (old, current) = (devlanes_ca()?, devlanes_ca()?);
    store.install(&old).await?;
    store.install(&current).await?;
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "acme root");
    let other = CaCertificate::from_pem(&params.self_signed(&key)?.pem(), "acme")?;
    store.install(&other).await?;

    store.uninstall(&current).await?;
    assert_eq!(store.status(&old).await?, TrustStatus::Missing);
    assert_eq!(store.status(&current).await?, TrustStatus::Missing);
    assert_eq!(store.status(&other).await?, TrustStatus::Installed);
    Ok(())
}

/// A CA the Disallowed store holds is not trusted, and installing it again
/// says why instead of pretending to succeed.
#[tokio::test]
async fn distrusted_ca_is_missing() -> Result {
    let (root, disallowed) = (ScratchStore::new(), ScratchStore::new());
    let store = store(&root, &disallowed)?;
    let ca = devlanes_ca()?;
    store.install(&ca).await?;
    CertStore::open(disallowed.name(), View::UserRegistry, Access::Write)?.add_der(ca.der())?;
    assert_eq!(store.status(&ca).await?, TrustStatus::Missing);
    let error = store.install(&ca).await.expect_err("distrusted");
    assert!(
        error.to_string().contains("Untrusted Certificates"),
        "{error}"
    );
    Ok(())
}

/// The real store is only read: its status is a definite answer, never an
/// error.
#[tokio::test]
async fn real_root_status_is_readable() -> Result {
    let store = WindowsTrust::default().store(TrustTarget::Firefox)?;
    let status = store.status(&devlanes_ca()?).await?;
    assert_eq!(status, TrustStatus::Missing, "a fresh CA is in no store");
    assert!(store.describe().contains("Firefox"));
    Ok(())
}

/// Adds a fresh CA to the real user `Root` and removes it again; Windows asks
/// twice. Run by hand: `cargo nextest run -p devlanes-trust-windows --run-ignored only`.
#[ignore = "needs a desktop session"]
#[tokio::test]
async fn real_root_round_trip() -> Result {
    let store = WindowsTrust::default().store(TrustTarget::System)?;
    let ca = devlanes_ca()?;
    store.install(&ca).await?;
    assert_eq!(store.status(&ca).await?, TrustStatus::Installed);
    store.uninstall(&ca).await?;
    assert_eq!(store.status(&ca).await?, TrustStatus::Missing);
    Ok(())
}
