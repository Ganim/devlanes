//! Argv tests for the Linux stores, plus ignored tests against real sudo and certutil.

use std::sync::{Arc, Mutex};

use devlanes_trust::fake::FakeExecutor;
use devlanes_trust::{
    CaCertificate, CommandOutput, TrustError, TrustStatus, TrustStore as _, TrustTarget,
};
use devlanes_trust_linux::{
    CHROMIUM_ID, CaStoreLayout, FIREFOX_ID, LinuxTrust, Privilege, SYSTEM_ID, SystemStore,
};

fn ca() -> CaCertificate {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = devlanes_pki::CertificateAuthority::load_or_create(&dir.path().join("ca"))
        .expect("create CA");
    CaCertificate::from_pem(ca.certificate_pem(), "devlanes test CA").expect("parse CA")
}

/// A sysroot with one layout's anchor directory.
fn sysroot(layout: CaStoreLayout) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("sysroot");
    std::fs::create_dir_all(root.path().join(layout.anchor_dir())).expect("anchors");
    root
}

/// Executor that performs `install` and `rm` on the tempdir sysroot like root would.
fn acting_executor(programs: &[&str]) -> Arc<FakeExecutor> {
    Arc::new(
        FakeExecutor::new(|command| {
            let argv = command.argv();
            let start = usize::from(argv[0] == "sudo") + usize::from(argv.get(1) == Some(&"-n"));
            match argv[start..] {
                ["install", "-m", "0644", from, to] => {
                    std::fs::copy(from, to)?;
                }
                ["rm", "-f", path] => {
                    std::fs::remove_file(path)?;
                }
                _ => {}
            }
            Ok(CommandOutput::ok(""))
        })
        .with_programs(programs),
    )
}

#[tokio::test]
async fn debian_install_copies_anchor_and_refreshes_with_sudo_n() {
    let root = sysroot(CaStoreLayout::Debian);
    let executor = acting_executor(&["sudo", "update-ca-certificates"]);
    let store = SystemStore::new(
        executor.clone(),
        root.path(),
        Privilege::Sudo { interactive: false },
    );
    let ca = ca();
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    store.install(&ca).await.expect("install");

    let anchor = root
        .path()
        .join("usr/local/share/ca-certificates/devlanes-test-ca.crt");
    let anchor_arg = anchor.display().to_string();
    let argvs = executor.argvs();
    assert_eq!(argvs.len(), 2);
    assert_eq!(argvs[0][..5], ["sudo", "-n", "install", "-m", "0644"]);
    assert_eq!(argvs[0][6], anchor_arg);
    assert_eq!(argvs[1], ["sudo", "-n", "update-ca-certificates"]);
    assert_eq!(std::fs::read_to_string(&anchor).expect("anchor"), ca.pem());
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );

    store.install(&ca).await.expect("reinstall");
    assert_eq!(executor.calls().len(), 2, "matching anchor needs no sudo");

    store.uninstall(&ca).await.expect("uninstall");
    let argvs = executor.argvs();
    assert_eq!(argvs[2], ["sudo", "-n", "rm", "-f", anchor_arg.as_str()]);
    assert_eq!(argvs[3], ["sudo", "-n", "update-ca-certificates"]);
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    store.uninstall(&ca).await.expect("uninstall again");
    assert_eq!(executor.calls().len(), 4, "absent anchor needs no sudo");
}

#[tokio::test]
async fn fedora_uses_update_ca_trust_and_interactive_sudo() {
    let root = sysroot(CaStoreLayout::Fedora);
    let executor = acting_executor(&["sudo", "update-ca-trust"]);
    let store = SystemStore::new(
        executor.clone(),
        root.path(),
        Privilege::Sudo { interactive: true },
    );
    store.install(&ca()).await.expect("install");
    let argvs = executor.argvs();
    assert_eq!(argvs[0][..4], ["sudo", "install", "-m", "0644"]);
    assert!(argvs[0][5].ends_with("etc/pki/ca-trust/source/anchors/devlanes-test-ca.pem"));
    assert_eq!(argvs[1], ["sudo", "update-ca-trust", "extract"]);
}

#[tokio::test]
async fn root_runs_commands_directly() {
    let root = sysroot(CaStoreLayout::Arch);
    let executor = acting_executor(&["update-ca-trust"]);
    let store = SystemStore::new(executor.clone(), root.path(), Privilege::Root);
    store.install(&ca()).await.expect("install");
    let argvs = executor.argvs();
    assert_eq!(argvs[0][..3], ["install", "-m", "0644"]);
    assert_eq!(argvs[1], ["update-ca-trust", "extract"]);
}

