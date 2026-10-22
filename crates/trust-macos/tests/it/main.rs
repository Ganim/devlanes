//! Argv tests for the macOS stores, plus ignored tests against a real keychain and certutil.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devlanes_trust::fake::FakeExecutor;
use devlanes_trust::{
    CaCertificate, CommandLine, CommandOutput, TrustError, TrustStatus, TrustStore as _,
    TrustTarget,
};
use devlanes_trust_macos::{
    DEVLANES_CA_LABEL, FIREFOX_ID, KEYCHAIN_ID, KeychainStore, MacosTrust, SECURITY,
};

fn ca() -> CaCertificate {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = devlanes_pki::CertificateAuthority::load_or_create(&dir.path().join("ca"))
        .expect("create CA");
    CaCertificate::from_pem(ca.certificate_pem(), "devlanes test CA").expect("parse CA")
}

fn subcommand(command: &CommandLine) -> &str {
    command.args.first().map_or("", String::as_str)
}

/// Output of `find-certificate -a -Z` listing `sha1` among other certificates.
fn listing(sha1: &str) -> String {
    format!(
        "SHA-256 hash: 00\nSHA-1 hash: 1111111111111111111111111111111111111111\nkeychain: \"k\"\n\
         SHA-256 hash: 22\nSHA-1 hash: {sha1}\nkeychain: \"k\"\n"
    )
}

fn keychain_store(executor: Arc<FakeExecutor>, keychain: &Path) -> KeychainStore {
    KeychainStore::new(executor, keychain, TrustTarget::System)
}

#[tokio::test]
async fn install_adds_to_login_keychain_with_root_trust() {
    let ca = ca();
    let written = Arc::new(Mutex::new(String::new()));
    let seen = written.clone();
    let executor = Arc::new(FakeExecutor::new(move |command| {
        Ok(match subcommand(command) {
            "find-certificate" => CommandOutput::failed(44, "could not be found"),
            "add-trusted-cert" => {
                let path = command.args.last().expect("pem");
                *seen.lock().expect("lock") = std::fs::read_to_string(path).expect("temp pem");
                CommandOutput::ok("")
            }
            other => panic!("unexpected {other}"),
        })
    }));
    let home = PathBuf::from("/Users/me");
    let store = MacosTrust::new(executor.clone(), &home)
        .store(TrustTarget::System)
        .expect("store");
    store.install(&ca).await.expect("install");

    let keychain = "/Users/me/Library/Keychains/login.keychain-db";
    let argvs = executor.argvs();
    assert_eq!(argvs.len(), 2);
    assert_eq!(
        argvs[0],
        [SECURITY, "find-certificate", "-a", "-Z", keychain]
    );
    assert_eq!(
        argvs[1][..6],
        [
            SECURITY,
            "add-trusted-cert",
            "-r",
            "trustRoot",
            "-k",
            keychain
        ]
    );
    assert_eq!(*written.lock().expect("lock"), ca.pem());
    let temp = PathBuf::from(&argvs[1][6]);
    assert!(!temp.exists(), "temp PEM is removed after install");
}

#[tokio::test]
async fn install_is_a_no_op_when_trusted() {
    let ca = ca();
    let sha1 = ca.sha1_hex();
    let executor = Arc::new(FakeExecutor::new(move |command| {
        Ok(match subcommand(command) {
            "find-certificate" => CommandOutput::ok(listing(&sha1)),
            "verify-cert" => CommandOutput::ok(""),
            other => panic!("unexpected {other}"),
        })
    }));
    let store = keychain_store(executor.clone(), Path::new("/k.keychain-db"));
    store.install(&ca).await.expect("install");
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    let argvs = executor.argvs();
    assert_eq!(argvs[1][..3], [SECURITY, "verify-cert", "-c"]);
    assert_eq!(argvs[1][4..], ["-l", "-L", "-q"]);
    assert!(argvs.iter().all(|argv| argv[1] != "add-trusted-cert"));
}

#[tokio::test]
async fn untrusted_certificate_only_gets_trust_settings() {
    let ca = ca();
    let sha1 = ca.sha1_hex();
    let executor = Arc::new(FakeExecutor::new(move |command| {
        Ok(match subcommand(command) {
            "find-certificate" => CommandOutput::ok(listing(&sha1)),
            "verify-cert" => CommandOutput::failed(1, "CSSMERR_TP_NOT_TRUSTED"),
            "add-trusted-cert" => CommandOutput::ok(""),
            other => panic!("unexpected {other}"),
        })
    }));
    let store = keychain_store(executor.clone(), Path::new("/k.keychain-db"));
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    store.install(&ca).await.expect("install");
    let add = executor.argvs().pop().expect("add");
    assert_eq!(add[..4], [SECURITY, "add-trusted-cert", "-r", "trustRoot"]);
    assert_eq!(
        add.len(),
        5,
        "no -k: the certificate is already in the keychain"
    );
}

