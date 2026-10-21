//! NSS databases (Firefox profiles, Chromium's `~/.pki/nssdb`) through `certutil`.
//!
//! Only `sql:` databases (`cert9.db`) are handled; NSS dropped the legacy `cert8.db`
//! format years ago. The `sql:` backend tolerates a running browser, so Firefox
//! does not need to be closed.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    CaCertificate, CommandExecutor, CommandLine, TempPem, TrustError, TrustStatus, TrustStore,
    TrustTarget, path_arg, run, run_checked,
};

/// Removing by nickname deletes one certificate per call; this bounds the loop
/// that clears certificates left by older CAs under the same nickname.
const MAX_DELETES: usize = 16;

/// Which NSS databases a store manages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NssDatabases {
    /// One shared database, created on install when absent (Chromium's `~/.pki/nssdb`,
    /// also read by Playwright's Chromium).
    Shared {
        /// Database directory.
        dir: PathBuf,
    },
    /// Every directory directly under these roots that holds a `cert9.db` (Firefox profiles).
    Profiles {
        /// Profile roots, for example `~/.mozilla/firefox`.
        roots: Vec<PathBuf>,
    },
}

/// Where to look for `certutil` beyond `PATH`, and how to install it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertutilLookup {
    /// Extra directories, for example Homebrew's `nss` keg.
    pub extra_dirs: Vec<PathBuf>,
    /// Install hint shown when `certutil` is missing.
    pub hint: String,
}

/// A [`TrustStore`] over one or more NSS databases.
pub struct NssStore {
    executor: Arc<dyn CommandExecutor>,
    target: TrustTarget,
    id: &'static str,
    databases: NssDatabases,
    certutil: CertutilLookup,
}

impl NssStore {
    /// Builds a store for `target`; `id` must be unique among the platform's stores.
    pub fn new(
        executor: Arc<dyn CommandExecutor>,
        target: TrustTarget,
        id: &'static str,
        databases: NssDatabases,
        certutil: CertutilLookup,
    ) -> Self {
        Self {
            executor,
            target,
            id,
            databases,
            certutil,
        }
    }

    /// Existing databases, found on a blocking thread.
    pub async fn existing_databases(&self) -> Result<Vec<PathBuf>, TrustError> {
        let databases = self.databases.clone();
        tokio::task::spawn_blocking(move || discover(&databases))
            .await
            .map_err(|error| TrustError::io("listing NSS databases", io::Error::other(error)))
    }

    /// Resolves `certutil` or explains how to install it.
    fn certutil(&self) -> Result<String, TrustError> {
        let path = self
            .executor
            .find_program("certutil", &self.certutil.extra_dirs)
            .ok_or_else(|| TrustError::ToolMissing {
                tool: "certutil".into(),
                hint: self.certutil.hint.clone(),
            })?;
        path_arg(&path)
    }

    /// Status of `ca` in one database.
    async fn database_status(
        &self,
        certutil: &str,
        db: &Path,
        ca: &CaCertificate,
    ) -> Result<TrustStatus, TrustError> {
        let command = CommandLine::new(certutil)
            .args(["-L", "-d"])
            .arg(sql_arg(db)?)
            .args(["-n", ca.name(), "-a"]);
        let output = run(self.executor.as_ref(), &command, &self.certutil.hint).await?;
        Ok(if output.success() {
            if ca.matches_pem(output.stdout.as_bytes()) {
                TrustStatus::Installed
            } else {
                TrustStatus::Missing
            }
        } else if is_not_found(&output.stderr) {
            TrustStatus::Missing
        } else {
            TrustStatus::Unknown {
                reason: format!("{}: {}", db.display(), output.stderr.trim()),
            }
        })
    }

    /// Deletes every certificate under the CA's nickname in one database.
    async fn delete_all(
        &self,
        certutil: &str,
        db: &Path,
        ca: &CaCertificate,
    ) -> Result<(), TrustError> {
        let command = CommandLine::new(certutil)
            .args(["-D", "-d"])
            .arg(sql_arg(db)?)
            .args(["-n", ca.name()]);
        for _ in 0..MAX_DELETES {
            let output = run(self.executor.as_ref(), &command, &self.certutil.hint).await?;
            if !output.success() {
                if is_not_found(&output.stderr) {
                    return Ok(());
                }
                output.into_checked(&command)?;
            }
        }
        Ok(())
    }

