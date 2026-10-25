//! Windows: one `ssh.exe` per session.
//!
//! Win32-OpenSSH has no ControlMaster (it would need descriptor passing over
//! Unix sockets), so nothing outlives a session and each connect
//! authenticates again. To keep that to one authentication per connect, the
//! probe and the tunnel share the process: the box prints the probe and a
//! sentinel line, then execs `devlanes tunnel --stdio`, and everything after
//! the sentinel is mux traffic. Installing devlanes and a standalone
//! [`SshTransport::probe`] are runs of their own.
//!
//! Every `ssh.exe` starts inside a kill-on-close job, so whatever it starts
//! (a `ProxyCommand`) ends with the session, and with the daemon if the
//! daemon dies. It shares the daemon's hidden console, so no window opens.
//!
//! The public types mirror the Unix module's, which stays as it was.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::{self, PipeReader, PipeWriter};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_trait::async_trait;
use devlanes_sys_windows::job::{Job, JobOptions};
use devlanes_sys_windows::spawn::{self, Child, CreationFlags, Environment, Spawn, Stdio, Stream};
use devlanes_transport::{BoxDuplex, Joined, Transport, TransportError};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, BufReader, ReadBuf};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::askpass::{AskpassConfig, AskpassServer, SOCKET_ENV};
use crate::bootstrap::{
    self, BootstrapPlan, BootstrapPolicy, EXIT_ALREADY_INSTALLED, ManifestError, ReleaseManifest,
    RemotePlatform, RemoteProbe,
};
use crate::sh_quote;

/// Bytes of remote stderr kept for error messages.
const STDERR_TAIL: usize = 4096;

/// Bytes of ssh's stderr kept while a session runs; read on so ssh never
/// blocks on a full pipe.
const STDERR_KEPT: usize = 64 * 1024;

/// Bound on what the box may print before the sentinel.
const MAX_HEADER: u64 = 64 * 1024;

/// Bound on an install run, which downloads the release on the box.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a failed ssh gets to report its exit status and last words.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// ssh's own exit status for connection and authentication failures.
const SSH_FAILED: i32 = 255;

/// Settings for [`SshTransport`].
#[derive(Clone)]
pub struct SshOptions {
    /// Kept for the Unix API, where control sockets live there; Windows
    /// writes nothing to disk.
    pub control_dir: PathBuf,
    /// Local `ssh` binary. The bare name `ssh` means the system OpenSSH client
    /// (`<System32>\OpenSSH\ssh.exe`), else the first `ssh.exe` on `PATH`.
    pub ssh_program: PathBuf,
    /// Remote command name, looked up on PATH and then in `$HOME/.devlanes/bin`.
    pub remote_program: String,
    /// TCP connect limit passed as `ConnectTimeout`.
    pub connect_timeout: Duration,
    /// Limit for one session's start, including time spent answering prompts.
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
    /// Defaults, with `control_dir` under `<devlanes_home>/run/ssh` as on Unix.
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
    /// Local process I/O failed.
    #[error("ssh I/O: {0}")]
    Io(#[from] io::Error),
    /// No OpenSSH client was found.
    #[error(
        "no ssh client found ({0}); add the Windows optional feature \"OpenSSH Client\" or put ssh.exe on PATH"
    )]
    ProgramMissing(PathBuf),
    /// ssh did not connect or authenticate; `log` holds its diagnostics.
    #[error("ssh to {destination} failed ({status}): {log}")]
    Master {
        /// The ssh destination.
        destination: String,
        /// Exit status description.
        status: String,
        /// Tail of ssh's stderr.
        log: String,
        /// Whether retrying cannot help, such as rejected credentials.
        permanent: bool,
    },
    /// ssh did not finish authenticating in time.
    #[error("ssh to {0} did not authenticate in time")]
    AuthTimeout(String),
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
            Self::ProgramMissing(_)
            | Self::BootstrapRequired(_)
            | Self::Manifest(_)
            | Self::AlreadyInstalled
            | Self::Install(_) => true,
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

/// [`Transport`] that runs `devlanes tunnel --stdio` on a box over system OpenSSH.
///
/// The user's `%USERPROFILE%\.ssh\config`, agent, ProxyJump and certificates
/// apply because the real `ssh.exe` does the work.
pub struct SshTransport {
    destination: String,
    options: SshOptions,
    /// What the last session or probe found on the box.
    probe: Mutex<Option<RemoteProbe>>,
    /// Cancelled by [`SshTransport::close`] and on drop, ending every session.
    sessions: Mutex<CancellationToken>,
    /// Set after the first tunnel opened: from then on a failing DNS lookup is a
    /// network blip (sleep, Wi-Fi change), not a misspelt host.
    connected: AtomicBool,
}

