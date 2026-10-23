//! Windows: one `ssh` process per session.
//!
//! Win32-OpenSSH has no ControlMaster (it would need descriptor passing over
//! Unix sockets), so nothing outlives a session and each connect authenticates
//! again. To keep that to one authentication per connect, the probe and the
//! tunnel share the process: the box prints the probe and a sentinel line, then
//! execs `devlanes tunnel --stdio`, and everything after the sentinel is mux
//! traffic. Installing devlanes and the standalone [`SshTransport::probe`] are
//! separate ssh runs.
//!
//! Every `ssh.exe` starts in a job that ends its whole tree when the job's handle
//! closes, so a `ProxyCommand` it started dies with the session, and with the
//! daemon if the daemon crashes.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use devlanes_sys::NoWindow as _;
use devlanes_sys::windows::job::Job;
use devlanes_transport::{BoxDuplex, Transport, TransportError};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{ExitInfo, SshError, SshOptions, SshStream, bootstrap_plan, master_error, tail};
use crate::askpass::AskpassServer;
use crate::bootstrap::{self, BootstrapPlan, BootstrapPolicy, EXIT_ALREADY_INSTALLED, RemoteProbe};
use crate::sh_quote;

/// Bound on what the box may print before the sentinel.
const MAX_HEADER: u64 = 64 * 1024;

/// Bytes of ssh's stderr kept while a session runs.
const STDERR_KEPT: usize = 64 * 1024;

/// Bound on an install run, which downloads the release on the box.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a failed ssh gets to report its exit status and last words.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// ssh's own exit status for connection and authentication failures.
const SSH_FAILED: i32 = 255;

