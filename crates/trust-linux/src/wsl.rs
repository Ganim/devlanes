//! The `windows` target under WSL: the Windows user root store, changed with
//! Windows' own `certutil.exe` through WSL interop, so Windows browsers (Edge,
//! Chrome, Firefox with enterprise roots) trust the CA of a daemon in the distro.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use devlanes_trust::{
    CaCertificate, CommandExecutor, CommandLine, CommandOutput, TempPem, TrustError, TrustStatus,
    TrustStore, TrustTarget, find_system_program, path_arg, run,
};

/// Store id of the Windows user root store reached from WSL.
pub const WINDOWS_ID: &str = "wsl-windows-user-root";

/// The CurrentUser store Windows browsers trust as roots.
pub const WINDOWS_ROOT: &str = "Root";

/// What [`WindowsInteropStore::location`] calls the real `Root` store.
const LOCATION: &str = "Windows user root store";

/// The confirmation every `Root` change needs, short enough for `doctor`'s fix line.
const APPROVAL: &str = "choose Yes in the Windows security warning";

/// Shown when `certutil.exe` cannot be started from the distro.
const INTEROP_HINT: &str = "Windows interop must be on in this WSL distro \
     ([interop] enabled=true in /etc/wsl.conf)";

/// Why the `windows` target is unavailable on Linux outside WSL.
pub(crate) const NOT_WSL: &str = "it needs WSL or Windows; this Linux is not a WSL distro";

/// Shown when `wslpath` is missing, which only happens outside WSL.
const WSLPATH_HINT: &str = "wslpath comes with WSL; run devlanes inside a WSL distro";

/// `certutil.exe` under WSL and the store it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsInterop {
    /// Windows' `certutil.exe` as Linux sees it, under the System32 mount.
    pub certutil: PathBuf,
    /// The CurrentUser store to change: [`WINDOWS_ROOT`], or a scratch store in tests.
    pub store: String,
}

impl WindowsInterop {
    /// Windows' `certutil.exe` and the user root store, or why this machine has
    /// neither: not WSL, interop off, or no drive mount with a `System32`.
    /// Blocking on the first call (WSL detection).
    pub fn detect() -> Result<Self, String> {
        let Some(wsl) = devlanes_sys::wsl() else {
            return Err(NOT_WSL.into());
        };
        if !wsl.interop {
            return Err(
                "Windows interop is off in this WSL distro; set enabled=true under \
                 [interop] in /etc/wsl.conf, then run wsl.exe --shutdown"
                    .into(),
            );
        }
        let system32 = wsl.system32.as_ref().ok_or_else(|| {
            "no Windows drive mount has Windows\\System32 (is the system drive mounted?)".to_owned()
        })?;
        Ok(Self {
            certutil: system32.join("certutil.exe"),
            store: WINDOWS_ROOT.to_owned(),
        })
    }

    /// Changes the CurrentUser store `store` instead; tests pass a scratch store,
    /// which never prompts.
    #[must_use]
    pub fn with_store(mut self, store: impl Into<String>) -> Self {
        self.store = store.into();
        self
    }
}

/// A change that may show the confirmation dialog.
#[derive(Debug, Clone, Copy)]
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

    /// The title of the dialog Windows shows for it.
    fn dialog(self) -> &'static str {
        match self {
            Self::Add => "\"Security Warning\"",
            Self::Delete => "\"Root Certificate Store\"",
        }
    }
}

/// The Windows user root store, read and changed with `certutil.exe -user`.
///
/// Status reads the user's own store and, for `Root`, the machine's, which an
/// administrator may have trusted the CA in for every user. Changes touch only
/// the user's store. `certutil` reports its errors with the HRESULT name
/// (`ERROR_CANCELLED`, `NTE_NOT_FOUND`) next to a translated message, so only
/// those names and the thumbprint are matched, never the message.
pub struct WindowsInteropStore {
    executor: Arc<dyn CommandExecutor>,
    interop: WindowsInterop,
}

impl WindowsInteropStore {
    /// Runs `interop.certutil` through `executor`.
    pub fn new(executor: Arc<dyn CommandExecutor>, interop: WindowsInterop) -> Self {
        Self { executor, interop }
    }

    /// Only the user's `Root` shows the confirmation dialog.
    fn prompts(&self) -> bool {
        self.interop.store.eq_ignore_ascii_case(WINDOWS_ROOT)
    }

    /// The PowerShell path of the store, for messages.
    fn path(&self) -> String {
        format!(r"Cert:\CurrentUser\{}", self.interop.store)
    }

    /// `certutil.exe` with `args`.
    fn certutil(&self, args: &[&str]) -> Result<CommandLine, TrustError> {
        Ok(CommandLine::new(path_arg(&self.interop.certutil)?).args(args.iter().copied()))
    }

    /// Runs `command`, mapping a `certutil.exe` that cannot start to the interop hint.
    async fn run(&self, command: &CommandLine) -> Result<CommandOutput, TrustError> {
        run(self.executor.as_ref(), command, INTEROP_HINT).await
    }