    /// Creates the shared database so Chromium finds the CA on its first start.
    async fn create_shared(&self, certutil: &str, dir: &Path) -> Result<(), TrustError> {
        let owned = dir.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::DirBuilderExt as _;
            // Chromium creates this directory 0700; match it since it can hold client keys.
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&owned)
        })
        .await
        .map_err(|error| TrustError::io("creating the NSS database", io::Error::other(error)))?
        .map_err(|error| TrustError::io(format!("creating {}", dir.display()), error))?;
        let command = CommandLine::new(certutil)
            .args(["-N", "-d"])
            .arg(sql_arg(dir)?)
            .arg("--empty-password");
        run_checked(self.executor.as_ref(), &command, &self.certutil.hint).await?;
        Ok(())
    }
}

#[async_trait]
impl TrustStore for NssStore {
    /// The target given at construction.
    fn target(&self) -> TrustTarget {
        self.target
    }

    /// The id given at construction.
    fn id(&self) -> &'static str {
        self.id
    }

    /// Names the database directory or profile roots.
    fn describe(&self) -> String {
        match &self.databases {
            NssDatabases::Shared { dir } => format!("NSS database {}", dir.display()),
            NssDatabases::Profiles { roots } => {
                let roots = roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} profiles under {roots}", self.target)
            }
        }
    }

    /// Adds the CA with `C,,` (trusted to issue TLS server certificates) to every database.
    ///
    /// A database that holds a different certificate under the nickname, left by an
    /// older CA, has it removed first because NSS refuses duplicate nicknames.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let certutil = self.certutil()?;
        let mut databases = self.existing_databases().await?;
        if databases.is_empty() {
            match &self.databases {
                NssDatabases::Shared { dir } => {
                    self.create_shared(&certutil, dir).await?;
                    databases.push(dir.clone());
                }
                NssDatabases::Profiles { roots } => {
                    return Err(TrustError::NoDatabases {
                        target: self.target,
                        searched: roots.clone(),
                    });
                }
            }
        }
        let pem = TempPem::create(ca).await?;
        for db in &databases {
            if self.database_status(&certutil, db, ca).await? == TrustStatus::Installed {
                continue;
            }
            self.delete_all(&certutil, db, ca).await?;
            let command = CommandLine::new(&certutil)
                .args(["-A", "-d"])
                .arg(sql_arg(db)?)
                .args(["-t", "C,,", "-n", ca.name(), "-i", pem.arg()]);
            run_checked(self.executor.as_ref(), &command, &self.certutil.hint).await?;
        }
        pem.close().await;
        Ok(())
    }

    /// Deletes the nickname from every database; absent databases need no change.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let databases = self.existing_databases().await?;
        if databases.is_empty() {
            return Ok(());
        }
        let certutil = self.certutil()?;
        for db in &databases {
            self.delete_all(&certutil, db, ca).await?;
        }
        Ok(())
    }

    /// Installed only when every database holds this exact certificate.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        let databases = self.existing_databases().await?;
        if databases.is_empty() {
            return Ok(match &self.databases {
                NssDatabases::Shared { .. } => TrustStatus::Missing,
                NssDatabases::Profiles { .. } => TrustStatus::Unknown {
                    reason: format!("no {} profiles found", self.target),
                },
            });
        }
        let certutil = match self.certutil() {
            Ok(certutil) => certutil,
            Err(error) => {
                return Ok(TrustStatus::Unknown {
                    reason: error.to_string(),
                });
            }
        };
        let mut combined = TrustStatus::Installed;
        for db in &databases {
            match self.database_status(&certutil, db, ca).await? {
                TrustStatus::Installed => {}
                // Missing is actionable (`devlanes trust` fixes it), so it wins over Unknown.
                TrustStatus::Missing => return Ok(TrustStatus::Missing),
                unknown @ TrustStatus::Unknown { .. } => combined = unknown,
            }
        }
        Ok(combined)
    }
}

/// The `-d` argument for a `sql:` database.
fn sql_arg(db: &Path) -> Result<String, TrustError> {
    Ok(format!("sql:{}", path_arg(db)?))
}

/// Whether certutil's stderr says the nickname or database is absent.
fn is_not_found(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("could not find") || stderr.contains("not found")
}

