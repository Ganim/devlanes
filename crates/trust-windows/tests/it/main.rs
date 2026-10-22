//! Tests against scratch CurrentUser stores, which never prompt, plus an ignored
//! test against the real `Root` store.
#![cfg(windows)]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use devlanes_sys::windows::cert::{
    Access, CertStore, TEST_STORE_PREFIX, View, unregister_test_store,
};
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore as _, TrustTarget};
use devlanes_trust_windows::{
    DEVLANES_CA_SUBJECT, ENTERPRISE_ROOTS_PREF, FIREFOX_ID, LOCATION, ROOT, STORE_ID,
    UserRootStore, WindowsTrust, enterprise_roots_disabled, thumbprint,
};

/// A scratch CurrentUser store, deleted on drop.
struct Scratch(String);

impl Scratch {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self(format!("{TEST_STORE_PREFIX}{}-{nanos}", std::process::id()))
    }

    /// Whether the store's user registry part holds `ca`.
    fn holds(&self, ca: &CaCertificate) -> bool {
        CertStore::open(&self.0, View::UserRegistry, Access::Read)
            .and_then(|store| store.contains(&thumbprint(ca).expect("thumbprint")))
            .expect("read scratch store")
    }

    /// Adds `ca` directly, as an older devlanes run or another tool would have.
    fn add(&self, ca: &CaCertificate) {
        CertStore::open(&self.0, View::Logical, Access::Write)
            .expect("open scratch store")
            .add_der(ca.der())
            .expect("add");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Tests that never wrote have no store to delete.
        if CertStore::open(&self.0, View::Logical, Access::Read).is_ok()
            && let Err(error) = unregister_test_store(&self.0)
        {
            eprintln!("leaked certificate store {}: {error}", self.0);
        }
    }
}

/// A fresh devlanes CA (common name [`DEVLANES_CA_SUBJECT`]).
fn devlanes_ca() -> CaCertificate {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = devlanes_pki::CertificateAuthority::load_or_create(&dir.path().join("ca"))
        .expect("create CA");
    CaCertificate::from_pem(ca.certificate_pem(), "devlanes test CA").expect("parse CA")
}

fn store(scratch: &Scratch, subject: bool) -> UserRootStore {
    let store = UserRootStore::new(&scratch.0, TrustTarget::System);
    if subject {
        store.with_subject(DEVLANES_CA_SUBJECT)
    } else {
        store
    }
}

#[tokio::test]
async fn install_status_uninstall_round_trip() {
    let scratch = Scratch::new();
    let store = store(&scratch, false);
    let ca = devlanes_ca();
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    store.uninstall(&ca).await.expect("removing an absent CA");

    store.install(&ca).await.expect("install");
    assert!(scratch.holds(&ca));
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    store.install(&ca).await.expect("install again");

    store.uninstall(&ca).await.expect("uninstall");
    assert!(!scratch.holds(&ca));
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
}

#[tokio::test]
async fn install_replaces_older_devlanes_cas_only() {
    let scratch = Scratch::new();
    let older = devlanes_ca();
    let foreign = CaCertificate::from_pem(&foreign_pem(), "other").expect("foreign");
    scratch.add(&older);
    scratch.add(&foreign);

    let ca = devlanes_ca();
    store(&scratch, true).install(&ca).await.expect("install");
    assert!(scratch.holds(&ca));
    assert!(!scratch.holds(&older), "older devlanes CA removed");
    assert!(scratch.holds(&foreign), "other CAs stay");
}

#[tokio::test]
async fn uninstall_with_subject_removes_every_devlanes_ca() {
    let scratch = Scratch::new();
    let older = devlanes_ca();
    let ca = devlanes_ca();
    let foreign = CaCertificate::from_pem(&foreign_pem(), "other").expect("foreign");
    for cert in [&older, &ca, &foreign] {
        scratch.add(cert);
    }

    // mkcert's CA is not ours: without a subject only the CA itself goes.
    store(&scratch, false)
        .uninstall(&ca)
        .await
        .expect("uninstall");
    assert!(!scratch.holds(&ca));
    assert!(scratch.holds(&older));

    store(&scratch, true)
        .uninstall(&ca)
        .await
        .expect("uninstall");
    assert!(!scratch.holds(&older));
    assert!(scratch.holds(&foreign));
}

