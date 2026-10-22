//! Tests against scratch CurrentUser stores, which never prompt, plus an ignored
//! test against the real `Root` store. Scratch stores are `devlanes-test-*` keys
//! under `HKCU\Software\Microsoft\SystemCertificates`, deleted by [`TestStore`]
//! when a test ends or panics.
#![cfg(windows)]

use std::path::{Path, PathBuf};

use devlanes_sys::windows::cert::{Access, CertStore, TestStore, View, interactive_desktop};
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore as _, TrustTarget};
use devlanes_trust_windows::{
    DEVLANES_CA_SUBJECT, ENTERPRISE_ROOTS_PREF, FIREFOX_ID, FirefoxDirs, LOCATION, ROOT, STORE_ID,
    UserRootStore, WindowsTrust, enterprise_roots_disabled, thumbprint,
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
async fn system_and_chrome_share_one_store_firefox_reports_it() {
    let (scratch, disallowed) = (TestStore::unique(), TestStore::unique());
    let trust = WindowsTrust::new(scratch.name(), FirefoxDirs::default())
        .with_disallowed(disallowed.name());
    let system = trust.store(TrustTarget::System).expect("system");
    let chrome = trust.store(TrustTarget::Chrome).expect("chrome");
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    // The target WSL uses for this store; a shared config reports it once.
    let windows = trust.store(TrustTarget::Windows).expect("windows");
    assert_eq!(system.id(), STORE_ID);
    assert_eq!(chrome.id(), STORE_ID);
    assert_eq!(windows.id(), STORE_ID);
    assert_eq!(chrome.target(), TrustTarget::Chrome);
    assert_eq!(firefox.id(), FIREFOX_ID);
    for store in [&system, &chrome, &windows, &firefox] {
        assert_eq!(store.changes(), STORE_ID, "one change set");
    }
    for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
        assert!(matches!(
            trust.store(target),
            Err(TrustError::Unsupported { target: t }) if t == target
        ));
    }

    let ca = devlanes_ca();
    firefox.install(&ca).await.expect("install through firefox");
    assert!(
        holds(&scratch, &ca),
        "firefox installs into the system store"
    );
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
    assert_eq!(firefox.pending_approval(&ca, true).await, None);
    assert!(
        firefox
            .location()
            .is_some_and(|location| location.contains("Firefox"))
    );
}

/// Writes a prefs file with `lines`, creating the profile folder.
fn write_prefs(profile: &Path, file: &str, lines: &[&str]) {
    std::fs::create_dir_all(profile).expect("profile");
    std::fs::write(profile.join(file), lines.join("\n")).expect("write prefs");
}

/// `user_pref` for [`ENTERPRISE_ROOTS_PREF`].
fn user_pref(value: bool) -> String {
    format!("user_pref(\"{ENTERPRISE_ROOTS_PREF}\", {value});")
}

#[tokio::test]
async fn firefox_hints_only_when_a_profile_ignores_the_windows_store() {
    let home = tempfile::tempdir().expect("home");
    let appdata = home.path().join("Roaming");
    let profiles = appdata.join(r"Mozilla\Firefox\Profiles");
    let dirs = FirefoxDirs {
        appdata: Some(appdata.clone()),
        local_appdata: None,
    };
    let trust = WindowsTrust::new(ROOT, dirs.clone());
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(firefox.hint().await, None, "no Firefox");

    let (on, off) = (user_pref(true), user_pref(false));
    write_prefs(&profiles.join("a.default-release"), "prefs.js", &[]);
    write_prefs(&profiles.join("b.on"), "prefs.js", &[&on]);
    assert_eq!(firefox.hint().await, None);

    write_prefs(&profiles.join("c.off"), "prefs.js", &[&off]);
    // user.js is applied last, so its user_pref overrides prefs.js either way.
    write_prefs(&profiles.join("d.user-on"), "prefs.js", &[&off]);
    write_prefs(&profiles.join("d.user-on"), "user.js", &[&on]);
    write_prefs(&profiles.join("e.user-off"), "prefs.js", &[&on]);
    write_prefs(&profiles.join("e.user-off"), "user.js", &[&off]);
    // pref() only sets the default: any user_pref wins over it.
    let default_off = format!("pref(\"{ENTERPRISE_ROOTS_PREF}\", false);");
    let sticky_off = format!("sticky_pref(\"{ENTERPRISE_ROOTS_PREF}\", false);");
    write_prefs(&profiles.join("f.default-off"), "user.js", &[&sticky_off]);
    write_prefs(&profiles.join("g.user-wins"), "prefs.js", &[&on]);
    write_prefs(&profiles.join("g.user-wins"), "user.js", &[&default_off]);
    // A commented-out line counts for nothing.
    write_prefs(
        &profiles.join("h.commented"),
        "prefs.js",
        &["/*", &off, "*/", &format!("// {off}")],
    );
    assert_eq!(
        enterprise_roots_disabled(&dirs.profiles()),
        ["c.off", "e.user-off", "f.default-off"]
    );
    let hint = firefox.hint().await.expect("hint");
    assert!(hint.contains("c.off, e.user-off, f.default-off"), "{hint}");
    assert!(hint.contains("about:config"), "{hint}");
}