    /// Whether the user's own store holds `ca`; `Err` carries why it is unknown.
    async fn in_user_store(&self, ca: &CaCertificate) -> Result<Result<bool, String>, TrustError> {
        let thumbprint = ca.sha1_hex();
        let command = self.certutil(&["-user", "-store", &self.interop.store, &thumbprint])?;
        Ok(found(&self.run(&command).await?, &thumbprint))
    }

    /// Whether the machine's `Root` holds `ca`; `false` for other stores.
    async fn in_machine_root(
        &self,
        ca: &CaCertificate,
    ) -> Result<Result<bool, String>, TrustError> {
        if !self.prompts() {
            return Ok(Ok(false));
        }
        let thumbprint = ca.sha1_hex();
        let command = self.certutil(&["-store", WINDOWS_ROOT, &thumbprint])?;
        Ok(found(&self.run(&command).await?, &thumbprint))
    }

    /// The Windows path of a Linux file, from `wslpath -w`.
    async fn windows_path(&self, linux: &str) -> Result<String, TrustError> {
        let wslpath = find_system_program(self.executor.as_ref(), "wslpath").ok_or_else(|| {
            TrustError::ToolMissing {
                tool: "wslpath".into(),
                hint: WSLPATH_HINT.into(),
            }
        })?;
        let command = CommandLine::new(path_arg(&wslpath)?).args(["-w", linux]);
        let output = run(self.executor.as_ref(), &command, WSLPATH_HINT)
            .await?
            .into_checked(&command)?;
        let path = output.stdout.trim();
        if path.is_empty() {
            return Err(TrustError::CommandFailed {
                command: command.to_string(),
                code: output.code,
                stderr: "printed no path".into(),
            });
        }
        Ok(path.to_owned())
    }

    /// Requires success of a store change, naming a declined dialog.
    fn changed(
        &self,
        change: Change,
        command: &CommandLine,
        output: CommandOutput,
    ) -> Result<(), TrustError> {
        if output.success() {
            return Ok(());
        }
        if cancelled(&output) {
            let action = match change {
                Change::Add => format!("adding the CA to {}", self.path()),
                Change::Delete => format!("removing the CA from {}", self.path()),
            };
            return Err(TrustError::Cancelled {
                detail: format!(
                    "{action} was declined in the Windows {} dialog; run `{}` again and \
                     choose Yes",
                    change.dialog(),
                    change.command()
                ),
            });
        }
        // certutil writes its errors to stdout.
        Err(TrustError::CommandFailed {
            command: command.to_string(),
            code: output.code,
            stderr: format!("{}{}", output.stdout, output.stderr),
        })
    }
}

#[async_trait]
impl TrustStore for WindowsInteropStore {
    /// Always [`TrustTarget::Windows`].
    fn target(&self) -> TrustTarget {
        TrustTarget::Windows
    }

    /// [`WINDOWS_ID`].
    fn id(&self) -> &'static str {
        WINDOWS_ID
    }

    /// Names the store, the tool and, for `Root`, the dialog.
    fn describe(&self) -> String {
        let how = "changed with certutil.exe through WSL interop";
        if self.prompts() {
            format!(
                "{LOCATION} ({}, {how}; Windows shows a security warning to confirm each change)",
                self.path()
            )
        } else {
            format!("{} ({how})", self.path())
        }
    }

    /// [`LOCATION`] for `Root`, the store path otherwise.
    fn location(&self) -> Option<String> {
        Some(if self.prompts() {
            LOCATION.to_owned()
        } else {
            self.path()
        })
    }

    /// Choosing Yes in the security warning, for `Root` only.
    fn approval(&self) -> Option<String> {
        self.prompts().then(|| APPROVAL.to_owned())
    }

    /// The dialog `install` or `uninstall` is about to show: none when the CA is
    /// already trusted, or when the user's store has nothing to delete.
    async fn pending_approval(&self, ca: &CaCertificate, install: bool) -> Option<String> {
        if !self.prompts() {
            return None;
        }
        if install {
            if let Ok(TrustStatus::Installed) = self.status(ca).await {
                return None;
            }
            return Some(format!(
                "Windows asks you to confirm adding the CA to your user root store in a {} \
                 dialog; choose Yes",
                Change::Add.dialog()
            ));
        }
        if let Ok(Ok(false)) = self.in_user_store(ca).await {
            return None;
        }
        Some(format!(
            "Windows asks you to confirm removing the CA from your user root store in a {} \
             dialog; choose Yes",
            Change::Delete.dialog()
        ))
    }

    /// Adds the CA to the user's store unless a root store already trusts it.
    /// `certutil` reads the PEM from a temp file through its `\\wsl.localhost`
    /// path. A store other than `Root` may not exist yet, and `certutil` only
    /// creates one with `-f`.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        if let Ok(TrustStatus::Installed) = self.status(ca).await {
            return Ok(());
        }
        let pem = TempPem::create(ca).await?;
        let file = self.windows_path(pem.arg()).await?;
        let mut args = Vec::new();
        if !self.prompts() {
            args.push("-f");
        }
        args.extend(["-user", "-addstore", &self.interop.store, &file]);
        let command = self.certutil(&args)?;
        let output = self.run(&command).await;
        pem.close().await;
        self.changed(Change::Add, &command, output?)
    }

    /// Deletes the CA from the user's store; absent is success. A copy in the
    /// machine store stays, as only an administrator can remove it.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        if let Ok(false) = self.in_user_store(ca).await? {
            return Ok(());
        }
        let thumbprint = ca.sha1_hex();
        let command = self.certutil(&["-user", "-delstore", &self.interop.store, &thumbprint])?;
        let output = self.run(&command).await?;
        self.changed(Change::Delete, &command, output)
    }

    /// Installed when the user's store or, for `Root`, the machine's holds the
    /// CA; unknown when `certutil.exe` cannot run or fails otherwise.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        let unknown = |reason: String| Ok(TrustStatus::Unknown { reason });
        match self.in_user_store(ca).await {
            Ok(Ok(true)) => return Ok(TrustStatus::Installed),
            Ok(Ok(false)) => {}
            Ok(Err(reason)) => return unknown(reason),
            Err(error) => return unknown(error.to_string()),
        }
        match self.in_machine_root(ca).await {
            Ok(Ok(true)) => Ok(TrustStatus::Installed),
            Ok(Ok(false)) => Ok(TrustStatus::Missing),
            Ok(Err(reason)) => unknown(reason),
            Err(error) => unknown(error.to_string()),
        }
    }
}