/// What one session run found.
enum Started {
    /// devlanes runs on the box and the stream carries the tunnel.
    Tunnel(Box<SshStream>),
    /// The box has no devlanes; the ssh process is gone.
    Missing(RemoteProbe),
}

/// A running `ssh.exe`. Its job ends ssh and everything it started once
/// dropped.
struct SshProcess {
    _job: Job,
    child: Arc<Child>,
}

/// This side's ends of ssh's standard streams.
struct Pipes {
    /// `None` when ssh reads `NUL`.
    stdin: Option<File>,
    stdout: File,
    stderr: File,
}

impl SshProcess {
    /// Waits for ssh to exit, off the executor.
    async fn wait(&self) -> io::Result<ExitStatus> {
        let child = self.child.clone();
        tokio::task::spawn_blocking(move || child.wait())
            .await
            .map_err(io::Error::other)?
            .map(ExitStatus::from_raw)
    }
}

/// What a finished run printed and how it ended.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl SshTransport {
    /// Targets `destination`, any name `ssh` accepts.
    pub fn new(destination: impl Into<String>, options: SshOptions) -> Self {
        Self {
            destination: destination.into(),
            options,
            probe: Mutex::new(None),
            sessions: Mutex::new(CancellationToken::new()),
            connected: AtomicBool::new(false),
        }
    }

    /// The ssh destination.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// Reports the box platform and devlanes location: what the last session
    /// found, else from a probe-only ssh run.
    pub async fn probe(&self) -> Result<RemoteProbe, SshError> {
        if let Some(probe) = self.remembered() {
            return Ok(probe);
        }
        let script = bootstrap::probe_script(&self.options.remote_program);
        let output = self
            .run_script(&script, self.options.auth_timeout)
            .await?
            .ok_or_else(|| SshError::AuthTimeout(self.destination.clone()))?;
        if let Some(error) = self.ssh_failed(&output) {
            return Err(error);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let probe =
            bootstrap::parse_probe(&stdout).ok_or_else(|| SshError::Probe(stdout.into_owned()))?;
        self.remember(Some(probe.clone()));
        Ok(probe)
    }

    /// Installs devlanes on the box as described by `plan`, in its own ssh run.
    ///
    /// Refuses when a binary exists already; restarting a box daemon is left to the user.
    pub async fn bootstrap(&self, plan: &BootstrapPlan) -> Result<(), SshError> {
        let artifact = plan
            .artifact
            .as_ref()
            .ok_or_else(|| SshError::BootstrapRequired(Box::new(plan.clone())))?;
        artifact.validate()?;
        let script = bootstrap::install_script(artifact)?;
        let output = self.run_script(&script, INSTALL_TIMEOUT).await?;
        // A new probe must find the installed binary.
        self.remember(None);
        let output = output.ok_or_else(|| {
            SshError::Install(format!(
                "no result within {} seconds",
                INSTALL_TIMEOUT.as_secs()
            ))
        })?;
        if let Some(error) = self.ssh_failed(&output) {
            return Err(error);
        }
        match output.status.code() {
            Some(0) => Ok(()),
            Some(EXIT_ALREADY_INSTALLED) => Err(SshError::AlreadyInstalled),
            _ => Err(SshError::Install(tail(&output.stderr))),
        }
    }

    /// Ends every session this transport started.
    pub async fn close(&self) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.cancel();
        *sessions = CancellationToken::new();
    }

    /// Opens the tunnel, bootstrapping if allowed.
    async fn open(&self) -> Result<SshStream, SshError> {
        let stream = self.open_tunnel().await?;
        self.connected.store(true, Ordering::Relaxed);
        Ok(stream)
    }

    /// [`SshTransport::open`] without the bookkeeping.
    async fn open_tunnel(&self) -> Result<SshStream, SshError> {
        let probe = match self.start().await? {
            Started::Tunnel(stream) => return Ok(*stream),
            Started::Missing(probe) => probe,
        };
        let plan = self.bootstrap_plan(probe.platform).await;
        if self.options.bootstrap == BootstrapPolicy::Report || plan.artifact.is_none() {
            return Err(SshError::BootstrapRequired(Box::new(plan)));
        }
        self.bootstrap(&plan).await?;
        match self.start().await? {
            Started::Tunnel(stream) => Ok(*stream),
            Started::Missing(_) => Err(SshError::Install("binary not found after install".into())),
        }
    }

    /// The release manifest: the configured one, else the one at `manifest_url`.
    async fn manifest(&self) -> Result<Option<ReleaseManifest>, ManifestError> {
        if let Some(manifest) = &self.options.manifest {
            return Ok(Some(manifest.clone()));
        }
        match &self.options.manifest_url {
            Some(url) => bootstrap::fetch_manifest(url, &self.options.release_version)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// What installing devlanes on a box of `platform` would take.
    async fn bootstrap_plan(&self, platform: RemotePlatform) -> BootstrapPlan {
        let (manifest, manifest_error) = match self.manifest().await {
            Ok(manifest) => (manifest, None),
            Err(error) => (None, Some(error.0)),
        };
        BootstrapPlan {
            destination: self.destination.clone(),
            artifact: manifest.as_ref().and_then(|m| m.select(&platform)).cloned(),
            platform,
            manifest_error,
        }
    }

    /// Runs the session script: probe, sentinel, tunnel.
    async fn start(&self) -> Result<Started, SshError> {
        // Taken before ssh starts, so a `close` while it authenticates ends
        // this session too instead of leaving a tunnel opened after it.
        let closed = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .child_token();
        let token = crate::random_token()?;
        let start = format!("devlanes-probe-{token}");
        let end = format!("devlanes-tunnel-ready-{token}");
        let script = bootstrap::session_script(&self.options.remote_program, &start, &end);
        let askpass = self.askpass().await?;
        let (process, pipes) = self.spawn(&script, true, askpass.as_ref()).await?;
        let stdin = pipes
            .stdin
            .ok_or_else(|| SshError::Io(io::Error::other("ssh stdin was not piped")))?;
        let stderr = tokio::spawn(collect_tail(pipes.stderr));
        let mut stdout = BufReader::new(pipes.stdout);
        let header = tokio::select! {
            header = tokio::time::timeout(self.options.auth_timeout, read_header(&mut stdout, &end)) => header,
            () = closed.cancelled() => {
                return Err(SshError::Io(io::Error::other("the SSH transport was closed")));
            }
        };
        // Prompts end with authentication; the box has answered by now.
        drop(askpass);
        // Returning early drops `process`, whose job ends ssh and whatever
        // ssh started.
        let lines = match header {
            Err(_) => return Err(SshError::AuthTimeout(self.destination.clone())),
            Ok(Err(error)) => return Err(SshError::Io(error)),
            Ok(Ok(Header::Ready(lines))) => lines,
            Ok(Ok(Header::Ended { printed })) => {
                return Err(self.ended_early(&process, stderr, printed).await);
            }
        };
        let probe = bootstrap::parse_session_header(&lines, &start)
            .ok_or_else(|| SshError::Probe(lines.join("\n")))?;
        self.remember(Some(probe.clone()));
        if probe.program.is_none() {
            return Ok(Started::Missing(probe));
        }
        let (exit_tx, exit_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let waiting = async {
                let status = process.wait().await;
                let stderr = stderr.await.unwrap_or_default();
                ExitInfo {
                    status: status.map_err(|error| error.to_string()),
                    stderr: tail(&stderr),
                }
            };
            // Dropping the stream drops `stop_tx`. Leaving this task drops
            // `process`, and its job ends ssh's whole tree.
            tokio::select! {
                info = waiting => {
                    let _ = exit_tx.send(info);
                }
                _ = stop_rx => {}
                () = closed.cancelled() => {}
            }
            drop(process);
        });
        Ok(Started::Tunnel(Box::new(SshStream {
            io: Joined::new(stdout, stdin),
            exit: Some(exit_rx),
            finished: None,
            _stop: stop_tx,
        })))
    }

    /// The error for a session whose output ended before the sentinel. Exit
    /// status 255 is ssh's own failure, classified from its stderr, unless the
    /// box `printed` something: ssh had authenticated then, and the 255 came
    /// from the box (its command exiting 255 or dying by a signal). Any other
    /// end came from the box too. ssh's stderr also carries the box's, so a
    /// shell startup file printing "Permission denied" or a missing `sh` (exit
    /// 127) must not read as rejected credentials, which would stop
    /// reconnecting for good; those are temporary errors carrying the stderr
    /// tail.
    async fn ended_early(
        &self,
        process: &SshProcess,
        stderr: JoinHandle<Vec<u8>>,
        printed: bool,
    ) -> SshError {
        let status = tokio::time::timeout(EXIT_GRACE, process.wait()).await;
        let log = match tokio::time::timeout(EXIT_GRACE, stderr).await {
            Ok(Ok(log)) => log,
            _ => Vec::new(),
        };
        let log = String::from_utf8_lossy(&log);
        let status = match status {
            Ok(Ok(status)) if status.success() => {
                return SshError::Probe(format!(
                    "the box ended the session before the probe finished: {}",
                    log.trim()
                ));
            }
            Ok(Ok(status)) if status.code() == Some(SSH_FAILED) && !printed => {
                return master_error(&self.destination, status, &log, self.first());
            }
            Ok(Ok(status)) => status.to_string(),
            Ok(Err(error)) => error.to_string(),
            Err(_) => "no exit status".to_owned(),
        };
        SshError::Master {
            destination: self.destination.clone(),
            status,
            log: tail(log.as_bytes()),
            permanent: false,
        }
    }

    /// Runs `script` on the box with `sh -c` in its own ssh run, waiting at most
    /// `limit`; `None` when it did not finish in time (ssh is ended then).
    async fn run_script(&self, script: &str, limit: Duration) -> Result<Option<Output>, SshError> {
        let askpass = self.askpass().await?;
        let (process, pipes) = self.spawn(script, false, askpass.as_ref()).await?;
        let finished = tokio::time::timeout(limit, async {
            let (stdout, stderr) =
                tokio::join!(collect_tail(pipes.stdout), collect_tail(pipes.stderr));
            let status = process.wait().await?;
            Ok::<_, io::Error>(Output {
                status,
                stdout,
                stderr,
            })
        })
        .await;
        drop(askpass);
        // Anything ssh left running (a ProxyCommand) ends here.
        drop(process);
        match finished {
            Ok(output) => Ok(Some(output?)),
            Err(_) => Ok(None),
        }
    }

    /// A prompt relay for one ssh run, when prompts are relayed at all.
    async fn askpass(&self) -> Result<Option<AskpassServer>, SshError> {
        match &self.options.askpass {
            Some(config) => Ok(Some(AskpassServer::start(config, &self.destination).await?)),
            None => Ok(None),
        }
    }

    /// Starts `ssh.exe` running `script` with `sh -c` on the box, in a new
    /// kill-on-close job, with piped stdout and stderr and, for a session,
    /// piped stdin (`NUL` otherwise).
    async fn spawn(
        &self,
        script: &str,
        session: bool,
        askpass: Option<&AskpassServer>,
    ) -> Result<(SshProcess, Pipes), SshError> {
        let requested = self.options.ssh_program.clone();
        let program = tokio::task::spawn_blocking(move || locate_program(&requested))
            .await
            .map_err(io::Error::other)?
            .map_err(SshError::ProgramMissing)?;
        let line = self.command_line(&program, script, askpass.is_none());
        let env = askpass.map(AskpassServer::env).unwrap_or_default();
        let found = program.clone();
        // Process creation touches the disk, so it stays off the executor.
        let spawned = tokio::task::spawn_blocking(move || start(&found, &line, &env, session))
            .await
            .map_err(io::Error::other)?;
        match spawned {
            Ok(process) => Ok(process),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(SshError::ProgramMissing(program))
            }
            Err(error) => Err(SshError::Io(error)),
        }
    }

    /// The whole command line: `ssh -T` with keepalives, the connect limit,
    /// the extra options, batch mode without a relay, `--`, the destination
    /// and the remote command.
    fn command_line(&self, program: &Path, script: &str, batch: bool) -> OsString {
        let mut args: Vec<String> = vec![
            program.to_string_lossy().into_owned(),
            // No terminal on the box: the tunnel speaks binary over stdio.
            "-T".to_owned(),
            "-o".to_owned(),
            "ServerAliveInterval=15".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
            "-o".to_owned(),
            format!(
                "ConnectTimeout={}",
                self.options.connect_timeout.as_secs().max(1)
            ),
        ];
        for option in &self.options.ssh_options {
            args.push("-o".to_owned());
            args.push(option.clone());
        }
        if batch {
            args.push("-o".to_owned());
            args.push("BatchMode=yes".to_owned());
        }
        // `--` keeps a destination starting with `-` from being parsed as an option.
        args.push("--".to_owned());
        args.push(self.destination.clone());
        // ssh joins its remaining arguments into one command for the login
        // shell, so the script travels as one quoted word.
        args.push(format!("sh -c {}", sh_quote(script)));
        let quoted: Vec<String> = args.iter().map(|arg| spawn::quote_arg(arg)).collect();
        OsString::from(quoted.join(" "))
    }

    /// ssh's own failure in a finished run, classified from its stderr. A run
    /// in which the box printed something had authenticated, so its 255 is the
    /// script's.
    fn ssh_failed(&self, output: &Output) -> Option<SshError> {
        (output.status.code() == Some(SSH_FAILED) && output.stdout.is_empty()).then(|| {
            master_error(
                &self.destination,
                output.status,
                &String::from_utf8_lossy(&output.stderr),
                self.first(),
            )
        })
    }

    /// Whether no tunnel has opened yet.
    fn first(&self) -> bool {
        !self.connected.load(Ordering::Relaxed)
    }

    /// The probe the last run found.
    fn remembered(&self) -> Option<RemoteProbe> {
        self.probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records (or forgets) what the box reported.
    fn remember(&self, probe: Option<RemoteProbe>) {
        *self.probe.lock().unwrap_or_else(PoisonError::into_inner) = probe;
    }
}