/// With `profiles.ini`, only the profiles it lists count, relative or absolute;
/// the MSIX build's folder under `%LOCALAPPDATA%\Packages` is read too.
#[test]
fn firefox_profiles_follow_profiles_ini_and_msix() {
    let home = tempfile::tempdir().expect("home");
    let appdata = home.path().join("Roaming");
    let local = home.path().join("Local");
    let firefox = appdata.join(r"Mozilla\Firefox");
    let off = user_pref(false);
    write_prefs(&firefox.join(r"Profiles\listed"), "prefs.js", &[&off]);
    write_prefs(&firefox.join(r"Profiles\removed"), "prefs.js", &[&off]);
    let elsewhere = home.path().join("elsewhere");
    write_prefs(&elsewhere, "prefs.js", &[&off]);
    std::fs::write(
        firefox.join("profiles.ini"),
        format!(
            "[Profile0]\nName=default\nIsRelative=1\nPath=Profiles/listed\n\n\
             [Profile1]\nName=other\nIsRelative=0\nPath={}\n",
            elsewhere.display()
        ),
    )
    .expect("profiles.ini");
    let msix =
        local.join(r"Packages\Mozilla.Firefox_n80bbvh6b1yt2\LocalCache\Roaming\Mozilla\Firefox");
    write_prefs(&msix.join(r"Profiles\store"), "prefs.js", &[&off]);
    write_prefs(
        &local.join(
            r"Packages\Mozilla.Thunderbird_x\LocalCache\Roaming\Mozilla\Firefox\Profiles\mail",
        ),
        "prefs.js",
        &[&off],
    );

    let dirs = FirefoxDirs {
        appdata: Some(appdata),
        local_appdata: Some(local),
    };
    assert_eq!(dirs.data_dirs(), [firefox.clone(), msix.clone()]);
    let profiles: Vec<PathBuf> = dirs.profiles();
    assert_eq!(
        profiles,
        [
            firefox.join(r"Profiles\listed"),
            elsewhere,
            msix.join(r"Profiles\store"),
        ]
    );
    assert_eq!(
        enterprise_roots_disabled(&profiles),
        ["elsewhere", "listed", "store"]
    );
}

/// Building stores for `Root` touches nothing; the text names the store and dialog.
#[tokio::test]
async fn root_store_names_itself_and_its_dialog() {
    let trust = WindowsTrust::new(ROOT, FirefoxDirs::default());
    let system = trust.store(TrustTarget::System).expect("system");
    assert_eq!(system.location().as_deref(), Some(LOCATION));
    let approval = system.approval().expect("Root prompts");
    assert_eq!(approval, "choose Yes in the Windows security warning");
    assert!(system.describe().contains(r"Cert:\CurrentUser\Root"));
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(firefox.approval(), Some(approval));

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
    let trust =
        WindowsTrust::new(ROOT, FirefoxDirs::default()).with_ca_subject(DEVLANES_CA_SUBJECT);
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
