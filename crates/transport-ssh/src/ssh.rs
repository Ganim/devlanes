//! System OpenSSH as the pipe.
//!
//! Unix keeps one ControlMaster per box and opens one channel per session over
//! it. Win32-OpenSSH has no ControlMaster, so on Windows every session is one
//! `ssh` process that probes the box and then becomes the tunnel.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use devlanes_transport::{Joined, TransportError};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::oneshot;

use crate::askpass::AskpassConfig;
use crate::bootstrap::{
    self, BootstrapPlan, BootstrapPolicy, ManifestError, ReleaseManifest, RemotePlatform,
};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::SshTransport;
#[cfg(windows)]
pub use windows::SshTransport;

/// Bytes of remote stderr kept for error messages.
const STDERR_TAIL: usize = 4096;

/// Settings for [`SshTransport`].
#[derive(Clone)]
pub struct SshOptions {
    /// Directory for control sockets, the master log and askpass files (made
    /// 0700). Unused on Windows, where nothing is written to disk.
    pub control_dir: PathBuf,
    /// Local `ssh` binary. On Windows the bare name `ssh` means the system
    /// OpenSSH client (`%SystemRoot%\System32\OpenSSH\ssh.exe`), else the first
    /// `ssh.exe` on `PATH`.
    pub ssh_program: PathBuf,
    /// Remote command name, looked up on PATH and then in `$HOME/.devlanes/bin`.
    pub remote_program: String,
    /// TCP connect limit passed as `ConnectTimeout`.
    pub connect_timeout: Duration,
    /// Limit for authenticating, including time spent answering prompts.
    pub auth_timeout: Duration,
    /// Extra `-o` options for ssh, such as `Port=2222`.
    pub ssh_options: Vec<String>,
    /// Prompt relay; without it ssh runs with `BatchMode=yes`.
    pub askpass: Option<AskpassConfig>,
    /// Releases the client may install on a box that lacks devlanes.
    pub manifest: Option<ReleaseManifest>,
    /// HTTPS URL of the release manifest, fetched when a box lacks devlanes and
    /// no `manifest` was given.
    pub manifest_url: Option<String>,
    /// Release version the fetched manifest must describe: the client's own.
    pub release_version: String,
    /// Whether a missing binary is reported or installed.
    pub bootstrap: BootstrapPolicy,
}

impl SshOptions {
    /// Defaults with control files under `<devlanes_home>/run/ssh`.
    pub fn new(devlanes_home: impl AsRef<Path>) -> Self {
        Self {
            control_dir: devlanes_home.as_ref().join("run").join("ssh"),
            ssh_program: PathBuf::from("ssh"),
            remote_program: "devlanes".to_owned(),
            connect_timeout: Duration::from_secs(15),
            auth_timeout: Duration::from_secs(120),
            ssh_options: Vec::new(),
            askpass: None,
            manifest: None,
            manifest_url: None,
            release_version: env!("CARGO_PKG_VERSION").to_owned(),
            bootstrap: BootstrapPolicy::Report,
        }
    }
}

/// SSH transport failures.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// Local filesystem or process I/O failed.
    #[error("ssh I/O: {0}")]
    Io(#[from] io::Error),
    /// The control socket path would not fit in `sun_path`.
    #[cfg(unix)]
    #[error("ssh control path {0} is too long; use a shorter control directory")]
    ControlPathTooLong(PathBuf),
    /// No OpenSSH client was found.
    #[cfg(windows)]
    #[error(
        "no ssh client found ({0}); add the Windows optional feature \"OpenSSH Client\" or put ssh.exe on PATH"
    )]
    ProgramMissing(PathBuf),
    /// ssh did not connect or authenticate; `log` holds ssh's own diagnostics.
    #[error("ssh to {destination} failed ({status}): {log}")]
    Master {
        /// The ssh destination.
        destination: String,
        /// Exit status description.
        status: String,
        /// Tail of ssh's log.
        log: String,
        /// Whether retrying cannot help, such as rejected credentials.
        permanent: bool,
    },
    /// ssh did not finish authenticating in time.
    #[error("ssh to {0} did not authenticate in time")]
    AuthTimeout(String),
    /// A command over the master failed.
    #[cfg(unix)]
    #[error("ssh command: {0}")]
    Ssh(#[from] openssh::Error),
    /// The box answered the probe with unexpected output.
    #[error("unexpected probe output from the box: {0:?}")]
    Probe(String),
    /// devlanes is missing on the box; the plan says what installing would do.
    #[error("{}", .0.instructions())]
    BootstrapRequired(Box<BootstrapPlan>),
    /// The release manifest is unusable.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// Installation was refused because a binary already exists.
    #[error("devlanes already exists on the box; bootstrap never replaces an installed binary")]
    AlreadyInstalled,
    /// The install script failed on the box.
    #[error("installing devlanes on the box failed: {0}")]
    Install(String),
}

impl SshError {
    /// Reports whether reconnecting without user action is pointless.
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Master { permanent, .. } => *permanent,
            #[cfg(unix)]
            Self::ControlPathTooLong(_) => true,
            #[cfg(windows)]
            Self::ProgramMissing(_) => true,
            Self::BootstrapRequired(_)
            | Self::Manifest(_)
            | Self::AlreadyInstalled
            | Self::Install(_) => true,
            #[cfg(unix)]
            Self::Ssh(_) => false,
            Self::Io(_) | Self::AuthTimeout(_) | Self::Probe(_) => false,
        }
    }
}

