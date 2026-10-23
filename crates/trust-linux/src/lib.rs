//! Linux trust stores: the distribution CA bundle, Chromium's NSS database, and
//! Firefox profiles.
//!
//! The system store needs root. Commands go through `sudo`, with `-n` when stdin is
//! not a terminal so CI with passwordless sudo works and a missing password fails
//! fast with [`TrustError::InteractionRequired`] instead of hanging. In a terminal,
//! sudo prompts on `/dev/tty`. NSS databases belong to the user and need no sudo.

// Builds on macOS so the argv tests run there too; empty on Windows, which uses
// `trust-windows`.
#![cfg(unix)]

use std::io::{self, IsTerminal as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use devlanes_trust::nss::{CertutilLookup, NssDatabases, NssStore};
use devlanes_trust::{
    CaCertificate, CommandExecutor, CommandLine, CommandOutput, SystemExecutor, TempPem,
    TrustError, TrustStatus, TrustStore, TrustTarget, find_system_program, path_arg, run,
};

/// Store id of the distribution CA bundle.
pub const SYSTEM_ID: &str = "linux-system";
/// Store id of `~/.pki/nssdb`.
pub const CHROMIUM_ID: &str = "nss-chromium";
/// Store id of the Firefox profiles.
pub const FIREFOX_ID: &str = "nss-firefox";

/// Shown when the `firefox` or `chrome` target needs `certutil`.
pub const CERTUTIL_HINT: &str = "install NSS tools: `sudo apt install libnss3-tools` \
     (Debian/Ubuntu), `sudo dnf install nss-tools` (Fedora) or `sudo pacman -S nss` (Arch)";

/// Shown when a refresh tool is missing.
const CA_TOOLS_HINT: &str = "install the `ca-certificates` package";

/// Shown when sudo is missing.
const SUDO_HINT: &str = "run `devlanes certs trust` as root or install sudo";

/// Shown if `install` or `rm` is missing from the system directories.
const COREUTILS_HINT: &str = "install coreutils";

/// Absolute paths of the root-run tools for one change.
struct Tools {
    install: String,
    rm: String,
    refresh: String,
    refresh_args: Vec<String>,
}

/// How root commands are run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// Already root: run commands directly.
    Root,
    /// Prefix with `sudo`; `interactive: false` adds `-n`.
    Sudo {
        /// Whether sudo may prompt for a password.
        interactive: bool,
    },
}

impl Privilege {
    /// Root when the effective uid is 0, else sudo, interactive only with a terminal on stdin.
    pub fn detect() -> Self {
        if rustix::process::geteuid().is_root() {
            Self::Root
        } else {
            Self::Sudo {
                interactive: io::stdin().is_terminal(),
            }
        }
    }
}

/// A distribution's CA anchor directory and refresh command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaStoreLayout {
    /// Fedora, RHEL, CentOS: `update-ca-trust`.
    Fedora,
    /// Debian, Ubuntu: `update-ca-certificates`.
    Debian,
    /// Arch: p11-kit anchors with `update-ca-trust`.
    Arch,
    /// openSUSE: `update-ca-certificates` over p11-kit anchors.
    Suse,
}

impl CaStoreLayout {
    /// Detection order, matching mkcert's, so hosts with several directories behave alike.
    pub const ALL: [Self; 4] = [Self::Fedora, Self::Debian, Self::Arch, Self::Suse];

    /// Anchor directory relative to the system root.
    pub fn anchor_dir(self) -> &'static str {
        match self {
            Self::Fedora => "etc/pki/ca-trust/source/anchors",
            Self::Debian => "usr/local/share/ca-certificates",
            Self::Arch => "etc/ca-certificates/trust-source/anchors",
            Self::Suse => "usr/share/pki/trust/anchors",
        }
    }

    /// Anchor file extension; `update-ca-certificates` only reads `.crt`.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Fedora | Self::Suse => "pem",
            Self::Debian | Self::Arch => "crt",
        }
    }

    /// Program and arguments that rebuild the bundles from the anchors.
    pub fn refresh(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Fedora | Self::Arch => ("update-ca-trust", &["extract"]),
            Self::Debian | Self::Suse => ("update-ca-certificates", &[]),
        }
    }

    /// First layout whose anchor directory exists under `sysroot`. Blocking.
    pub fn detect(sysroot: &Path) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|layout| sysroot.join(layout.anchor_dir()).is_dir())
    }
}

/// Builds the Linux store for each [`TrustTarget`].
pub struct LinuxTrust {
    executor: Arc<dyn CommandExecutor>,
    home: PathBuf,
    sysroot: PathBuf,
    privilege: Privilege,
}

