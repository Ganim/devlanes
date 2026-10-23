//! Tests against scratch CurrentUser stores, which never prompt, plus an ignored
//! test against the real `Root` store. Scratch stores are `devlanes-test-*` keys
//! under `HKCU\Software\Microsoft\SystemCertificates`, deleted by [`TestStore`]
//! when a test ends or panics.
#![cfg(windows)]

use devlanes_sys::windows::cert::{Access, CertStore, TestStore, View, interactive_desktop};
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore as _, TrustTarget};
use devlanes_trust_windows::{
    DEVLANES_CA_SUBJECT, LOCATION, ROOT, STORE_ID, UserRootStore, WindowsTrust, thumbprint,
};

/// Whether the user registry part of `store` holds `ca`.
fn holds(store: &TestStore, ca: &CaCertificate) -> bool {
    match CertStore::open(store.name(), View::UserRegistry, Access::Read) {
        Ok(opened) => opened
            .contains(&thumbprint(ca).expect("thumbprint"))
            .expect("read scratch store"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("open {}: {error}", store.name()),
    }
}

/// Adds `ca` directly, as an older devlanes run or another tool would have.
fn add(store: &TestStore, ca: &CaCertificate) {
    CertStore::open(store.name(), View::UserRegistry, Access::Write)
        .expect("open scratch store")
        .add_der(ca.der())
        .expect("add");
}

/// A fresh devlanes CA (common name [`DEVLANES_CA_SUBJECT`]).
fn devlanes_ca() -> CaCertificate {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = devlanes_pki::CertificateAuthority::load_or_create(&dir.path().join("ca"))
        .expect("create CA");
    CaCertificate::from_pem(ca.certificate_pem(), "devlanes test CA").expect("parse CA")
}

/// The store for `scratch`, with a scratch deny list so the real one is never read.
fn store(scratch: &TestStore, disallowed: &TestStore, subject: bool) -> UserRootStore {
    let store =
        UserRootStore::new(scratch.name(), TrustTarget::System).with_disallowed(disallowed.name());
    if subject {
        store.with_subject(DEVLANES_CA_SUBJECT)
    } else {
        store
    }
}

#[tokio::test]
async fn install_status_uninstall_round_trip() {
    let (scratch, disallowed) = (TestStore::unique(), TestStore::unique());
    let store = store(&scratch, &disallowed, false);
    let ca = devlanes_ca();
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    store.uninstall(&ca).await.expect("removing an absent CA");

    store.install(&ca).await.expect("install");
    assert!(holds(&scratch, &ca));
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    store.install(&ca).await.expect("install again");

    store.uninstall(&ca).await.expect("uninstall");
    assert!(!holds(&scratch, &ca));
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
}

/// Install only adds, as on macOS; older devlanes CAs go on uninstall, which
/// `recreate_ca_command` runs first.
#[tokio::test]
async fn install_keeps_older_cas_uninstall_removes_devlanes_ones() {
    let (scratch, disallowed) = (TestStore::unique(), TestStore::unique());
    let older = devlanes_ca();
    let foreign = CaCertificate::from_pem(&foreign_pem(), "other").expect("foreign");
    add(&scratch, &older);
    add(&scratch, &foreign);

    let ca = devlanes_ca();
    let store = store(&scratch, &disallowed, true);
    store.install(&ca).await.expect("install");
    assert!(holds(&scratch, &ca));
    assert!(holds(&scratch, &older), "install removes nothing");
    assert!(holds(&scratch, &foreign));

    store.uninstall(&ca).await.expect("uninstall");
    assert!(!holds(&scratch, &ca));
    assert!(!holds(&scratch, &older), "older devlanes CA removed");
    assert!(holds(&scratch, &foreign), "other CAs stay");
}

#[tokio::test]
async fn uninstall_without_subject_removes_only_the_ca() {
    let (scratch, disallowed) = (TestStore::unique(), TestStore::unique());
    let older = devlanes_ca();
    let ca = devlanes_ca();
    for cert in [&older, &ca] {
        add(&scratch, cert);
    }
    // mkcert's CA is not ours: without a subject only the CA itself goes.
    store(&scratch, &disallowed, false)
        .uninstall(&ca)
        .await
        .expect("uninstall");
    assert!(!holds(&scratch, &ca));
    assert!(holds(&scratch, &older));
}

/// A CA the `Disallowed` store also holds is installed but not trusted; install
/// does not add it again.
#[tokio::test]
async fn disallowed_ca_is_distrusted() {
    let (scratch, disallowed) = (TestStore::unique(), TestStore::unique());
    let store = store(&scratch, &disallowed, false);
    let ca = devlanes_ca();
    add(&disallowed, &ca);
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing,
        "only the deny list holds it"
    );
    store.install(&ca).await.expect("install");
    let status = store.status(&ca).await.expect("status");
    let TrustStatus::Distrusted { reason } = status else {
        panic!("expected distrusted, got {status:?}");
    };
    assert!(reason.contains(disallowed.name()), "{reason}");
    store.install(&ca).await.expect("install again");
    store.uninstall(&ca).await.expect("uninstall");
    assert!(!holds(&scratch, &ca));
    assert!(holds(&disallowed, &ca), "the deny list is never changed");
}