/// Lists existing databases; unreadable roots count as empty because a missing
/// browser is the common case, not an error.
fn discover(databases: &NssDatabases) -> Vec<PathBuf> {
    match databases {
        NssDatabases::Shared { dir } => {
            if dir.join("cert9.db").is_file() {
                vec![dir.clone()]
            } else {
                Vec::new()
            }
        }
        NssDatabases::Profiles { roots } => {
            let mut found = Vec::new();
            for root in roots {
                let Ok(entries) = std::fs::read_dir(root) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.join("cert9.db").is_file() {
                        found.push(path);
                    }
                }
            }
            found.sort();
            found
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::CommandOutput;
    use crate::fake::FakeExecutor;

    fn ca() -> CaCertificate {
        CaCertificate::from_pem(&crate::test_ca_pem(), "devlanes test CA").expect("parse")
    }

    fn lookup() -> CertutilLookup {
        CertutilLookup {
            extra_dirs: Vec::new(),
            hint: "install nss".into(),
        }
    }

    fn profile(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("profile dir");
        std::fs::write(dir.join("cert9.db"), b"").expect("cert9.db");
        dir
    }

    fn not_found() -> CommandOutput {
        CommandOutput::failed(
            255,
            "certutil: Could not find cert: devlanes test CA\n: PR_FILE_NOT_FOUND_ERROR: File not found",
        )
    }

    #[tokio::test]
    async fn installs_into_every_firefox_profile() {
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join("profiles");
        let first = profile(&root, "a.default");
        let second = profile(&root, "b.dev");
        std::fs::create_dir_all(root.join("no-db")).expect("dir");
        let ca = ca();
        let added = Arc::new(Mutex::new(Vec::new()));
        let seen = added.clone();
        let executor = Arc::new(
            FakeExecutor::new(move |command| {
                if command.args.first().map(String::as_str) == Some("-A") {
                    let file = command.args.last().expect("pem path");
                    let pem = std::fs::read_to_string(file).expect("temp pem exists during run");
                    seen.lock().expect("lock").push(pem);
                    return Ok(CommandOutput::ok(""));
                }
                if command.args.first().map(String::as_str) == Some("-L") {
                    return Ok(not_found());
                }
                Ok(not_found())
            })
            .with_programs(&["certutil"]),
        );
        let store = NssStore::new(
            executor.clone(),
            TrustTarget::Firefox,
            "nss-firefox",
            NssDatabases::Profiles {
                roots: vec![root.clone()],
            },
            lookup(),
        );
        store.install(&ca).await.expect("install");

        let first_db = format!("sql:{}", first.display());
        let second_db = format!("sql:{}", second.display());
        let argvs = executor.argvs();
        let adds: Vec<_> = argvs.iter().filter(|argv| argv[1] == "-A").collect();
        assert_eq!(adds.len(), 2);
        assert_eq!(
            adds[0][..9],
            [
                "certutil",
                "-A",
                "-d",
                &first_db,
                "-t",
                "C,,",
                "-n",
                "devlanes test CA",
                "-i"
            ]
        );
        assert_eq!(adds[1][3], second_db);
        assert_eq!(
            argvs[0],
            [
                "certutil",
                "-L",
                "-d",
                &first_db,
                "-n",
                "devlanes test CA",
                "-a"
            ]
        );
        assert_eq!(
            argvs[1],
            ["certutil", "-D", "-d", &first_db, "-n", "devlanes test CA"]
        );
        let added = added.lock().expect("lock");
        assert!(added.iter().all(|pem| pem == ca.pem()));
    }

    #[tokio::test]
    async fn skips_databases_that_already_trust_the_ca() {
        let home = tempfile::tempdir().expect("home");
        let db = home.path().join("nssdb");
        std::fs::create_dir_all(&db).expect("dir");
        std::fs::write(db.join("cert9.db"), b"").expect("db");
        let ca = ca();
        let pem = ca.pem().to_owned();
        let executor = Arc::new(
            FakeExecutor::new(move |_| Ok(CommandOutput::ok(pem.clone())))
                .with_programs(&["certutil"]),
        );
        let store = NssStore::new(
            executor.clone(),
            TrustTarget::Chrome,
            "nss-chromium",
            NssDatabases::Shared { dir: db },
            lookup(),
        );
        store.install(&ca).await.expect("install");
        assert_eq!(executor.argvs().len(), 1, "only the status probe runs");
        assert_eq!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Installed
        );
    }

    #[tokio::test]
    async fn creates_missing_shared_database() {
        let home = tempfile::tempdir().expect("home");
        let db = home.path().join(".pki/nssdb");
        let executor = Arc::new(
            FakeExecutor::new(|command| {
                Ok(match command.args.first().map(String::as_str) {
                    Some("-L" | "-D") => not_found(),
                    _ => CommandOutput::ok(""),
                })
            })
            .with_programs(&["certutil"]),
        );
        let store = NssStore::new(
            executor.clone(),
            TrustTarget::Chrome,
            "nss-chromium",
            NssDatabases::Shared { dir: db.clone() },
            lookup(),
        );
        assert_eq!(
            store.status(&ca()).await.expect("status"),
            TrustStatus::Missing
        );
        store.install(&ca()).await.expect("install");
        let sql = format!("sql:{}", db.display());
        assert_eq!(
            executor.argvs()[0],
            ["certutil", "-N", "-d", &sql, "--empty-password"]
        );
        assert!(db.is_dir());
    }

    #[tokio::test]
    async fn reports_missing_certutil_and_profiles() {
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join("profiles");
        let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
        let store = NssStore::new(
            executor.clone(),
            TrustTarget::Firefox,
            "nss-firefox",
            NssDatabases::Profiles {
                roots: vec![root.clone()],
            },
            lookup(),
        );
        let ca = ca();
        assert!(matches!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Unknown { .. }
        ));
        assert!(matches!(
            store.install(&ca).await,
            Err(TrustError::ToolMissing { hint, .. }) if hint == "install nss"
        ));
        profile(&root, "p");
        assert!(matches!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Unknown { reason } if reason.contains("install nss")
        ));
        store
            .uninstall(&ca)
            .await
            .expect_err("certutil needed to remove");
        assert!(executor.calls().is_empty());

        let empty = NssStore::new(
            Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))).with_programs(&["certutil"])),
            TrustTarget::Firefox,
            "nss-firefox",
            NssDatabases::Profiles {
                roots: vec![home.path().join("none")],
            },
            lookup(),
        );
        assert!(matches!(
            empty.install(&ca).await,
            Err(TrustError::NoDatabases {
                target: TrustTarget::Firefox,
                ..
            })
        ));
        empty.uninstall(&ca).await.expect("nothing to remove");
    }

    #[tokio::test]
    async fn uninstall_deletes_until_nickname_is_gone() {
        let home = tempfile::tempdir().expect("home");
        let db = profile(home.path(), "nssdb");
        let deletes = Arc::new(Mutex::new(0));
        let count = deletes.clone();
        let executor = Arc::new(
            FakeExecutor::new(move |_| {
                let mut count = count.lock().expect("lock");
                *count += 1;
                Ok(if *count <= 2 {
                    CommandOutput::ok("")
                } else {
                    not_found()
                })
            })
            .with_programs(&["certutil"]),
        );
        let store = NssStore::new(
            executor.clone(),
            TrustTarget::Chrome,
            "nss-chromium",
            NssDatabases::Shared { dir: db },
            lookup(),
        );
        store.uninstall(&ca()).await.expect("uninstall");
        assert_eq!(executor.calls().len(), 3);
    }

    #[tokio::test]
    async fn unexpected_certutil_failure_is_reported() {
        let home = tempfile::tempdir().expect("home");
        let db = profile(home.path(), "nssdb");
        let executor = Arc::new(
            FakeExecutor::new(|_| Ok(CommandOutput::failed(1, "SEC_ERROR_BAD_DATABASE")))
                .with_programs(&["certutil"]),
        );
        let store = NssStore::new(
            executor,
            TrustTarget::Chrome,
            "nss-chromium",
            NssDatabases::Shared { dir: db },
            lookup(),
        );
        let ca = ca();
        assert!(matches!(
            store.status(&ca).await.expect("status"),
            TrustStatus::Unknown { reason } if reason.contains("SEC_ERROR_BAD_DATABASE")
        ));
        assert!(matches!(
            store.uninstall(&ca).await,
            Err(TrustError::CommandFailed { code: Some(1), .. })
        ));
    }
}