impl Drop for SshTransport {
    /// Ends the sessions, as stopping the Unix master does.
    fn drop(&mut self) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cancel();
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn connect(&self) -> Result<BoxDuplex, TransportError> {
        Ok(Box::new(self.open().await?))
    }
}

/// Starts `program` with `line` inside a new kill-on-close job. The
/// environment is this process's, without the per-drive `=C:` entries (which
/// cannot go into a block) and with `env` replacing any spelling of its
/// names. Blocking.
fn start(
    program: &Path,
    line: &OsStr,
    env: &[(&'static str, OsString)],
    session: bool,
) -> io::Result<(SshProcess, Pipes)> {
    let replaced = |name: &OsStr| {
        name.to_str().is_none_or(|name| {
            name.starts_with('=')
                || name.eq_ignore_ascii_case(SOCKET_ENV)
                || env.iter().any(|(own, _)| name.eq_ignore_ascii_case(own))
        })
    };
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !replaced(name))
        .collect();
    variables.extend(
        env.iter()
            .map(|(name, value)| (OsString::from(name), value.clone())),
    );
    let environment = Environment::new(variables)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let job = Job::create(JobOptions {
        kill_on_close: true,
        die_on_unhandled_exception: true,
    })?;
    let (out_read, out_write) = spawn::pipe()?;
    let (err_read, err_write) = spawn::pipe()?;
    let stdin_pipe = if session { Some(spawn::pipe()?) } else { None };
    let child = spawn::spawn(&Spawn {
        program,
        command_line: line,
        environment: Some(&environment),
        cwd: None,
        stdio: Stdio::Handles {
            stdin: stdin_pipe
                .as_ref()
                .map_or(Stream::Null, |(read, _)| Stream::Handle(read.as_handle())),
            stdout: Stream::Handle(out_write.as_handle()),
            stderr: Stream::Handle(err_write.as_handle()),
        },
        inherit: &[],
        job: Some(&job),
        // ssh shares the daemon's hidden console: no window, no console host.
        flags: CreationFlags::default(),
    })?;
    // ssh holds its own ends now; this process keeps only the other ones, so
    // ssh's exit reaches the readers as end of file.
    drop((out_write, err_write));
    let stdin = stdin_pipe.map(|(read, write)| {
        drop(read);
        writer(write)
    });
    Ok((
        SshProcess {
            _job: job,
            child: Arc::new(child),
        },
        Pipes {
            stdin,
            stdout: reader(out_read),
            stderr: reader(err_read),
        },
    ))
}

/// An async reader over the reading end of an anonymous pipe. Anonymous pipes
/// have no overlapped I/O, so tokio reads them on its blocking pool, as it
/// does a child's standard streams.
fn reader(pipe: PipeReader) -> File {
    File::from_std(std::fs::File::from(OwnedHandle::from(pipe)))
}

/// An async writer over the writing end of an anonymous pipe; see [`reader`].
fn writer(pipe: PipeWriter) -> File {
    File::from_std(std::fs::File::from(OwnedHandle::from(pipe)))
}

/// Resolves a bare `ssh` to the system OpenSSH client
/// (`<System32>\OpenSSH\ssh.exe`, found through the API rather than
/// `%SystemRoot%`, which the environment controls), else the first match on
/// `PATH`. Git for Windows ships an MSYS `ssh` that also works, but it reads
/// its own `/etc/ssh` and does not use the Windows agent service, so the
/// system client goes first. Paths with a directory are used as given. `Err`
/// carries the name looked for when nothing matched: a bare name is never
/// started, since `CreateProcessW` would look for it in the working
/// directory. Blocking.
fn locate_program(program: &Path) -> Result<PathBuf, PathBuf> {
    if program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return Ok(program.to_path_buf());
    }
    let name = if program.extension().is_none() {
        program.with_extension("exe")
    } else {
        program.to_path_buf()
    };
    let system = devlanes_sys_windows::file::system_directory()
        .ok()
        .map(|system| system.join("OpenSSH"));
    let path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    system
        .into_iter()
        .chain(path.into_iter().filter(|dir| dir.is_absolute()))
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .ok_or(name)
}