/// [`Transport`] that runs `devlanes tunnel --stdio` on a box over system OpenSSH.
///
/// The user's `~/.ssh/config`, agent, ProxyJump and certificates apply because
/// the real `ssh` binary does the work.
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
    Tunnel(SshStream),
    /// The box has no devlanes; the ssh process is gone.
    Missing(RemoteProbe),
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
            Started::Tunnel(stream) => return Ok(stream),
            Started::Missing(probe) => probe,
        };
        let plan = bootstrap_plan(&self.destination, &self.options, probe.platform).await;
        if self.options.bootstrap == BootstrapPolicy::Report || plan.artifact.is_none() {
            return Err(SshError::BootstrapRequired(Box::new(plan)));
        }
        self.bootstrap(&plan).await?;
        match self.start().await? {
            Started::Tunnel(stream) => Ok(stream),
            Started::Missing(_) => Err(SshError::Install("binary not found after install".into())),
        }
    }

    /// Runs the session script: probe, sentinel, tunnel.
    async fn start(&self) -> Result<Started, SshError> {
        let token = crate::random_token()?;
        let start = format!("devlanes-probe-{token}");
        let end = format!("devlanes-tunnel-ready-{token}");
        let script = bootstrap::session_script(&self.options.remote_program, &start, &end);
        let (mut command, program, askpass) = self.command().await?;
        command
            .arg(remote_shell(&script))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, job) = self.spawn_in_job(&mut command, &program)?;
        let missing = || SshError::Io(io::Error::other("ssh stdio was not piped"));
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let stderr = tokio::spawn(collect_tail(child.stderr.take().ok_or_else(missing)?));
        let mut stdout = BufReader::new(stdout);
        let header =
            tokio::time::timeout(self.options.auth_timeout, read_header(&mut stdout, &end)).await;
        // Prompts end with authentication; the box has answered by now.
        drop(askpass);
        // Returning early drops `child`, whose kill_on_drop ends ssh, and `job`,
        // which ends whatever ssh started.
        let lines = match header {
            Err(_) => return Err(SshError::AuthTimeout(self.destination.clone())),
            Ok(Err(error)) => return Err(SshError::Io(error)),
            Ok(Ok(Header::Ready(lines))) => lines,
            Ok(Ok(Header::Ended { printed })) => {
                return Err(self.ended_early(child, stderr, printed).await);
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
        let closed = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .child_token();
        tokio::spawn(async move {
            // Ends ssh's remaining tree whichever way this task finishes.
            let _job = job;
            let waiting = async move {
                let status = child.wait().await;
                let stderr = stderr.await.unwrap_or_default();
                ExitInfo {
                    status: status.map_err(|error| error.to_string()),
                    stderr: tail(&stderr),
                }
            };
            // Dropping the stream drops `stop_tx`; the child has kill_on_drop, so
            // abandoning `waiting` ends the local ssh client.
            tokio::select! {
                info = waiting => {
                    let _ = exit_tx.send(info);
                }
                _ = stop_rx => {}
                () = closed.cancelled() => {}
            }
        });
        Ok(Started::Tunnel(SshStream::new(
            stdout, stdin, exit_rx, stop_tx,
        )))
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
        mut child: Child,
        stderr: JoinHandle<Vec<u8>>,
        printed: bool,
    ) -> SshError {
        let status = tokio::time::timeout(EXIT_GRACE, child.wait()).await;
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
        let (mut command, program, askpass) = self.command().await?;
        command
            .arg(remote_shell(script))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (child, job) = self.spawn_in_job(&mut command, &program)?;
        let output = tokio::time::timeout(limit, child.wait_with_output()).await;
        drop(askpass);
        // Anything ssh left running (a ProxyCommand) ends here.
        drop(job);
        match output {
            Ok(output) => Ok(Some(output?)),
            Err(_) => Ok(None),
        }
    }

    /// The ssh command up to the destination, with askpass routing (or batch
    /// mode), the program it runs, and the relay that must outlive the
    /// authentication.
    async fn command(&self) -> Result<(Command, PathBuf, Option<AskpassServer>), SshError> {
        let requested = self.options.ssh_program.clone();
        let program = tokio::task::spawn_blocking(move || locate_program(&requested))
            .await
            .map_err(io::Error::other)?;
        let mut command = Command::new(&program);
        // The daemon has no console: without this every ssh run would open a
        // console window.
        command.no_window().kill_on_drop(true);
        // No terminal on the box: the tunnel speaks binary over stdio.
        command
            .arg("-T")
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
                let server = AskpassServer::start(config, &self.destination).await?;
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
        Ok((command, program, askpass))
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

    /// Starts `command` inside a new job that ends the whole tree when the job
    /// is dropped or this process dies.
    fn spawn_in_job(
        &self,
        command: &mut Command,
        program: &Path,
    ) -> Result<(Child, Job), SshError> {
        let job = Job::create_kill_on_close()?;
        let child = job
            .spawn_helper(command)
            .map_err(|error| spawn_error(program, error))?;
        Ok((child, job))
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
    /// Ends the sessions, as dropping a Unix master does.
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

/// The remote command for `script`. ssh joins its arguments into one command
/// line for the login shell, so the script travels as one quoted word.
fn remote_shell(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// Resolves a bare `ssh` to the system OpenSSH client (`System32\OpenSSH`,
/// found through the API rather than `%SystemRoot%`, which the environment
/// controls), else the first match on `PATH`. Git for Windows ships an MSYS
/// `ssh` that also works, but it reads its own `/etc/ssh` and `~/.ssh` through
/// MSYS paths, so the system client goes first. Paths with a directory are used
/// as given. Blocking.
fn locate_program(program: &Path) -> PathBuf {
    if program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return program.to_path_buf();
    }
    let name = if program.extension().is_none() {
        program.with_extension("exe")
    } else {
        program.to_path_buf()
    };
    let system = devlanes_sys::windows::dirs::system_directory()
        .ok()
        .map(|system| system.join("OpenSSH"));
    let path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    system
        .into_iter()
        .chain(path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .unwrap_or(name)
}

/// A failed spawn; a missing program gets its own error with an install hint.
fn spawn_error(program: &Path, error: io::Error) -> SshError {
    if error.kind() == io::ErrorKind::NotFound {
        SshError::ProgramMissing(program.to_path_buf())
    } else {
        SshError::Io(error)
    }
}

/// How the output before the sentinel ended.
#[derive(Debug, PartialEq, Eq)]
enum Header {
    /// The sentinel came; the lines before it.
    Ready(Vec<String>),
    /// EOF first. `printed` says whether the box wrote anything, which means
    /// ssh had authenticated and the box's side ended the session.
    Ended { printed: bool },
}

/// Reads lines until `sentinel`. The reader keeps whatever follows the
/// sentinel for the tunnel.
async fn read_header(reader: &mut BufReader<ChildStdout>, sentinel: &str) -> io::Result<Header> {
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

/// Reads `reader` to its end, keeping the last [`STDERR_KEPT`] bytes. Reading
/// on is what keeps ssh from blocking on a full stderr pipe.
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use devlanes_testing::fake_tool::{Behaviour, install};

    use super::*;
    use crate::askpass::{AskpassConfig, AskpassPrompt, PromptRelay};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct Never;

    #[async_trait]
    impl PromptRelay for Never {
        async fn prompt(&self, _prompt: AskpassPrompt) -> Option<String> {
            None
        }
    }

    /// A transport whose ssh is a fake that prints `stderr` and exits like ssh.
    fn fake(dir: &Path, stderr: &str) -> Result<SshTransport, Box<dyn std::error::Error>> {
        fake_exiting(dir, stderr, 255)
    }

    /// A transport whose ssh is a fake that prints `stderr` and exits with `code`.
    fn fake_exiting(
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

    /// Rejected credentials stop reconnecting; the run carries the askpass
    /// environment, the marker and the session script after the destination.
    #[tokio::test]
    async fn rejected_credentials_are_permanent_and_askpass_env_is_set() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let mut transport = fake(
                dir.path(),
                "require={env:SSH_ASKPASS_REQUIRE} marker={env:DEVLANES_ASKPASS} \
                 askpass={env:SSH_ASKPASS}\nuser@box: Permission denied (publickey).\n",
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
            assert!(message.contains("require=force marker=1"), "{message}");
            assert!(
                message.contains(r"askpass=C:\acme\devlanes.exe"),
                "{message}"
            );
            let args: Vec<String> =
                serde_json::from_str(std::fs::read_to_string(dir.path().join("args"))?.trim())?;
            let at = args
                .iter()
                .position(|arg| arg == "--")
                .ok_or("no -- before the destination")?;
            assert_eq!(args[at + 1], "box");
            assert!(args[at + 2].starts_with("sh -c 'printf "), "{args:?}");
            assert!(args.contains(&"-T".to_owned()), "{args:?}");
            assert!(!args.contains(&"BatchMode=yes".to_owned()), "{args:?}");
            Ok(())
        })
        .await?
    }

    /// Network failures keep the link retrying; without askpass ssh runs in batch mode.
    #[tokio::test]
    async fn network_failures_are_transient() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let transport = fake(
                dir.path(),
                "ssh: connect to host box port 22: Connection refused\n",
            )?;
            let Err(error) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(!error.is_permanent(), "{error}");
            let args = std::fs::read_to_string(dir.path().join("args"))?;
            assert!(args.contains("BatchMode=yes"), "{args}");
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
                let transport = fake_exiting(
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
            let transport = fake(&quiet, "bash: /home/acme/.bashrc: Permission denied\n")?;
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

    /// Bare names prefer the system client; paths are kept.
    #[test]
    fn bare_names_find_the_system_client() {
        let explicit = Path::new(r"C:\tools\ssh.exe");
        assert_eq!(locate_program(explicit), explicit);
        let found = locate_program(Path::new("ssh"));
        assert_eq!(
            found.file_name().and_then(|name| name.to_str()),
            Some("ssh.exe")
        );
        let system = devlanes_sys::windows::dirs::system_directory()
            .ok()
            .map(|system| system.join(r"OpenSSH\ssh.exe"));
        if let Some(system) = system.filter(|path| path.is_file()) {
            assert_eq!(found, system);
        }
    }

    /// Output before the sentinel is the header; what follows stays buffered for
    /// the tunnel; EOF first is `None`.
    #[tokio::test]
    async fn headers_end_at_the_sentinel_and_keep_the_rest() -> TestResult {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let tool = install(
                dir.path(),
                "box",
                &Behaviour::new().stdout("motd\nLinux\nx86_64\n/bin/devlanes\nready-7\nMUX"),
            )?;
            let mut child = Command::new(&tool)
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
            let header = read_header(&mut stdout, "ready-7").await?;
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
            stdout.read_to_string(&mut rest).await?;
            assert_eq!(rest, "MUX");
            child.wait().await?;

            let mut child = Command::new(&tool)
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
            assert_eq!(
                read_header(&mut stdout, "other").await?,
                Header::Ended { printed: true }
            );
            child.wait().await?;

            let silent = install(dir.path(), "silent", &Behaviour::new())?;
            let mut child = Command::new(&silent)
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
            assert_eq!(
                read_header(&mut stdout, "other").await?,
                Header::Ended { printed: false }
            );
            child.wait().await?;
            Ok(())
        })
        .await?
    }
}