impl LinuxTrust {
    /// Uses `executor` for commands, `home` for NSS databases and `/` as system root.
    pub fn new(
        executor: Arc<dyn CommandExecutor>,
        home: impl Into<PathBuf>,
        privilege: Privilege,
    ) -> Self {
        Self {
            executor,
            home: home.into(),
            sysroot: PathBuf::from("/"),
            privilege,
        }
    }

    /// Replaces the system root, so tests can lay out anchor directories in a tempdir.
    #[must_use]
    pub fn with_sysroot(mut self, sysroot: impl Into<PathBuf>) -> Self {
        self.sysroot = sysroot.into();
        self
    }

    /// Real commands, `$HOME` and [`Privilege::detect`].
    pub fn from_env() -> Result<Self, TrustError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            TrustError::io(
                "finding the home directory",
                io::Error::new(io::ErrorKind::NotFound, "HOME is not set"),
            )
        })?;
        Ok(Self::new(
            Arc::new(SystemExecutor),
            PathBuf::from(home),
            Privilege::detect(),
        ))
    }

    /// The store for `target`; mobile targets are [`TrustError::Unsupported`].
    pub fn store(&self, target: TrustTarget) -> Result<Box<dyn TrustStore>, TrustError> {
        match target {
            TrustTarget::System => Ok(Box::new(SystemStore::new(
                self.executor.clone(),
                &self.sysroot,
                self.privilege,
            ))),
            TrustTarget::Chrome => Ok(Box::new(NssStore::new(
                self.executor.clone(),
                TrustTarget::Chrome,
                CHROMIUM_ID,
                NssDatabases::Shared {
                    dir: chromium_nssdb(&self.home),
                },
                certutil_lookup(),
            ))),
            TrustTarget::Firefox => Ok(Box::new(NssStore::new(
                self.executor.clone(),
                TrustTarget::Firefox,
                FIREFOX_ID,
                NssDatabases::Profiles {
                    roots: firefox_profile_roots(&self.home),
                },
                certutil_lookup(),
            ))),
            TrustTarget::IosSimulator | TrustTarget::AndroidEmulator => {
                Err(TrustError::Unsupported { target })
            }
        }
    }
}

/// `~/.pki/nssdb`, shared by Chrome, Chromium and Playwright's Chromium.
pub fn chromium_nssdb(home: &Path) -> PathBuf {
    home.join(".pki/nssdb")
}

/// Firefox profile roots for the deb/rpm, snap and Flatpak packages.
pub fn firefox_profile_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
        home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ]
}

/// `certutil` lookup on `PATH` with distribution install hints.
pub fn certutil_lookup() -> CertutilLookup {
    CertutilLookup {
        extra_dirs: Vec::new(),
        hint: CERTUTIL_HINT.into(),
    }
}

/// The distribution CA bundle, changed through an anchor file and a refresh command.
pub struct SystemStore {
    executor: Arc<dyn CommandExecutor>,
    sysroot: PathBuf,
    privilege: Privilege,
}

impl SystemStore {
    /// Manages anchors under `sysroot` (normally `/`).
    pub fn new(
        executor: Arc<dyn CommandExecutor>,
        sysroot: impl Into<PathBuf>,
        privilege: Privilege,
    ) -> Self {
        Self {
            executor,
            sysroot: sysroot.into(),
            privilege,
        }
    }