/// ssh's wording for rejected credentials, as in `user@box: Permission denied
/// (publickey).`. The log is ssh's stderr, which also carries the box's,
/// where a bare "Permission denied" names a file.
const DENIED: &str = "Permission denied (";

/// Classifies a failed connection from ssh's stderr.
///
/// A name that does not resolve is permanent only before the first successful
/// connection (a typo); afterwards it is a network change that will pass.
fn master_error(destination: &str, status: ExitStatus, log: &str, first: bool) -> SshError {
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

/// How the output before the sentinel ended.
#[derive(Debug, PartialEq, Eq)]
enum Header {
    /// The sentinel came; the lines before it.
    Ready(Vec<String>),
    /// End of file first. `printed` says whether the box wrote anything, which
    /// means ssh had authenticated and the box's side ended the session.
    Ended { printed: bool },
}

/// Reads lines until `sentinel`. The reader keeps whatever follows the
/// sentinel for the tunnel.
async fn read_header<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    sentinel: &str,
) -> io::Result<Header> {
    let mut lines = Vec::new();
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take(MAX_HEADER + 1 - total)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(Header::Ended { printed: total > 0 });
        }
        total += read as u64;
        if total > MAX_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the box printed too much before the probe finished",
            ));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text == sentinel {
            return Ok(Header::Ready(lines));
        }
        lines.push(text.to_owned());
    }
}