#[tokio::test]
async fn uninstall_deletes_by_sha1_with_trust_settings() {
    let ca = ca();
    let sha1 = ca.sha1_hex();
    let listed = sha1.clone();
    let executor = Arc::new(FakeExecutor::new(move |command| {
        Ok(match subcommand(command) {
            "find-certificate" => CommandOutput::ok(listing(&listed)),
            "verify-cert" => CommandOutput::ok(""),
            "delete-certificate" => CommandOutput::ok(""),
            other => panic!("unexpected {other}"),
        })
    }));
    let store = keychain_store(executor.clone(), Path::new("/k.keychain-db"));
    store.uninstall(&ca).await.expect("uninstall");
    assert_eq!(
        executor.argvs().pop().expect("delete"),
        [
            SECURITY,
            "delete-certificate",
            "-Z",
            sha1.as_str(),
            "-t",
            "/k.keychain-db"
        ]
    );
}

#[tokio::test]
async fn uninstall_with_label_also_removes_older_devlanes_cas() {
    let ca = ca();
    let sha1 = ca.sha1_hex();
    let old = "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD";
    let ours = sha1.clone();
    let executor = Arc::new(FakeExecutor::new(move |command| {
        Ok(match subcommand(command) {
            "find-certificate" if command.args.iter().any(|a| a == "-c") => {
                CommandOutput::ok(format!("SHA-1 hash: {ours}\nSHA-1 hash: {old}\n"))
            }
            "find-certificate" => CommandOutput::ok(listing(&ours)),
            "delete-certificate" => CommandOutput::ok(""),
            other => panic!("unexpected {other}"),
        })
    }));
    let store = MacosTrust::new(executor.clone(), "/Users/me")
        .with_ca_label(DEVLANES_CA_LABEL)
        .store(TrustTarget::System)
        .expect("store");
    store.uninstall(&ca).await.expect("uninstall");
    let keychain = "/Users/me/Library/Keychains/login.keychain-db";
    let argvs = executor.argvs();
    assert_eq!(
        argvs[1],
        [
            SECURITY,
            "find-certificate",
            "-a",
            "-c",
            DEVLANES_CA_LABEL,
            "-Z",
            keychain
        ]
    );
    let deleted: Vec<&str> = argvs
        .iter()
        .filter(|argv| argv[1] == "delete-certificate")
        .map(|argv| argv[3].as_str())
        .collect();
    let mut expected = vec![old, sha1.as_str()];
    expected.sort_unstable();
    assert_eq!(deleted, expected, "each hash deleted once");
}

#[test]
fn label_matches_the_devlanes_ca_common_name() {
    let der = ca().der().to_vec();
    assert!(
        der.windows(DEVLANES_CA_LABEL.len())
            .any(|window| window == DEVLANES_CA_LABEL.as_bytes()),
        "devlanes-pki's CA subject must carry {DEVLANES_CA_LABEL:?}"
    );
}

#[tokio::test]
async fn uninstall_of_absent_certificate_changes_nothing() {
    let ca = ca();
    let executor = Arc::new(FakeExecutor::new(|_| {
        Ok(CommandOutput::ok(listing(
            "2222222222222222222222222222222222222222",
        )))
    }));
    let store = keychain_store(executor.clone(), Path::new("/k.keychain-db"));
    store.uninstall(&ca).await.expect("uninstall");
    assert_eq!(executor.calls().len(), 1);
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
}

#[tokio::test]
async fn headless_session_needs_interaction() {
    let ca = ca();
    let executor = Arc::new(FakeExecutor::new(|command| {
        Ok(match subcommand(command) {
            "find-certificate" => CommandOutput::failed(44, ""),
            _ => CommandOutput::failed(
                1,
                "SecTrustSettingsSetTrustSettings: The authorization was denied since no user interaction was possible.",
            ),
        })
    }));
    let store = keychain_store(executor, Path::new("/k.keychain-db"));
    assert!(matches!(
        store.install(&ca).await,
        Err(TrustError::InteractionRequired { command, .. }) if command.contains("add-trusted-cert")
    ));
}