    /// Detects the layout on a blocking thread.
    pub async fn layout(&self) -> Result<CaStoreLayout, TrustError> {
        let sysroot = self.sysroot.clone();
        tokio::task::spawn_blocking(move || CaStoreLayout::detect(&sysroot))
            .await
            .map_err(|error| TrustError::io("detecting the CA store", io::Error::other(error)))?
            .ok_or_else(|| {
                let searched = CaStoreLayout::ALL
                    .iter()
                    .map(|layout| self.sysroot.join(layout.anchor_dir()).display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                TrustError::UnsupportedSystem(format!("none of {searched} exist"))
            })
    }

    /// The anchor file for `ca` under `layout`.
    pub fn anchor_path(&self, layout: CaStoreLayout, ca: &CaCertificate) -> PathBuf {
        self.sysroot.join(layout.anchor_dir()).join(format!(
            "{}.{}",
            ca.file_stem(),
            layout.extension()
        ))
    }

    /// Reads the anchor file on a blocking thread; `None` when absent.
    async fn read_anchor(&self, path: &Path) -> Result<Option<Vec<u8>>, TrustError> {
        let owned = path.to_owned();
        let read = tokio::task::spawn_blocking(move || std::fs::read(owned))
            .await
            .map_err(|error| TrustError::io("reading the CA anchor", io::Error::other(error)))?;
        match read {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(TrustError::io(format!("reading {}", path.display()), error)),
        }
    }

    /// Absolute path of a root-run tool from the system directories, never `PATH`.
    fn system_program(&self, name: &str, hint: &str) -> Result<String, TrustError> {
        let path = find_system_program(self.executor.as_ref(), name).ok_or_else(|| {
            TrustError::ToolMissing {
                tool: name.into(),
                hint: hint.into(),
            }
        })?;
        path_arg(&path)
    }

    /// Resolves every tool a change needs before running any of them, so a missing
    /// tool leaves no stray anchor.
    fn tools(&self, layout: CaStoreLayout) -> Result<Tools, TrustError> {
        let (refresh, args) = layout.refresh();
        Ok(Tools {
            install: self.system_program("install", COREUTILS_HINT)?,
            rm: self.system_program("rm", COREUTILS_HINT)?,
            refresh: self.system_program(refresh, CA_TOOLS_HINT)?,
            refresh_args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        })
    }

    /// Runs the absolute `program` with `args` as root and requires success.
    async fn privileged(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<CommandOutput, TrustError> {
        let command = match self.privilege {
            Privilege::Root => CommandLine::new(program).args(args.iter().cloned()),
            Privilege::Sudo { interactive } => {
                let mut command = CommandLine::new(self.system_program("sudo", SUDO_HINT)?);
                if !interactive {
                    command = command.arg("-n");
                }
                command.arg(program).args(args.iter().cloned())
            }
        };
        let output = run(self.executor.as_ref(), &command, SUDO_HINT).await?;
        if !output.success() && needs_password(&output.stderr) {
            return Err(TrustError::InteractionRequired {
                command: command.to_string(),
                detail: "sudo needs a password; rerun `devlanes certs trust` in a terminal or \
                         allow passwordless sudo"
                    .into(),
            });
        }
        output.into_checked(&command)
    }
}

#[async_trait]
impl TrustStore for SystemStore {
    /// Always [`TrustTarget::System`].
    fn target(&self) -> TrustTarget {
        TrustTarget::System
    }

    /// [`SYSTEM_ID`].
    fn id(&self) -> &'static str {
        SYSTEM_ID
    }

    /// Names the sysroot and the sudo requirement.
    fn describe(&self) -> String {
        let how = match self.privilege {
            Privilege::Root => "as root",
            Privilege::Sudo { .. } => "with sudo",
        };
        format!(
            "system CA store under {} (changed {how})",
            self.sysroot.display()
        )
    }

    /// Copies the PEM into the anchor directory with mode 0644 and rebuilds the bundles.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let layout = self.layout().await?;
        let anchor = self.anchor_path(layout, ca);
        if let Some(existing) = self.read_anchor(&anchor).await?
            && ca.matches_pem(&existing)
        {
            return Ok(());
        }
        let tools = self.tools(layout)?;
        if let Privilege::Sudo { .. } = self.privilege {
            self.system_program("sudo", SUDO_HINT)?;
        }
        let pem = TempPem::create(ca).await?;
        let copy = vec![
            "-m".to_owned(),
            "0644".to_owned(),
            pem.arg().to_owned(),
            path_arg(&anchor)?,
        ];
        self.privileged(&tools.install, &copy).await?;
        pem.close().await;
        if let Err(error) = self.privileged(&tools.refresh, &tools.refresh_args).await {
            // Status only reads the anchor, so a stale anchor would report Installed
            // and make the next install skip the refresh. Removal is best effort.
            let _ = self
                .privileged(&tools.rm, &["-f".to_owned(), path_arg(&anchor)?])
                .await;
            return Err(error);
        }
        Ok(())
    }

    /// Deletes the anchor file, whatever certificate it holds, and rebuilds the bundles.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        let layout = self.layout().await?;
        let anchor = self.anchor_path(layout, ca);
        if self.read_anchor(&anchor).await?.is_none() {
            return Ok(());
        }
        let tools = self.tools(layout)?;
        self.privileged(&tools.rm, &["-f".to_owned(), path_arg(&anchor)?])
            .await?;
        self.privileged(&tools.refresh, &tools.refresh_args).await?;
        Ok(())
    }

    /// Installed when the anchor file holds this exact certificate.
    ///
    /// The generated bundle is not parsed because its path differs per distribution;
    /// install removes the anchor again when the refresh fails, keeping the two in step.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        let layout = match self.layout().await {
            Ok(layout) => layout,
            Err(error) => {
                return Ok(TrustStatus::Unknown {
                    reason: error.to_string(),
                });
            }
        };
        let anchor = self.anchor_path(layout, ca);
        Ok(match self.read_anchor(&anchor).await {
            Ok(Some(bytes)) if ca.matches_pem(&bytes) => TrustStatus::Installed,
            Ok(_) => TrustStatus::Missing,
            Err(error) => TrustStatus::Unknown {
                reason: error.to_string(),
            },
        })
    }
}

/// Whether sudo refused because it would have to prompt.
fn needs_password(stderr: &str) -> bool {
    stderr.contains("a password is required") || stderr.contains("a terminal is required")
}
