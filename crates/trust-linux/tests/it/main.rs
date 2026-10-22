//! Argv tests for the Linux stores, plus ignored tests against real sudo and certutil.
#![cfg(unix)]

use std::sync::{Arc, Mutex};

use devlanes_trust::fake::FakeExecutor;
use devlanes_trust::{
    CaCertificate, CommandOutput, TrustError, TrustStatus, TrustStore as _, TrustTarget,
};
use devlanes_trust_linux::{
    CHROMIUM_ID, CaStoreLayout, FIREFOX_ID, LinuxTrust, Privilege, SYSTEM_ID, SystemStore,
    WINDOWS_ID, WINDOWS_ROOT, WindowsInterop, WindowsInteropStore,
};

const SUDO: &str = "/usr/bin/sudo";
const INSTALL: &str = "/usr/bin/install";
const RM: &str = "/bin/rm";
const UPDATE_CERTIFICATES: &str = "/usr/sbin/update-ca-certificates";
const UPDATE_TRUST: &str = "/usr/bin/update-ca-trust";

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

/// Executor with `install` and `rm` in the system directories plus `programs`, which
/// performs them on the tempdir sysroot like root would.
fn acting_executor(programs: &[&str]) -> Arc<FakeExecutor> {
    Arc::new(
        FakeExecutor::new(|command| {
            let argv = command.argv();
            let start = usize::from(argv[0] == SUDO) + usize::from(argv.get(1) == Some(&"-n"));
            match argv[start..] {
                [INSTALL, "-m", "0644", from, to] => {
                    std::fs::copy(from, to)?;
                }
                [RM, "-f", path] => {
                    std::fs::remove_file(path)?;
                }
                _ => {}
            }
            Ok(CommandOutput::ok(""))
        })
        .with_programs(&[INSTALL, RM])
        .with_programs(programs),
    )
}

#[tokio::test]
async fn debian_install_copies_anchor_and_refreshes_with_sudo_n() {
    let root = sysroot(CaStoreLayout::Debian);
    let executor = acting_executor(&[SUDO, UPDATE_CERTIFICATES]);
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
    assert_eq!(argvs[0][..5], [SUDO, "-n", INSTALL, "-m", "0644"]);
    assert_eq!(argvs[0][6], anchor_arg);
    assert_eq!(argvs[1], [SUDO, "-n", UPDATE_CERTIFICATES]);
    assert_eq!(std::fs::read_to_string(&anchor).expect("anchor"), ca.pem());
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Installed
    );

    store.install(&ca).await.expect("reinstall");
    assert_eq!(executor.calls().len(), 2, "matching anchor needs no sudo");

    store.uninstall(&ca).await.expect("uninstall");
    let argvs = executor.argvs();
    assert_eq!(argvs[2], [SUDO, "-n", RM, "-f", anchor_arg.as_str()]);
    assert_eq!(argvs[3], [SUDO, "-n", UPDATE_CERTIFICATES]);
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
    let executor = acting_executor(&[SUDO, UPDATE_TRUST]);
    let store = SystemStore::new(
        executor.clone(),
        root.path(),
        Privilege::Sudo { interactive: true },
    );
    store.install(&ca()).await.expect("install");
    let argvs = executor.argvs();
    assert_eq!(argvs[0][..4], [SUDO, INSTALL, "-m", "0644"]);
    assert!(argvs[0][5].ends_with("etc/pki/ca-trust/source/anchors/devlanes-test-ca.pem"));
    assert_eq!(argvs[1], [SUDO, UPDATE_TRUST, "extract"]);
}

#[tokio::test]
async fn root_runs_commands_directly() {
    let root = sysroot(CaStoreLayout::Arch);
    let executor = acting_executor(&[UPDATE_TRUST]);
    let store = SystemStore::new(executor.clone(), root.path(), Privilege::Root);
    store.install(&ca()).await.expect("install");
    let argvs = executor.argvs();
    assert_eq!(argvs[0][..3], [INSTALL, "-m", "0644"]);
    assert_eq!(argvs[1], [UPDATE_TRUST, "extract"]);
}

#[tokio::test]
async fn privileged_tools_never_come_from_path() {
    let root = sysroot(CaStoreLayout::Debian);
    // Bare names are what a hostile PATH entry would provide.
    let executor = Arc::new(
        FakeExecutor::new(|_| Ok(CommandOutput::ok(""))).with_programs(&[
            "sudo",
            "install",
            "rm",
            "update-ca-certificates",
        ]),
    );
    let store = SystemStore::new(
        executor.clone(),
        root.path(),
        Privilege::Sudo { interactive: true },
    );
    assert!(matches!(
        store.install(&ca()).await,
        Err(TrustError::ToolMissing { .. })
    ));
    assert!(executor.calls().is_empty());
}