#[tokio::test]
async fn system_and_chrome_share_one_store_firefox_reports_it() {
    let scratch = Scratch::new();
    let profiles = tempfile::tempdir().expect("profiles");
    let trust = WindowsTrust::new(&scratch.0, vec![profiles.path().to_owned()]);
    let system = trust.store(TrustTarget::System).expect("system");
    let chrome = trust.store(TrustTarget::Chrome).expect("chrome");
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(system.id(), STORE_ID);
    assert_eq!(chrome.id(), STORE_ID);
    assert_eq!(chrome.target(), TrustTarget::Chrome);
    assert_eq!(firefox.id(), FIREFOX_ID);
    for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
        assert!(matches!(
            trust.store(target),
            Err(TrustError::Unsupported { target: t }) if t == target
        ));
    }

    let ca = devlanes_ca();
    firefox.install(&ca).await.expect("install through firefox");
    assert!(scratch.holds(&ca), "firefox installs into the system store");
    assert_eq!(
        system.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    assert_eq!(
        firefox.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    system.uninstall(&ca).await.expect("uninstall");
    assert_eq!(
        firefox.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    // Scratch stores do not prompt, so no dialog is announced.
    assert_eq!(firefox.approval(), None);
    assert!(
        firefox
            .location()
            .is_some_and(|location| location.contains("Firefox"))
    );
}

#[tokio::test]
async fn firefox_hints_only_when_a_profile_ignores_the_windows_store() {
    let profiles = tempfile::tempdir().expect("profiles");
    let root = profiles.path();
    let profile = |name: &str, prefs: Option<bool>, user: Option<bool>| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("profile");
        for (file, value) in [("prefs.js", prefs), ("user.js", user)] {
            if let Some(value) = value {
                write_pref(&dir.join(file), value);
            }
        }
    };
    profile("a.default-release", None, None);
    profile("b.on", Some(true), None);
    let trust = WindowsTrust::new(ROOT, vec![root.to_owned()]);
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(firefox.hint().await, None);

    profile("c.off", Some(false), None);
    // user.js is applied last, so it overrides prefs.js either way.
    profile("d.user-on", Some(false), Some(true));
    profile("e.user-off", Some(true), Some(false));
    assert_eq!(
        enterprise_roots_disabled(&[root.to_owned()]),
        ["c.off", "e.user-off"]
    );
    let hint = firefox.hint().await.expect("hint");
    assert!(hint.contains("c.off, e.user-off"), "{hint}");
    assert!(hint.contains("about:config"), "{hint}");
    assert!(
        WindowsTrust::new(ROOT, vec![root.join("missing")])
            .store(TrustTarget::Firefox)
            .expect("firefox")
            .hint()
            .await
            .is_none()
    );
}

/// Building stores for `Root` touches nothing; the text names the store and dialog.
#[test]
fn root_store_names_itself_and_its_dialog() {
    let trust = WindowsTrust::new(ROOT, Vec::new());
    let system = trust.store(TrustTarget::System).expect("system");
    assert_eq!(system.location().as_deref(), Some(LOCATION));
    let approval = system.approval().expect("Root prompts");
    assert!(approval.contains("Security Warning"), "{approval}");
    assert!(system.describe().contains(r"Cert:\CurrentUser\Root"));
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(firefox.approval(), Some(approval));
}

/// Writes one boolean [`ENTERPRISE_ROOTS_PREF`] line, as Firefox does.
fn write_pref(path: &Path, value: bool) {
    std::fs::write(
        path,
        format!("// Mozilla User Preferences\nuser_pref(\"{ENTERPRISE_ROOTS_PREF}\", {value});\n"),
    )
    .expect("write prefs");
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

/// The real store: one click on each Windows security warning.
#[tokio::test]
#[ignore = "needs a desktop session and a click on the Windows security dialog"]
async fn real_root_install_status_uninstall() {
    let trust = WindowsTrust::new(ROOT, Vec::new()).with_ca_subject(DEVLANES_CA_SUBJECT);
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
