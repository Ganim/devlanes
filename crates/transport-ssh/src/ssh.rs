//! System OpenSSH as the pipe: one ControlMaster per box, one channel per session.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_trait::async_trait;
use devlanes_transport::{BoxDuplex, Joined, Transport, TransportError};
use openssh::{ChildStdin, ChildStdout, Session, Stdio};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::sync::{Mutex, oneshot};

use crate::askpass::{AskpassConfig, AskpassServer};
use crate::bootstrap::{
    self, BootstrapPlan, BootstrapPolicy, EXIT_ALREADY_INSTALLED, ManifestError, ReleaseManifest,
    RemoteProbe,
};

/// macOS `sun_path` holds 104 bytes including the terminator; leave margin for ssh's suffixes.
const MAX_CONTROL_PATH: usize = 100;

/// Bytes of remote stderr kept for error messages.
const STDERR_TAIL: usize = 4096;

/// Distinguishes masters started by one process.
static NEXT_MASTER: AtomicU64 = AtomicU64::new(0);

/// Settings for [`SshTransport`].
#[derive(Clone)]
pub struct SshOptions {
    /// Directory for control sockets, the master log and askpass files (made 0700).
    pub control_dir: PathBuf,
    /// Local `ssh` binary.
    pub ssh_program: PathBuf,
    /// Remote command name, looked up on PATH and then in `$HOME/.devlanes/bin`.
    pub remote_program: String,
    /// TCP connect limit passed as `ConnectTimeout`.
    pub connect_timeout: Duration,
    /// Limit for the whole master launch, including time spent answering prompts.
    pub auth_timeout: Duration,
    /// Extra `-o` options for the master, such as `Port=2222`.
    pub ssh_options: Vec<String>,
    /// Prompt relay; without it the master runs with `BatchMode=yes`.
    pub askpass: Option<AskpassConfig>,
    /// Releases the client may install on a box that lacks devlanes.
    pub manifest: Option<ReleaseManifest>,
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
    #[error("ssh control path {0} is too long; use a shorter control directory")]
    ControlPathTooLong(PathBuf),
    /// The master did not start; `log` holds ssh's own diagnostics.
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
    /// The master did not finish authenticating in time.
    #[error("ssh to {0} did not authenticate in time")]
    AuthTimeout(String),
    /// A command over the master failed.
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
            Self::ControlPathTooLong(_)
            | Self::BootstrapRequired(_)
            | Self::Manifest(_)
            | Self::AlreadyInstalled
            | Self::Install(_) => true,
            Self::Io(_) | Self::AuthTimeout(_) | Self::Ssh(_) | Self::Probe(_) => false,
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

/// A running ControlMaster and what the box reported over it.
struct Master {
    session: Arc<Session>,
    ctl: PathBuf,
    log: PathBuf,
    ssh_program: PathBuf,
    probe: Option<RemoteProbe>,
}

impl Drop for Master {
    fn drop(&mut self) {
        // Resumed sessions do not stop the master on drop, so ask it to exit.
        // Spawned without waiting; tokio reaps the child in the background.
        if tokio::runtime::Handle::try_current().is_ok() {
            let _ = tokio::process::Command::new(&self.ssh_program)
                .arg("-S")
                .arg(&self.ctl)
                .args(["-O", "exit", "--", "none"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        let log = self.log.clone();
        // Unlinking one file is cheap; doing it inline keeps cleanup working without a runtime.
        let _ = std::fs::remove_file(log);
    }
}

/// [`Transport`] that runs `devlanes tunnel --stdio` on a box over system OpenSSH.
///
/// The user's `~/.ssh/config`, agent, ProxyJump and certificates apply because
/// the real `ssh` binary does the work.
pub struct SshTransport {
    destination: String,
    options: SshOptions,
    master: Mutex<Option<Master>>,
}

impl SshTransport {
    /// Targets `destination`, any name `ssh` accepts.
    pub fn new(destination: impl Into<String>, options: SshOptions) -> Self {
        Self {
            destination: destination.into(),
            options,
            master: Mutex::new(None),
        }
    }

    /// The ssh destination.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// Reports the box platform and devlanes location, starting a master if needed.
    pub async fn probe(&self) -> Result<RemoteProbe, SshError> {
        let mut master = self.master.lock().await;
        let master = self.ensure_master(&mut master).await?;
        self.ensure_probe(master).await.cloned()
    }

    /// Installs devlanes on the box as described by `plan`.
    ///
    /// Refuses when a binary exists already; restarting a box daemon is left to the user.
    pub async fn bootstrap(&self, plan: &BootstrapPlan) -> Result<(), SshError> {
        let artifact = plan
            .artifact
            .as_ref()
            .ok_or_else(|| SshError::BootstrapRequired(Box::new(plan.clone())))?;
        artifact.validate()?;
        let script = bootstrap::install_script(artifact)?;
        let mut master = self.master.lock().await;
        let master = self.ensure_master(&mut master).await?;
        let output = master
            .session
            .command("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .output()
            .await?;
        // A new probe must find the installed binary.
        master.probe = None;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(EXIT_ALREADY_INSTALLED) => Err(SshError::AlreadyInstalled),
            _ => Err(SshError::Install(tail(&output.stderr))),
        }
    }

    /// Stops the master, ending every session over it.
    pub async fn close(&self) {
        self.master.lock().await.take();
    }

    /// Opens the tunnel, starting or reusing the master and bootstrapping if allowed.
    async fn open(&self) -> Result<SshStream, SshError> {
        let mut guard = self.master.lock().await;
        let master = self.ensure_master(&mut guard).await?;
        let probe = self.ensure_probe(master).await?.clone();
        if let Some(program) = probe.program {
            return spawn_tunnel(master.session.clone(), &program).await;
        }
        let plan = BootstrapPlan {
            destination: self.destination.clone(),
            artifact: self
                .options
                .manifest
                .as_ref()
                .and_then(|m| m.select(&probe.platform))
                .cloned(),
            platform: probe.platform,
        };
        if self.options.bootstrap == BootstrapPolicy::Report || plan.artifact.is_none() {
            return Err(SshError::BootstrapRequired(Box::new(plan)));
        }
        // `bootstrap` takes the master lock itself.
        drop(guard);
        self.bootstrap(&plan).await?;
        let mut guard = self.master.lock().await;
        let master = self.ensure_master(&mut guard).await?;
        let program = self
            .ensure_probe(master)
            .await?
            .program
            .clone()
            .ok_or_else(|| SshError::Install("binary not found after install".into()))?;
        spawn_tunnel(master.session.clone(), &program).await
    }

    /// Returns a live master, replacing a dead one.
    async fn ensure_master<'a>(
        &self,
        slot: &'a mut Option<Master>,
    ) -> Result<&'a mut Master, SshError> {
        let alive = match slot.as_ref() {
            Some(master) => master.session.check().await.is_ok(),
            None => false,
        };
        if !alive {
            *slot = None;
            *slot = Some(self.launch_master().await?);
        }
        slot.as_mut()
            .ok_or_else(|| SshError::Io(io::Error::other("master slot empty after launch")))
    }

    /// Runs the probe once per master.
    async fn ensure_probe<'a>(&self, master: &'a mut Master) -> Result<&'a RemoteProbe, SshError> {
        if master.probe.is_none() {
            let output = master
                .session
                .command("sh")
                .arg("-c")
                .arg(bootstrap::probe_script(&self.options.remote_program))
                .stdin(Stdio::null())
                .output()
                .await?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let probe = bootstrap::parse_probe(&stdout)
                .ok_or_else(|| SshError::Probe(stdout.into_owned()))?;
            master.probe = Some(probe);
        }
        master
            .probe
            .as_ref()
            .ok_or_else(|| SshError::Probe(String::new()))
    }