/// Reads `reader` to its end, keeping the last [`STDERR_KEPT`] bytes.
async fn collect_tail(mut reader: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                kept.extend_from_slice(&buffer[..read]);
                if kept.len() > STDERR_KEPT {
                    kept.drain(..kept.len() - STDERR_KEPT);
                }
            }
        }
    }
}

/// How the remote tunnel process ended.
struct ExitInfo {
    status: Result<ExitStatus, String>,
    stderr: String,
}

/// The tunnel's standard streams as one duplex.
///
/// At end of file it waits for ssh's exit status, so a crashed or missing
/// `tunnel` command surfaces as an error carrying its stderr instead of a
/// bare disconnect.
pub struct SshStream {
    io: Joined<BufReader<File>, File>,
    exit: Option<oneshot::Receiver<ExitInfo>>,
    finished: Option<Result<(), String>>,
    _stop: oneshot::Sender<()>,
}

impl SshStream {
    /// Resolves end of file into success or the remote failure.
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
    use devlanes_testing::fake_tool::{Behaviour, install};

    use super::*;
    use crate::askpass::{AskpassPrompt, PromptRelay};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct Never;

    #[async_trait]
    impl PromptRelay for Never {
        async fn prompt(&self, _prompt: AskpassPrompt) -> Option<String> {
            None
        }
    }