#[tokio::test]
async fn browser_targets_share_the_system_store() {
    let (scratch, disallowed) = (TestStore::unique(), TestStore::unique());
    let trust = WindowsTrust::new(scratch.name()).with_disallowed(disallowed.name());
    let system = trust.store(TrustTarget::System).expect("system");
    let chrome = trust.store(TrustTarget::Chrome).expect("chrome");
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    // The target WSL uses for this store; a shared config reports it once.
    let windows = trust.store(TrustTarget::Windows).expect("windows");
    for store in [&system, &chrome, &windows, &firefox] {
        assert_eq!(store.id(), STORE_ID);
    }
    assert_eq!(chrome.target(), TrustTarget::Chrome);
    for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
        assert!(matches!(
            trust.store(target),
            Err(TrustError::Unsupported { target: t }) if t == target
        ));
    }

    let ca = devlanes_ca();
    firefox.install(&ca).await.expect("install through firefox");
    assert!(holds(&scratch, &ca), "firefox is the system store");
    assert_eq!(
        system.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    system.uninstall(&ca).await.expect("uninstall");
    assert_eq!(
        firefox.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    // Scratch stores do not prompt, so no dialog is announced.
    assert_eq!(firefox.approval(), None);
    assert_eq!(firefox.pending_approval(&ca, true).await, None);
}

/// Building stores for `Root` touches nothing; the text names the store and dialog.
#[tokio::test]
async fn root_store_names_itself_and_its_dialog() {
    let trust = WindowsTrust::new(ROOT);
    let system = trust.store(TrustTarget::System).expect("system");
    assert_eq!(system.location().as_deref(), Some(LOCATION));
    let approval = system.approval().expect("Root prompts");
    assert_eq!(approval, "choose Yes in the Windows security warning");
    assert!(system.describe().contains(r"Cert:\CurrentUser\Root"));

    // Read-only lookups in the real store: a fresh CA is absent, so an install
    // would add it (one dialog when there is a desktop) and an uninstall without
    // a subject has nothing to delete.
    let ca = devlanes_ca();
    let pending = system.pending_approval(&ca, true).await;
    if interactive_desktop().expect("window station") {
        let pending = pending.expect("an add prompts");
        assert!(pending.contains("\"Security Warning\""), "{pending}");
    } else {
        assert_eq!(pending, None, "no dialog without a desktop");
    }
    assert_eq!(system.pending_approval(&ca, false).await, None);
}

/// A CA from someone else, with an unrelated subject.
fn foreign_pem() -> String {
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "acme internal CA");
    params.self_signed(&key).expect("certificate").pem()
}

/// The real store: one click on each Windows security warning. The only test
/// that shows the dialogs for the add through the registry view
/// (`CERT_STORE_PROV_SYSTEM_REGISTRY_W`) and the delete.
#[tokio::test]
#[ignore = "needs a desktop session and a click on the Windows security dialog"]
async fn real_root_install_status_uninstall() {
    let trust = WindowsTrust::new(ROOT).with_ca_subject(DEVLANES_CA_SUBJECT);
    let store = trust.store(TrustTarget::System).expect("store");
    let ca = devlanes_ca();
    store.install(&ca).await.expect("install (choose Yes)");
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    store.uninstall(&ca).await.expect("uninstall (choose Yes)");
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
}