    /// Starts `ssh -M -f -N` with askpass routing and resumes it as an openssh session.
    ///
    /// openssh's own builder forces `BatchMode=yes` and cannot set the master's
    /// environment, which would rule out askpass, so the master is launched here.
    async fn launch_master(&self) -> Result<Master, SshError> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            NEXT_MASTER.fetch_add(1, Ordering::Relaxed)
        );
        let ctl = self.options.control_dir.join(format!("m-{id}"));
        if ctl.as_os_str().len() > MAX_CONTROL_PATH {
            return Err(SshError::ControlPathTooLong(ctl));
        }
        let dir = self.options.control_dir.clone();
        tokio::task::spawn_blocking(move || private_dir(&dir))
            .await
            .map_err(io::Error::other)??;
        let name = format!("m-{id}");
        let log = self.options.control_dir.join(format!("{name}.log"));
        let mut command = tokio::process::Command::new(&self.options.ssh_program);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .arg("-E")
            .arg(&log)
            .arg("-S")
            .arg(&ctl)
            .args(["-M", "-f", "-N"])
            .args(["-o", "ControlPersist=yes"])
            .args(["-o", "ServerAliveInterval=15"])
            .args(["-o", "ServerAliveCountMax=3"])
            .arg("-o")
            .arg(format!(
                "ConnectTimeout={}",
                self.options.connect_timeout.as_secs().max(1)
            ));
        for option in &self.options.ssh_options {
            command.arg("-o").arg(option);
        }
        let askpass = match &self.options.askpass {
            Some(config) => {
                let server = AskpassServer::start(
                    &self.options.control_dir,
                    &format!("a-{id}"),
                    config,
                    &self.destination,
                )
                .await?;
                for (key, value) in server.env() {
                    command.env(key, value);
                }
                Some(server)
            }
            None => {
                command.args(["-o", "BatchMode=yes"]);
                None
            }
        };
        // `--` keeps a destination starting with `-` from being parsed as an option.
        command.arg("--").arg(&self.destination);
        // With -f, ssh forks into the background once authenticated, so this
        // returns when the master is usable or authentication failed.
        let status = tokio::time::timeout(self.options.auth_timeout, command.status())
            .await
            .map_err(|_| SshError::AuthTimeout(self.destination.clone()))??;
        drop(askpass);
        if !status.success() {
            let log_path = log.clone();
            let text = tokio::task::spawn_blocking(move || {
                let text = std::fs::read_to_string(&log_path).unwrap_or_default();
                let _ = std::fs::remove_file(&log_path);
                text
            })
            .await
            .map_err(io::Error::other)?;
            return Err(master_error(&self.destination, status, &text));
        }
        let session = Session::resume(
            ctl.clone().into_boxed_path(),
            Some(log.clone().into_boxed_path()),
        );
        Ok(Master {
            session: Arc::new(session),
            ctl,
            log,
            ssh_program: self.options.ssh_program.clone(),
            probe: None,
        })
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn connect(&self) -> Result<BoxDuplex, TransportError> {
        Ok(Box::new(self.open().await?))
    }
}