/// Reads a `certutil -store <store> <thumbprint>` answer: found when it succeeds
/// and lists the thumbprint (a bare number would also match a certificate by
/// index), absent when the store or certificate is not found (`ERROR_FILE_NOT_FOUND`,
/// `NTE_NOT_FOUND`, `CRYPT_E_NOT_FOUND`), else the output as the reason.
fn found(output: &CommandOutput, thumbprint: &str) -> Result<bool, String> {
    let text = format!("{}{}", output.stdout, output.stderr).to_ascii_lowercase();
    if output.success() {
        return Ok(text.contains(&thumbprint.to_ascii_lowercase()));
    }
    if text.contains("not_found") {
        return Ok(false);
    }
    let detail = text
        .lines()
        .map(str::trim)
        .rfind(|line| line.contains("failed"))
        .unwrap_or("no output")
        .to_owned();
    Err(format!(
        "certutil.exe failed with exit code {}: {detail}",
        output
            .code
            .map_or_else(|| "none".to_owned(), |code| code.to_string())
    ))
}

/// Whether the user said No in the dialog: `ERROR_CANCELLED` (`0x800704c7`).
fn cancelled(output: &CommandOutput) -> bool {
    let text = format!("{}{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("error_cancelled") || text.contains("0x800704c7")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "24cf0e1cdba43289dff23abc760169e23b12925c";

    /// Real certutil answers, from Windows 11 26300 through WSL 2.7.
    #[test]
    fn reads_store_lookups() {
        let listed = CommandOutput::ok(format!(
            "devlanes-test\n================ Certificate 0 ================\nCert Hash(sha1): {HASH}\nCertUtil: -store command completed successfully.\n"
        ));
        assert_eq!(found(&listed, &HASH.to_uppercase()), Ok(true));
        // A digits-only id would select a certificate by index instead.
        let other = CommandOutput::ok(
            "Cert Hash(sha1): 00ff\nCertUtil: -store command completed successfully.\n",
        );
        assert_eq!(found(&other, HASH), Ok(false));
        let missing_cert = CommandOutput {
            code: Some(17),
            stdout: "CertUtil: -store command FAILED: 0x80090011 (-2146893807 NTE_NOT_FOUND)\nCertUtil: Object was not found.\n".into(),
            stderr: String::new(),
        };
        assert_eq!(found(&missing_cert, HASH), Ok(false));
        let missing_store = CommandOutput {
            code: Some(2),
            stdout: "CertUtil: -store command FAILED: 0x80070002 (WIN32: 2 ERROR_FILE_NOT_FOUND)\n"
                .into(),
            stderr: String::new(),
        };
        assert_eq!(found(&missing_store, HASH), Ok(false));
        let denied = CommandOutput {
            code: Some(5),
            stdout: "CertUtil: -store command FAILED: 0x80070005 (WIN32: 5 ERROR_ACCESS_DENIED)\n"
                .into(),
            stderr: String::new(),
        };
        let reason = found(&denied, HASH).expect_err("unknown");
        assert!(reason.contains("error_access_denied"), "{reason}");
        assert!(reason.contains("exit code 5"), "{reason}");
    }

    #[test]
    fn recognises_a_declined_dialog() {
        let declined = CommandOutput {
            code: Some(199),
            stdout:
                "CertUtil: -addstore command FAILED: 0x800704c7 (WIN32: 1223 ERROR_CANCELLED)\n"
                    .into(),
            stderr: String::new(),
        };
        assert!(cancelled(&declined));
        assert!(!cancelled(&CommandOutput::failed(
            2,
            "ERROR_FILE_NOT_FOUND"
        )));
    }
}