impl From<SshError> for TransportError {
    fn from(error: SshError) -> Self {
        if error.is_permanent() {
            TransportError::permanent(error)
        } else {
            TransportError::transient(error)
        }
    }
}

/// The release manifest: the configured one, else the one at `manifest_url`.
async fn release_manifest(options: &SshOptions) -> Result<Option<ReleaseManifest>, ManifestError> {
    if let Some(manifest) = &options.manifest {
        return Ok(Some(manifest.clone()));
    }
    match &options.manifest_url {
        Some(url) => bootstrap::fetch_manifest(url, &options.release_version)
            .await
            .map(Some),
        None => Ok(None),
    }
}

/// What installing devlanes on a box of `platform` would take.
async fn bootstrap_plan(
    destination: &str,
    options: &SshOptions,
    platform: RemotePlatform,
) -> BootstrapPlan {
    let (manifest, manifest_error) = match release_manifest(options).await {
        Ok(manifest) => (manifest, None),
        Err(error) => (None, Some(error.0)),
    };
    BootstrapPlan {
        destination: destination.to_owned(),
        artifact: manifest.as_ref().and_then(|m| m.select(&platform)).cloned(),
        platform,
        manifest_error,
    }
}

/// ssh's wording for rejected credentials. The Unix master log holds only
/// ssh's own lines.
#[cfg(unix)]
const DENIED: &str = "Permission denied";

/// ssh's wording for rejected credentials, as in `user@box: Permission denied
/// (publickey).`. On Windows the log is ssh's stderr, which also carries the
/// box's, where a bare "Permission denied" names a file.
#[cfg(windows)]
const DENIED: &str = "Permission denied (";

/// Classifies a failed connection from ssh's log.
///
/// A name that does not resolve is permanent only before the first successful
/// connection (a typo); afterwards it is a network change that will pass. The
/// other patterns are ssh's own messages; on Windows a box would print them
/// only from an ssh it runs itself, and runs in which the box wrote to stdout
/// do not come here.
fn master_error(destination: &str, status: impl fmt::Display, log: &str, first: bool) -> SshError {
    // These need new credentials or known_hosts changes, not time.
    const PERMANENT: [&str; 3] = [
        DENIED,
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
    ];
    let log = log.trim();
    let permanent = PERMANENT.iter().any(|needle| log.contains(needle))
        || (first && log.contains("Could not resolve hostname"));
    SshError::Master {
        destination: destination.to_owned(),
        status: status.to_string(),
        permanent,
        log: tail(log.as_bytes()),
    }
}

/// Returns the last [`STDERR_TAIL`] bytes as lossy UTF-8.
fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

/// How the remote tunnel process ended.
struct ExitInfo {
    status: Result<ExitStatus, String>,
    stderr: String,
}

/// Reading half of a tunnel's stdio.
type TunnelReader = Box<dyn AsyncRead + Send + Unpin>;

/// Writing half of a tunnel's stdio.
type TunnelWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// The tunnel process's stdio as one duplex.
///
/// At EOF it waits for the remote exit status, so a crashed or missing
/// `tunnel` command surfaces as an error carrying its stderr instead of a
/// bare disconnect.
pub struct SshStream {
    io: Joined<TunnelReader, TunnelWriter>,
    exit: Option<oneshot::Receiver<ExitInfo>>,
    finished: Option<Result<(), String>>,
    _stop: oneshot::Sender<()>,
}

impl SshStream {
    /// Joins a tunnel's stdout and stdin. `exit` delivers how the tunnel ended;
    /// dropping the stream drops `stop`, which tells the owner of the process to
    /// end it.
    fn new(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        exit: oneshot::Receiver<ExitInfo>,
        stop: oneshot::Sender<()>,
    ) -> Self {
        Self {
            io: Joined::new(Box::new(reader), Box::new(writer)),
            exit: Some(exit),
            finished: None,
            _stop: stop,
        }
    }

    /// Resolves EOF into success or the remote failure.
    fn poll_eof(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.finished.is_none() {
            let outcome = match self.exit.as_mut() {
                Some(exit) => match ready!(Pin::new(exit).poll(cx)) {
                    Ok(info) => describe_exit(info),
                    Err(_) => Ok(()),
                },
                None => Ok(()),
            };
            self.exit = None;
            self.finished = Some(outcome);
        }
        match &self.finished {
            Some(Err(message)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                message.clone(),
            ))),
            _ => Poll::Ready(Ok(())),
        }
    }
}

/// Turns a tunnel exit into `Ok` for a clean exit or a message otherwise.
fn describe_exit(info: ExitInfo) -> Result<(), String> {
    let detail = if info.stderr.is_empty() {
        String::new()
    } else {
        format!(": {}", info.stderr)
    };
    match info.status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "remote `devlanes tunnel --stdio` exited with {status}{detail}"
        )),
        Err(error) => Err(format!(
            "remote `devlanes tunnel --stdio` failed: {error}{detail}"
        )),
    }
}

impl AsyncRead for SshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.finished.is_some() {
            return this.poll_eof(cx);
        }
        let before = buf.filled().len();
        ready!(Pin::new(&mut this.io).poll_read(cx, buf))?;
        if buf.filled().len() == before && buf.remaining() > 0 {
            return this.poll_eof(cx);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}