/// Creates `dir` with mode 0700 so sockets and askpass files stay private.
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Classifies a failed master launch from ssh's log.
fn master_error(destination: &str, status: ExitStatus, log: &str) -> SshError {
    // These need new credentials or known_hosts changes, not time.
    const PERMANENT: [&str; 4] = [
        "Permission denied",
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        "Could not resolve hostname",
    ];
    let log = log.trim();
    SshError::Master {
        destination: destination.to_owned(),
        status: status.to_string(),
        permanent: PERMANENT.iter().any(|needle| log.contains(needle)),
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

/// Runs `<program> tunnel --stdio` and joins its stdio into a duplex.
async fn spawn_tunnel(session: Arc<Session>, program: &str) -> Result<SshStream, SshError> {
    let mut child = session
        .arc_command(program)
        .args(["tunnel", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .await?;
    let missing = || SshError::Io(io::Error::other("tunnel stdio was not piped"));
    let stdin = child.stdin().take().ok_or_else(missing)?;
    let stdout = child.stdout().take().ok_or_else(missing)?;
    let mut stderr = child.stderr().take().ok_or_else(missing)?;
    let (exit_tx, exit_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let collect = async move {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf).await;
            buf
        };
        let waiting = async move {
            let (status, stderr) = tokio::join!(child.wait(), collect);
            ExitInfo {
                status: status.map_err(|error| error.to_string()),
                stderr: tail(&stderr),
            }
        };
        // Dropping the stream drops `stop_tx`; the child has kill_on_drop, so
        // abandoning `waiting` disconnects the local ssh client.
        tokio::select! {
            info = waiting => {
                let _ = exit_tx.send(info);
            }
            _ = stop_rx => {}
        }
    });
    Ok(SshStream {
        io: Joined::new(stdout, stdin),
        exit: Some(exit_rx),
        finished: None,
        _stop: stop_tx,
    })
}

/// The tunnel process's stdio as one duplex.
///
/// At EOF it waits for the remote exit status, so a crashed or missing
/// `tunnel` command surfaces as an error carrying its stderr instead of a
/// bare disconnect.
pub struct SshStream {
    io: Joined<ChildStdout, ChildStdin>,
    exit: Option<oneshot::Receiver<ExitInfo>>,
    finished: Option<Result<(), String>>,
    _stop: oneshot::Sender<()>,
}

impl SshStream {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes an executable fake `ssh` that records its environment and fails like ssh.
    fn fake_ssh(dir: &Path, log_line: &str) -> io::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-ssh");
        let script = format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do case \"$1\" in -E) log=\"$2\"; shift;; esac; shift; done\n\
             printf '%s\\n' \"askpass=$SSH_ASKPASS require=$SSH_ASKPASS_REQUIRE\" > \"{}/env\"\n\
             printf '%s\\n' {} > \"$log\"\n\
             exit 255\n",
            dir.display(),
            crate::sh_quote(log_line),
        );
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    struct Never;

    #[async_trait]
    impl crate::PromptRelay for Never {
        async fn prompt(&self, _prompt: crate::AskpassPrompt) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn rejected_credentials_are_permanent_and_askpass_env_is_set()
    -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir()?;
            let mut options = SshOptions::new(dir.path());
            options.ssh_program = fake_ssh(dir.path(), "user@box: Permission denied (publickey).")?;
            options.askpass = Some(AskpassConfig {
                exe: PathBuf::from("/usr/bin/devlanes"),
                relay: Arc::new(Never),
            });
            let transport = SshTransport::new("box", options);
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(error.is_permanent());
            assert!(error.to_string().contains("Permission denied"));
            let env = std::fs::read_to_string(dir.path().join("env"))?;
            assert!(env.contains("require=force"), "{env}");
            assert!(env.contains(".askpass"), "{env}");
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn network_failures_are_transient() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir()?;
            let mut options = SshOptions::new(dir.path());
            options.ssh_program = fake_ssh(
                dir.path(),
                "connect to host box port 22: Connection refused",
            )?;
            let transport = SshTransport::new("box", options);
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!error.is_permanent());
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn long_control_paths_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let deep = dir.path().join("x".repeat(120));
        let transport = SshTransport::new("box", SshOptions::new(deep));
        let result = transport.launch_master().await;
        assert!(matches!(result, Err(SshError::ControlPathTooLong(_))));
        Ok(())
    }
}