#[tokio::test]
async fn sudo_without_password_needs_interaction() {
    let root = sysroot(CaStoreLayout::Debian);
    let executor = Arc::new(
        FakeExecutor::new(|_| Ok(CommandOutput::failed(1, "sudo: a password is required\n")))
            .with_programs(&["sudo", "update-ca-certificates"]),
    );
    let store = SystemStore::new(
        executor,
        root.path(),
        Privilege::Sudo { interactive: false },
    );
    assert!(matches!(
        store.install(&ca()).await,
        Err(TrustError::InteractionRequired { command, .. }) if command.starts_with("sudo -n install")
    ));
}

#[tokio::test]
async fn failed_refresh_removes_the_anchor() {
    let root = sysroot(CaStoreLayout::Debian);
    let executor = Arc::new(
        FakeExecutor::new(|command| {
            let argv = command.argv();
            match argv[..] {
                ["install", "-m", "0644", from, to] => {
                    std::fs::copy(from, to)?;
                    Ok(CommandOutput::ok(""))
                }
                ["rm", "-f", path] => {
                    std::fs::remove_file(path)?;
                    Ok(CommandOutput::ok(""))
                }
                _ => Ok(CommandOutput::failed(2, "broken bundle")),
            }
        })
        .with_programs(&["update-ca-certificates"]),
    );
    let store = SystemStore::new(executor, root.path(), Privilege::Root);
    let ca = ca();
    assert!(matches!(
        store.install(&ca).await,
        Err(TrustError::CommandFailed { code: Some(2), .. })
    ));
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
}

#[tokio::test]
async fn missing_tools_and_layouts_are_reported() {
    let ca = ca();
    let bare = tempfile::tempdir().expect("sysroot");
    let executor = acting_executor(&["sudo", "update-ca-certificates"]);
    let store = SystemStore::new(executor.clone(), bare.path(), Privilege::Root);
    assert!(matches!(
        store.install(&ca).await,
        Err(TrustError::UnsupportedSystem(_))
    ));
    assert!(matches!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Unknown { .. }
    ));

    let root = sysroot(CaStoreLayout::Debian);
    let no_refresh = SystemStore::new(acting_executor(&["sudo"]), root.path(), Privilege::Root);
    assert!(matches!(
        no_refresh.install(&ca).await,
        Err(TrustError::ToolMissing { tool, .. }) if tool == "update-ca-certificates"
    ));
    let no_sudo = SystemStore::new(
        acting_executor(&["update-ca-certificates"]),
        root.path(),
        Privilege::Sudo { interactive: true },
    );
    assert!(matches!(
        no_sudo.install(&ca).await,
        Err(TrustError::ToolMissing { tool, .. }) if tool == "sudo"
    ));
    assert!(
        std::fs::read_dir(root.path().join(CaStoreLayout::Debian.anchor_dir()))
            .expect("anchors")
            .next()
            .is_none(),
        "no anchor is written when a tool is missing"
    );
}

#[test]
fn detection_follows_mkcert_order() {
    let root = tempfile::tempdir().expect("sysroot");
    assert_eq!(CaStoreLayout::detect(root.path()), None);
    for layout in [
        CaStoreLayout::Suse,
        CaStoreLayout::Arch,
        CaStoreLayout::Debian,
    ] {
        std::fs::create_dir_all(root.path().join(layout.anchor_dir())).expect("anchors");
        assert_eq!(CaStoreLayout::detect(root.path()), Some(layout));
    }
    std::fs::create_dir_all(root.path().join(CaStoreLayout::Fedora.anchor_dir())).expect("dir");
    assert_eq!(
        CaStoreLayout::detect(root.path()),
        Some(CaStoreLayout::Fedora)
    );
}

