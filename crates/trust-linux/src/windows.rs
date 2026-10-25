//! The `windows` target inside WSL: the Windows user root store, read and
//! changed with Windows' own `certutil.exe` through WSL interop, so browsers on
//! Windows (Edge, Chrome, Firefox reading the Windows roots) trust the CA of a
//! daemon in the distro.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use devlanes_trust::{
    CaCertificate, CommandExecutor, CommandLine, CommandOutput, TempPem, TrustError, TrustStatus,
    TrustStore, TrustTarget, find_system_program, path_arg, run,
};

/// Store id of the Windows user root store.
pub const WINDOWS_ID: &str = "windows-user-root";

/// The CurrentUser store Windows browsers read trusted roots from.
const ROOT: &str = "Root";

/// Shown when `certutil.exe` cannot be started from the distro.
const INTEROP_HINT: &str = "Windows programs must start from this WSL distro \
     ([interop] enabled=true in /etc/wsl.conf)";

/// Shown when `wslpath` is missing, which only happens outside WSL.
const WSLPATH_HINT: &str = "wslpath comes with WSL; run devlanes inside a WSL distro";

/// How long a store lookup may take before `status` gives up. Interop starts
/// a Windows process for each call; `doctor` waits on this.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Windows' `certutil.exe` as Linux sees it, or why this machine cannot reach
/// it: not WSL, or interop off. Found once per process, since async callers
/// such as `doctor` ask for the store and finding it reads the mount table
/// and stats the Windows drives; blocking on that first call.
pub(crate) fn certutil() -> Result<PathBuf, String> {
    static CERTUTIL: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    CERTUTIL.get_or_init(find_certutil).clone()
}

/// [`certutil`] without the cache.
fn find_certutil() -> Result<PathBuf, String> {
    match devlanes_sys::wsl() {
        None => Err("this Linux is not a WSL distro".into()),
        Some(wsl) if !wsl.interop => Err("Windows interop is off in this WSL distro; set \
             enabled=true under [interop] in /etc/wsl.conf, then run wsl.exe --shutdown from Windows"
            .into()),
        Some(_) => Ok(devlanes_sys::windows_system32().join("certutil.exe")),
    }
}

/// The Windows user root store (`Cert:\CurrentUser\Root`). Windows shows a
/// security warning on the desktop for each add and delete; lookups show none.
pub struct WindowsStore {
    executor: Arc<dyn CommandExecutor>,
    certutil: PathBuf,
}

impl WindowsStore {
    /// Runs the `certutil.exe` at `certutil` through `executor`.
    pub fn new(executor: Arc<dyn CommandExecutor>, certutil: impl Into<PathBuf>) -> Self {
        Self {
            executor,
            certutil: certutil.into(),
        }
    }

    /// `certutil.exe -user` with `args`.
    fn command(&self, args: &[&str]) -> Result<CommandLine, TrustError> {
        Ok(CommandLine::new(path_arg(&self.certutil)?)
            .arg("-user")
            .args(args.iter().copied()))
    }

    /// Whether the user's root store holds `ca`, from `-verifystore Root
    /// <thumbprint>`. Found only when the answer lists the thumbprint: a
    /// digits-only id would select a certificate by index, and an empty store
    /// answers such an id with success.
    async fn lookup(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        let thumbprint = ca.sha1_hex();
        let command = self.command(&["-verifystore", ROOT, &thumbprint])?;
        let output = tokio::time::timeout(
            LOOKUP_TIMEOUT,
            run(self.executor.as_ref(), &command, INTEROP_HINT),
        )
        .await
        .map_err(|_| {
            TrustError::io(
                format!("running `{command}`"),
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "Windows did not answer within {}s",
                        LOOKUP_TIMEOUT.as_secs()
                    ),
                ),
            )
        })??;
        match lookup_status(&output, &thumbprint) {
            Some(status) => Ok(status),
            None => Err(failed(&command, output)),
        }
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
        match output.stdout.trim() {
            "" => Err(TrustError::CommandFailed {
                command: command.to_string(),
                code: output.code,
                stderr: "printed no path".into(),
            }),
            path => Ok(path.to_owned()),
        }
    }

    /// Runs a change and requires success; a No in the Windows dialog fails
    /// like any other error, with certutil's `ERROR_CANCELLED` in the message.
    async fn change(&self, command: CommandLine) -> Result<(), TrustError> {
        let output = run(self.executor.as_ref(), &command, INTEROP_HINT).await?;
        if output.success() {
            Ok(())
        } else {
            Err(failed(&command, output))
        }
    }
}