#[tokio::test]
async fn sudo_without_password_needs_interaction() {
    let root = sysroot(CaStoreLayout::Debian);
    let executor = Arc::new(
        FakeExecutor::new(|_| Ok(CommandOutput::failed(1, "sudo: a password is required\n")))
            .with_programs(&[SUDO, INSTALL, RM, UPDATE_CERTIFICATES]),
    );
    let store = SystemStore::new(
        executor,
        root.path(),
        Privilege::Sudo { interactive: false },
    );
    assert!(matches!(
        store.install(&ca()).await,
        Err(TrustError::InteractionRequired { command, .. })
            if command.starts_with(&format!("{SUDO} -n {INSTALL}"))
    ));
}

#[tokio::test]
async fn failed_refresh_removes_the_anchor() {
    let root = sysroot(CaStoreLayout::Debian);
    let executor = Arc::new(
        FakeExecutor::new(|command| {
            let argv = command.argv();
            match argv[..] {
                [INSTALL, "-m", "0644", from, to] => {
                    std::fs::copy(from, to)?;
                    Ok(CommandOutput::ok(""))
                }
                [RM, "-f", path] => {
                    std::fs::remove_file(path)?;
                    Ok(CommandOutput::ok(""))
                }
                _ => Ok(CommandOutput::failed(2, "broken bundle")),
            }
        })
        .with_programs(&[INSTALL, RM, UPDATE_CERTIFICATES]),
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
    let executor = acting_executor(&[SUDO, UPDATE_CERTIFICATES]);
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
    let no_refresh = SystemStore::new(acting_executor(&[SUDO]), root.path(), Privilege::Root);
    assert!(matches!(
        no_refresh.install(&ca).await,
        Err(TrustError::ToolMissing { tool, .. }) if tool == "update-ca-certificates"
    ));
    let no_sudo = SystemStore::new(
        acting_executor(&[UPDATE_CERTIFICATES]),
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
                Some("-L") if command.args.len() == 3 => {
                    CommandOutput::ok("devlanes test CA                 C,,\n")
                }
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

const CERTUTIL_EXE: &str = "/mnt/c/Windows/System32/certutil.exe";
const WSLPATH: &str = "/usr/bin/wslpath";

/// `certutil.exe` under the usual System32 mount, changing `store`.
fn interop(store: &str) -> WindowsInterop {
    WindowsInterop {
        certutil: CERTUTIL_EXE.into(),
        store: store.into(),
    }
}

/// Answers like Windows' certutil with `held` in the user's store, and like
/// `wslpath -w` for the temp PEM, which must exist while certutil runs.
fn windows_executor(held: bool, add_result: CommandOutput) -> Arc<FakeExecutor> {
    let not_found = || CommandOutput {
        code: Some(17),
        stdout: "CertUtil: -store command FAILED: 0x80090011 (-2146893807 NTE_NOT_FOUND)\n".into(),
        stderr: String::new(),
    };
    Arc::new(
        FakeExecutor::new(move |command| {
            let argv = command.argv();
            Ok(match argv[..] {
                [WSLPATH, "-w", pem] => {
                    assert!(std::path::Path::new(pem).is_file(), "{pem}");
                    CommandOutput::ok("\\\\wsl.localhost\\Ubuntu\\tmp\\devlanes-trust-1.pem\n")
                }
                [CERTUTIL_EXE, "-user", "-store", _, hash] if held => {
                    CommandOutput::ok(format!("Cert Hash(sha1): {}\n", hash.to_lowercase()))
                }
                [CERTUTIL_EXE, .., "-addstore", _, _] => add_result.clone(),
                [CERTUTIL_EXE, _, "-store", ..] | [CERTUTIL_EXE, "-store", ..] => not_found(),
                _ => CommandOutput::ok(""),
            })
        })
        .with_programs(&[WSLPATH]),
    )
}

/// Outside WSL the `windows` target names why it is unavailable.
#[test]
fn windows_target_is_unavailable_outside_wsl() {
    let home = tempfile::tempdir().expect("home");
    let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
    let trust = LinuxTrust::new(executor.clone(), home.path(), Privilege::Root);
    assert!(matches!(
        trust.store(TrustTarget::Windows),
        Err(TrustError::Unavailable { target: TrustTarget::Windows, reason }) if reason.contains("WSL")
    ));
    let store = LinuxTrust::new(executor, home.path(), Privilege::Root)
        .with_windows(Ok(interop(WINDOWS_ROOT)))
        .store(TrustTarget::Windows)
        .expect("windows");
    assert_eq!(store.id(), WINDOWS_ID);
    assert_eq!(store.location().as_deref(), Some("Windows user root store"));
    assert!(store.approval().is_some());
}

/// Install looks in the user's and the machine's root store, converts the temp
/// PEM with `wslpath -w` and adds it to the user's `Root` without `-f`.
#[tokio::test]
async fn windows_install_adds_to_the_user_root() {
    let executor = windows_executor(false, CommandOutput::ok("added"));
    let store = WindowsInteropStore::new(executor.clone(), interop(WINDOWS_ROOT));
    let ca = ca();
    let hash = ca.sha1_hex();
    assert_eq!(
        store.status(&ca).await.expect("status"),
        TrustStatus::Missing
    );
    assert!(store.pending_approval(&ca, true).await.is_some());
    store.install(&ca).await.expect("install");
    let argvs = executor.argvs();
    let user = [CERTUTIL_EXE, "-user", "-store", "Root", hash.as_str()];
    let machine = [CERTUTIL_EXE, "-store", "Root", hash.as_str()];
    assert_eq!(argvs[0], user);
    assert_eq!(argvs[1], machine);
    let install = &argvs[argvs.len() - 4..];
    assert_eq!(install[0], user);
    assert_eq!(install[1], machine);
    assert_eq!(install[2][..2], [WSLPATH, "-w"]);
    assert_eq!(
        install[3],
        [
            CERTUTIL_EXE,
            "-user",
            "-addstore",
            "Root",
            r"\\wsl.localhost\Ubuntu\tmp\devlanes-trust-1.pem"
        ]
    );
    assert!(
        !std::path::Path::new(&install[2][2]).exists(),
        "temp PEM removed"
    );
}

/// A scratch store may not exist yet, so its add passes `-f`; it never prompts.
#[tokio::test]
async fn windows_scratch_store_is_created_with_force() {
    let executor = windows_executor(false, CommandOutput::ok("added"));
    let store = WindowsInteropStore::new(executor.clone(), interop("devlanes-test-1"));
    store.install(&ca()).await.expect("install");
    let add = executor.argvs().pop().expect("add");
    assert_eq!(add[..4], [CERTUTIL_EXE, "-f", "-user", "-addstore"]);
    assert!(store.approval().is_none());
    assert_eq!(
        store.location().as_deref(),
        Some(r"Cert:\CurrentUser\devlanes-test-1")
    );
}

/// A trusted CA is left alone; uninstall deletes it from the user's store by
/// thumbprint.
#[tokio::test]
async fn windows_uninstall_deletes_by_thumbprint() {
    let executor = windows_executor(true, CommandOutput::ok("added"));
    let store = WindowsInteropStore::new(executor.clone(), interop(WINDOWS_ROOT));
    let ca = ca();
    store.install(&ca).await.expect("install");
    assert_eq!(executor.argvs().len(), 1, "only the lookup ran");
    assert!(store.pending_approval(&ca, false).await.is_some());
    store.uninstall(&ca).await.expect("uninstall");
    assert_eq!(
        executor.argvs().pop().expect("delete"),
        [
            CERTUTIL_EXE,
            "-user",
            "-delstore",
            "Root",
            ca.sha1_hex().as_str()
        ]
    );
}

/// No in the dialog comes back as `ERROR_CANCELLED` and is reported as declined.
#[tokio::test]
async fn windows_declined_dialog_is_cancelled() {
    let declined = CommandOutput {
        code: Some(199),
        stdout: "CertUtil: -addstore command FAILED: 0x800704c7 (WIN32: 1223 ERROR_CANCELLED)\n"
            .into(),
        stderr: String::new(),
    };
    let store = WindowsInteropStore::new(windows_executor(false, declined), interop(WINDOWS_ROOT));
    assert!(matches!(
        store.install(&ca()).await,
        Err(TrustError::Cancelled { detail }) if detail.contains("devlanes certs trust")
    ));
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

    /// Deletes a scratch CurrentUser store with `reg.exe` when the test ends:
    /// `certutil` can empty a store but not remove it.
    struct ScratchStore {
        name: String,
        reg: PathBuf,
    }

    impl Drop for ScratchStore {
        fn drop(&mut self) {
            let key = format!(r"HKCU\Software\Microsoft\SystemCertificates\{}", self.name);
            let removed = Command::new(&self.reg)
                .args(["delete", &key, "/f"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !removed {
                eprintln!("leaked certificate store {key}");
            }
        }
    }

    /// Adds a test CA to a scratch Windows store through WSL interop, reads it
    /// back and deletes it, with the real `certutil.exe` and `wslpath`. Scratch
    /// stores never show the root store's dialog.
    #[tokio::test]
    #[ignore = "needs WSL with interop"]
    async fn windows_scratch_store_round_trip() {
        let detected = WindowsInterop::detect().expect("WSL with interop");
        let name = format!(
            "devlanes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        let _scratch = ScratchStore {
            name: name.clone(),
            reg: detected.certutil.with_file_name("reg.exe"),
        };
        let store = WindowsInteropStore::new(Arc::new(SystemExecutor), detected.with_store(&name));
        let ca = unique_ca("windows");
        let status = async || store.status(&ca).await.expect("status");
        assert_eq!(status().await, TrustStatus::Missing);
        store.install(&ca).await.expect("install");
        assert_eq!(status().await, TrustStatus::Installed);
        store.install(&ca).await.expect("install again");
        store.uninstall(&ca).await.expect("uninstall");
        assert_eq!(status().await, TrustStatus::Missing);
        store.uninstall(&ca).await.expect("uninstall again");
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