    /// A transport whose ssh is a fake that prints `stderr` and exits with `code`.
    fn fake(
        dir: &Path,
        stderr: &str,
        code: u8,
    ) -> Result<SshTransport, Box<dyn std::error::Error>> {
        let ssh = install(
            dir,
            "ssh",
            &Behaviour::new()
                .record(dir.join("args"))
                .stderr(stderr)
                .exit(code),
        )?;
        let mut options = SshOptions::new(dir);
        options.ssh_program = ssh;
        Ok(SshTransport::new("box", options))
    }

    /// The arguments the fake recorded on its first run.
    fn recorded(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(dir.join("args"))?;
        let first = text.lines().next().ok_or("the fake recorded nothing")?;
        Ok(serde_json::from_str(first)?)
    }

    /// Rejected credentials stop reconnecting. The run carries the askpass
    /// environment and the session script after `--` and the destination.
    #[tokio::test]
    async fn rejected_credentials_are_permanent_and_askpass_env_is_set() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let mut transport = fake(
                dir.path(),
                "require={env:SSH_ASKPASS_REQUIRE} relay={env:DEVLANES_ASKPASS_SOCKET} \
                 askpass={env:SSH_ASKPASS}\nuser@box: Permission denied (publickey).\n",
                255,
            )?;
            transport.options.askpass = Some(AskpassConfig {
                exe: PathBuf::from(r"C:\acme\devlanes.exe"),
                relay: Arc::new(Never),
            });
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(error.is_permanent(), "{error}");
            let message = error.to_string();
            assert!(message.contains("Permission denied"), "{message}");
            assert!(message.contains("require=force"), "{message}");
            assert!(
                message.contains(r"relay=\\.\pipe\devlanes-askpass-"),
                "{message}"
            );
            assert!(
                message.contains(r"askpass=C:\acme\devlanes.exe"),
                "{message}"
            );
            let args = recorded(dir.path())?;
            let at = args
                .iter()
                .position(|arg| arg == "--")
                .ok_or("no -- before the destination")?;
            assert_eq!(args[at + 1], "box");
            assert!(args[at + 2].starts_with("sh -c 'printf "), "{args:?}");
            assert_eq!(args.len(), at + 3, "{args:?}");
            assert!(args.contains(&"-T".to_owned()), "{args:?}");
            assert!(!args.contains(&"BatchMode=yes".to_owned()), "{args:?}");
            Ok(())
        })
        .await?
    }

    /// Network failures keep the link retrying; without askpass ssh runs in
    /// batch mode and gets no relay.
    #[tokio::test]
    async fn network_failures_are_transient() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let transport = fake(
                dir.path(),
                "relay=[{env:DEVLANES_ASKPASS_SOCKET}]\n\
                 ssh: connect to host box port 22: Connection refused\n",
                255,
            )?;
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!error.is_permanent(), "{error}");
            assert!(error.to_string().contains("relay=[]"), "{error}");
            assert!(recorded(dir.path())?.contains(&"BatchMode=yes".to_owned()));
            Ok(())
        })
        .await?
    }

    /// A session that ends with a status other than ssh's own 255 failed on the
    /// box, even when the box's stderr (which ssh passes on) says "Permission
    /// denied": the link keeps retrying and the error carries that stderr.
    #[tokio::test]
    async fn box_failures_before_the_tunnel_are_transient() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            for code in [1, 127] {
                let run = dir.path().join(code.to_string());
                std::fs::create_dir(&run)?;
                let transport = fake(
                    &run,
                    "bash: /home/acme/.bashrc: Permission denied\nsh: devlanes: not found\n",
                    code,
                )?;
                let Err(error) = transport.connect().await else {
                    panic!("fake ssh unexpectedly connected");
                };
                assert!(!error.is_permanent(), "exit {code}: {error}");
                let message = error.to_string();
                assert!(message.contains(".bashrc: Permission denied"), "{message}");
                assert!(message.contains(&code.to_string()), "{message}");
            }
            Ok(())
        })
        .await?
    }

    /// Exit 255 is ssh's own only while the box printed nothing, and even then
    /// a bare "Permission denied" from the box's shell is not ssh rejecting
    /// credentials: both keep the link retrying.
    #[tokio::test]
    async fn box_failures_with_status_255_are_transient() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let quiet = dir.path().join("quiet");
            std::fs::create_dir(&quiet)?;
            let transport = fake(&quiet, "bash: /home/acme/.bashrc: Permission denied\n", 255)?;
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!error.is_permanent(), "{error}");

            let printed = dir.path().join("printed");
            std::fs::create_dir(&printed)?;
            let ssh = install(
                &printed,
                "ssh",
                &Behaviour::new()
                    .stdout("devlanes-probe-x\n")
                    .stderr("user@box: Permission denied (publickey).\n")
                    .exit(255),
            )?;
            let mut options = SshOptions::new(&printed);
            options.ssh_program = ssh;
            let transport = SshTransport::new("box", options);
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!error.is_permanent(), "{error}");
            Ok(())
        })
        .await?
    }

    /// A misspelt host stops the link only before the first connection.
    #[tokio::test]
    async fn dns_failures_are_permanent_only_before_the_first_connection() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let transport = fake(
                dir.path(),
                "ssh: Could not resolve hostname box: No such host is known.\n",
                255,
            )?;
            let Err(first) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(first.is_permanent(), "a typo should stop the link");
            transport.connected.store(true, Ordering::Relaxed);
            let Err(later) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!later.is_permanent(), "a reconnect after DNS loss retries");
            Ok(())
        })
        .await?
    }

    /// An ssh that never prints the sentinel fails the session once the
    /// authentication limit passes, and is ended with its job.
    #[tokio::test]
    async fn a_silent_ssh_times_out() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let ssh = install(
                dir.path(),
                "ssh",
                &Behaviour::new().sleep(Duration::from_secs(60)),
            )?;
            let mut options = SshOptions::new(dir.path());
            options.ssh_program = ssh;
            options.auth_timeout = Duration::from_millis(500);
            let transport = SshTransport::new("box", options);
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!error.is_permanent(), "{error}");
            assert!(error.to_string().contains("in time"), "{error}");
            Ok(())
        })
        .await?
    }

    /// A probe run reads the box's answer; the next probe reuses it.
    #[tokio::test]
    async fn probes_parse_and_are_remembered() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let ssh = install(
                dir.path(),
                "ssh",
                &Behaviour::new()
                    .record(dir.path().join("args"))
                    .stdout("Linux\nx86_64\n/usr/local/bin/devlanes\n"),
            )?;
            let mut options = SshOptions::new(dir.path());
            options.ssh_program = ssh;
            let transport = SshTransport::new("box", options);
            let probe = transport.probe().await?;
            assert_eq!(probe.platform.os, "linux");
            assert_eq!(probe.program.as_deref(), Some("/usr/local/bin/devlanes"));
            assert_eq!(transport.probe().await?, probe);
            let runs = std::fs::read_to_string(dir.path().join("args"))?;
            assert_eq!(runs.lines().count(), 1, "{runs}");
            Ok(())
        })
        .await?
    }

    /// A missing client is a permanent error that names the optional feature.
    #[tokio::test]
    async fn a_missing_client_is_reported() -> TestResult {
        let dir = tempfile::tempdir()?;
        let mut options = SshOptions::new(dir.path());
        options.ssh_program = dir.path().join("no-such-ssh.exe");
        let transport = SshTransport::new("box", options);
        let Err(error) = transport.connect().await else {
            panic!("a missing ssh connected");
        };
        assert!(error.is_permanent());
        assert!(error.to_string().contains("OpenSSH Client"), "{error}");
        Ok(())
    }

    /// Bare names prefer the system client; paths are kept; a name found
    /// nowhere is never left for `CreateProcessW` to look up.
    #[test]
    fn bare_names_find_the_system_client() {
        let explicit = Path::new(r"C:\tools\ssh.exe");
        assert_eq!(locate_program(explicit).as_deref(), Ok(explicit));
        let system = devlanes_sys_windows::file::system_directory()
            .ok()
            .map(|system| system.join(r"OpenSSH\ssh.exe"));
        if let Some(system) = system.filter(|path| path.is_file()) {
            assert_eq!(locate_program(Path::new("ssh")), Ok(system));
        }
        assert_eq!(
            locate_program(Path::new("devlanes-no-such-ssh")),
            Err(PathBuf::from("devlanes-no-such-ssh.exe"))
        );
    }

    /// The command line splits back into the arguments ssh needs, the remote
    /// command as one argument whatever quotes it holds.
    #[test]
    fn command_lines_keep_the_remote_command_whole() {
        let mut options = SshOptions::new(r"C:\h");
        options.ssh_options = vec![r#"IdentityFile="C:\Users\a b\key""#.to_owned()];
        let transport = SshTransport::new("acme@box", options);
        let script = r#"printf '%s\n' "$p" 'x'"#;
        let line = transport.command_line(
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe"),
            script,
            true,
        );
        let line = line.to_string_lossy();
        assert!(
            line.starts_with(r"C:\Windows\System32\OpenSSH\ssh.exe -T -o ServerAliveInterval=15"),
            "{line}"
        );
        assert!(
            line.contains(r#"-o "IdentityFile=\"C:\Users\a b\key\"""#),
            "{line}"
        );
        assert!(line.contains("-o BatchMode=yes -- acme@box "), "{line}");
        assert!(
            line.ends_with(r#""sh -c 'printf '\''%s\n'\'' \"$p\" '\''x'\'''""#),
            "{line}"
        );
    }

    /// Output before the sentinel is the header; what follows stays buffered for
    /// the tunnel; end of file first says whether anything was printed.
    #[tokio::test]
    async fn headers_end_at_the_sentinel_and_keep_the_rest() -> TestResult {
        let mut reader = BufReader::new(&b"motd\nLinux\r\nx86_64\n/bin/devlanes\nready-7\nMUX"[..]);
        let header = read_header(&mut reader, "ready-7").await?;
        assert_eq!(
            header,
            Header::Ready(vec![
                "motd".to_owned(),
                "Linux".to_owned(),
                "x86_64".to_owned(),
                "/bin/devlanes".to_owned()
            ])
        );
        let mut rest = String::new();
        reader.read_to_string(&mut rest).await?;
        assert_eq!(rest, "MUX");

        let mut reader = BufReader::new(&b"motd\n"[..]);
        assert_eq!(
            read_header(&mut reader, "other").await?,
            Header::Ended { printed: true }
        );
        let mut reader = BufReader::new(&b""[..]);
        assert_eq!(
            read_header(&mut reader, "other").await?,
            Header::Ended { printed: false }
        );
        Ok(())
    }
}