#[async_trait]
impl TrustStore for WindowsStore {
    /// Always [`TrustTarget::Windows`].
    fn target(&self) -> TrustTarget {
        TrustTarget::Windows
    }

    /// [`WINDOWS_ID`].
    fn id(&self) -> &'static str {
        WINDOWS_ID
    }

    /// Names the store, the tool and the dialog.
    fn describe(&self) -> String {
        "Windows user root store (Cert:\\CurrentUser\\Root), changed with certutil.exe through \
         WSL interop; Windows asks to confirm each change"
            .into()
    }

    /// Adds the CA unless the store holds it. `certutil` reads the PEM from a
    /// temp file through its `\\wsl.localhost` path.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        if self.lookup(ca).await? == TrustStatus::Installed {
            return Ok(());
        }
        let pem = TempPem::create(ca).await?;
        let file = self.windows_path(pem.arg()).await?;
        let result = self
            .change(self.command(&["-addstore", ROOT, &file])?)
            .await;
        pem.close().await;
        result
    }

    /// Deletes the CA by thumbprint; absent is success.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        if self.lookup(ca).await? == TrustStatus::Missing {
            return Ok(());
        }
        let thumbprint = ca.sha1_hex();
        self.change(self.command(&["-delstore", ROOT, &thumbprint])?)
            .await
    }

    /// Installed when the user's root store lists the CA; unknown when
    /// `certutil.exe` cannot run, times out or fails otherwise.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        Ok(self
            .lookup(ca)
            .await
            .unwrap_or_else(|error| TrustStatus::Unknown {
                reason: error.to_string(),
            }))
    }
}

/// Reads a `-verifystore` answer: installed when it succeeds and lists the
/// thumbprint, missing when it succeeds without it or the certificate is not
/// found (`NTE_NOT_FOUND`, `CRYPT_E_NOT_FOUND`). certutil prints error names
/// next to a translated message, so only the names are matched. `None` for
/// any other failure.
fn lookup_status(output: &CommandOutput, thumbprint: &str) -> Option<TrustStatus> {
    let text = format!("{}{}", output.stdout, output.stderr).to_ascii_lowercase();
    if output.success() {
        return Some(if text.contains(&thumbprint.to_ascii_lowercase()) {
            TrustStatus::Installed
        } else {
            TrustStatus::Missing
        });
    }
    text.contains("not_found").then_some(TrustStatus::Missing)
}

/// A failed certutil call; certutil writes its errors to stdout.
fn failed(command: &CommandLine, output: CommandOutput) -> TrustError {
    TrustError::CommandFailed {
        command: command.to_string(),
        code: output.code,
        stderr: format!("{}{}", output.stdout, output.stderr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "FBD3B2C0991549EC5FB12FA1A994D2E0C32D8057";

    /// Real certutil answers, from Windows 11 26300 through WSL 2.
    #[test]
    fn reads_lookups() {
        let listed = CommandOutput::ok(
            "Root \"Trusted Root Certification Authorities\"\n================ Certificate 0 ================\nCert Hash(sha1): fbd3b2c0991549ec5fb12fa1a994d2e0c32d8057\nCertUtil: -verifystore command completed successfully.\n",
        );
        assert_eq!(lookup_status(&listed, HASH), Some(TrustStatus::Installed));
        let empty = CommandOutput::ok(
            "Root \"Trusted Root Certification Authorities\"\nCertUtil: -verifystore command completed successfully.\n",
        );
        assert_eq!(lookup_status(&empty, HASH), Some(TrustStatus::Missing));
        let missing = CommandOutput {
            code: Some(17),
            stdout: "Root \"Trusted Root Certification Authorities\"\nCertUtil: -verifystore command FAILED: 0x80090011 (-2146893807 NTE_NOT_FOUND)\nCertUtil: Object was not found.\n".into(),
            stderr: String::new(),
        };
        assert_eq!(lookup_status(&missing, HASH), Some(TrustStatus::Missing));
        let denied = CommandOutput {
            code: Some(5),
            stdout:
                "CertUtil: -verifystore command FAILED: 0x80070005 (WIN32: 5 ERROR_ACCESS_DENIED)\n"
                    .into(),
            stderr: String::new(),
        };
        assert_eq!(lookup_status(&denied, HASH), None);
    }
}