#[tokio::test]
async fn unreadable_keychain_is_unknown() {
    let ca = ca();
    let executor = Arc::new(FakeExecutor::new(|_| {
        Ok(CommandOutput::failed(
            50,
            "The specified keychain could not be found.",
        ))
    }));
    let store = keychain_store(executor, Path::new("/k.keychain-db"));
    assert!(matches!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Unknown { reason } if reason.contains("could not be found")
    ));
}

#[tokio::test]
async fn targets_map_to_stores() {
    let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
    let home = tempfile::tempdir().expect("home");
    let trust = MacosTrust::new(executor, home.path());
    let system = trust.store(TrustTarget::System).expect("system");
    let chrome = trust.store(TrustTarget::Chrome).expect("chrome");
    assert_eq!((system.id(), chrome.id()), (KEYCHAIN_ID, KEYCHAIN_ID));
    assert_eq!(chrome.target(), TrustTarget::Chrome);
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(firefox.id(), FIREFOX_ID);
    assert!(
        firefox
            .describe()
            .contains("Library/Application Support/Firefox/Profiles")
    );
    for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
        assert!(matches!(
            trust.store(target),
            Err(TrustError::Unsupported { target: t }) if t == target
        ));
    }
}

#[tokio::test]
async fn firefox_without_certutil_has_brew_hint() {
    let home = tempfile::tempdir().expect("home");
    let profile = home
        .path()
        .join("Library/Application Support/Firefox/Profiles/abc.default-release");
    std::fs::create_dir_all(&profile).expect("profile");
    std::fs::write(profile.join("cert9.db"), b"").expect("cert9.db");
    let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
    let firefox = MacosTrust::new(executor, home.path())
        .store(TrustTarget::Firefox)
        .expect("firefox");
    assert!(matches!(
        firefox.install(&ca()).await,
        Err(TrustError::ToolMissing { tool, hint }) if tool == "certutil" && hint.contains("brew install nss")
    ));
}

#[cfg(target_os = "macos")]
mod real {
    use std::process::Command;

    use devlanes_trust::{CommandExecutor as _, SystemExecutor};

    use super::*;

    /// Deletes the throwaway keychain, which `create-keychain` also added to the search list.
    struct KeychainGuard(PathBuf);

    impl Drop for KeychainGuard {
        fn drop(&mut self) {
            let _ = Command::new(SECURITY)
                .arg("delete-keychain")
                .arg(&self.0)
                .status();
        }
    }

    /// Installs into a throwaway keychain. Trust settings are per user, not per
    /// keychain, so this shows the real password dialog and touches user trust
    /// settings until the uninstall at the end.
    #[tokio::test]
    #[ignore = "needs keychain"]
    async fn keychain_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keychain = dir.path().join("devlanes-test.keychain-db");
        let created = Command::new(SECURITY)
            .args(["create-keychain", "-p", "devlanes-test"])
            .arg(&keychain)
            .status()
            .expect("create-keychain");
        assert!(created.success());
        let _guard = KeychainGuard(keychain.clone());

        let ca = ca();
        let store = KeychainStore::new(Arc::new(SystemExecutor), &keychain, TrustTarget::System);
        assert_eq!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Missing
        );
        store.install(&ca).await.expect("install");
        assert_eq!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Installed
        );
        store.uninstall(&ca).await.expect("uninstall");
        assert_eq!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Missing
        );
    }

    /// Installs into a fake Firefox profile with the real `certutil`.
    #[tokio::test]
    #[ignore = "needs certutil"]
    async fn firefox_profile_round_trip() {
        let home = tempfile::tempdir().expect("home");
        let profile = home
            .path()
            .join("Library/Application Support/Firefox/Profiles/test.default");
        std::fs::create_dir_all(&profile).expect("profile");
        let certutil = SystemExecutor
            .find_program(
                "certutil",
                &devlanes_trust_macos::certutil_lookup().extra_dirs,
            )
            .expect("certutil installed");
        let created = Command::new(certutil)
            .arg("-N")
            .arg("-d")
            .arg(format!("sql:{}", profile.display()))
            .arg("--empty-password")
            .status()
            .expect("certutil -N");
        assert!(created.success());

        let ca = ca();
        let firefox = MacosTrust::new(Arc::new(SystemExecutor), home.path())
            .store(TrustTarget::Firefox)
            .expect("firefox");
        firefox.install(&ca).await.expect("install");
        assert_eq!(
            firefox.status(&ca).await.expect("status"),
            TrustStatus::Installed
        );
        firefox.uninstall(&ca).await.expect("uninstall");
        assert_eq!(
            firefox.status(&ca).await.expect("status"),
            TrustStatus::Missing
        );
    }
}