#[tokio::test]
async fn chrome_uses_pki_nssdb_and_firefox_scans_profiles() {
    let home = tempfile::tempdir().expect("home");
    let ca = ca();
    let pem = ca.pem().to_owned();
    let listed = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = listed.clone();
    let executor = Arc::new(
        FakeExecutor::new(move |command| {
            seen.lock().expect("lock").push(command.args.join(" "));
            Ok(match command.args.first().map(String::as_str) {
                Some("-L") => CommandOutput::ok(pem.clone()),
                _ => CommandOutput::ok(""),
            })
        })
        .with_programs(&["certutil"]),
    );
    let trust = LinuxTrust::new(executor.clone(), home.path(), Privilege::Root);
    let chrome = trust.store(TrustTarget::Chrome).expect("chrome");
    assert_eq!(chrome.id(), CHROMIUM_ID);
    chrome.install(&ca).await.expect("install");
    let nssdb = home.path().join(".pki/nssdb");
    assert!(nssdb.is_dir());
    let sql = format!("sql:{}", nssdb.display());
    assert_eq!(
        executor.argvs()[0],
        ["certutil", "-N", "-d", sql.as_str(), "--empty-password"]
    );

    let snap = home
        .path()
        .join("snap/firefox/common/.mozilla/firefox/x.default");
    std::fs::create_dir_all(&snap).expect("snap profile");
    std::fs::write(snap.join("cert9.db"), b"").expect("cert9.db");
    let firefox = trust.store(TrustTarget::Firefox).expect("firefox");
    assert_eq!(firefox.id(), FIREFOX_ID);
    assert_eq!(
        firefox.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );
    let last = listed.lock().expect("lock").pop().expect("probe");
    assert!(last.contains(&format!("sql:{}", snap.display())));

    assert_eq!(
        trust.store(TrustTarget::System).expect("system").id(),
        SYSTEM_ID
    );
    for target in [TrustTarget::IosSimulator, TrustTarget::AndroidEmulator] {
        assert!(matches!(
            trust.store(target),
            Err(TrustError::Unsupported { target: t }) if t == target
        ));
    }
}

#[tokio::test]
async fn missing_certutil_has_package_hint() {
    let home = tempfile::tempdir().expect("home");
    let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
    let chrome = LinuxTrust::new(executor, home.path(), Privilege::Root)
        .store(TrustTarget::Chrome)
        .expect("chrome");
    assert!(matches!(
        chrome.install(&ca()).await,
        Err(TrustError::ToolMissing { hint, .. }) if hint.contains("libnss3-tools")
    ));
}

#[cfg(target_os = "linux")]
mod real {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use devlanes_trust::SystemExecutor;

    use super::*;

    /// A CA with a per-process name so a leftover anchor never collides with a rerun.
    fn unique_ca(tag: &str) -> CaCertificate {
        let ca = ca();
        CaCertificate::from_pem(
            ca.pem(),
            format!("devlanes {tag} test {}", std::process::id()),
        )
        .expect("rename")
    }

    /// The anchor path the system store will use for `ca`.
    fn anchor_for(ca: &CaCertificate) -> Option<PathBuf> {
        let layout = CaStoreLayout::detect(Path::new("/"))?;
        Some(Path::new("/").join(layout.anchor_dir()).join(format!(
            "{}.{}",
            ca.file_stem(),
            layout.extension()
        )))
    }

    /// Removes the test anchor even when an assertion fails midway.
    struct AnchorGuard(Option<PathBuf>);

    impl Drop for AnchorGuard {
        fn drop(&mut self) {
            let Some(anchor) = self.0.take() else { return };
            if anchor.exists() {
                let _ = Command::new("sudo")
                    .args(["-n", "rm", "-f"])
                    .arg(&anchor)
                    .status();
                let _ = Command::new("sudo")
                    .args([
                        "-n",
                        "sh",
                        "-c",
                        "update-ca-certificates || update-ca-trust extract",
                    ])
                    .status();
            }
        }
    }

    /// Installs a test-owned CA into the real system store and removes it again.
    #[tokio::test]
    #[ignore = "needs sudo"]
    async fn system_store_round_trip() {
        let ca = unique_ca("system");
        let _guard = AnchorGuard(anchor_for(&ca));
        let home = tempfile::tempdir().expect("home");
        let store = LinuxTrust::new(Arc::new(SystemExecutor), home.path(), Privilege::detect())
            .store(TrustTarget::System)
            .expect("system");
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

    /// Creates and fills a throwaway `~/.pki/nssdb` with the real `certutil`.
    #[tokio::test]
    #[ignore = "needs certutil"]
    async fn chromium_nssdb_round_trip() {
        let home = tempfile::tempdir().expect("home");
        let ca = ca();
        let chrome = LinuxTrust::new(Arc::new(SystemExecutor), home.path(), Privilege::Root)
            .store(TrustTarget::Chrome)
            .expect("chrome");
        assert_eq!(
            chrome.status(&ca).await.expect("status"),
            TrustStatus::Missing
        );
        chrome.install(&ca).await.expect("install");
        assert_eq!(
            chrome.status(&ca).await.expect("status"),
            TrustStatus::Installed
        );
        chrome.uninstall(&ca).await.expect("uninstall");
        assert_eq!(
            chrome.status(&ca).await.expect("status"),
            TrustStatus::Missing
        );
    }
}
