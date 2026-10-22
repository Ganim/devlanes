//! Unix: one ControlMaster per box, one channel per session.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use devlanes_transport::{BoxDuplex, Transport, TransportError};
use openssh::{Session, Stdio};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, oneshot};

use super::{ExitInfo, SshError, SshOptions, SshStream, bootstrap_plan, master_error, tail};
use crate::askpass::AskpassServer;
use crate::bootstrap::{self, BootstrapPlan, BootstrapPolicy, EXIT_ALREADY_INSTALLED, RemoteProbe};

/// macOS `sun_path` holds 104 bytes including the terminator, and ssh binds the
/// master at the control path plus `.` and 16 random characters before renaming it.
const MAX_CONTROL_PATH: usize = 104 - 1 - 17;

/// How long an idle master outlives its last session. Finite, so a master left
/// behind by a daemon that died exits on its own; long enough that a restarted
/// daemon reconnects through it without new prompts.
const CONTROL_PERSIST: &str = "ControlPersist=600";

/// Distinguishes askpass relays started by one process.
static NEXT_ASKPASS: AtomicU64 = AtomicU64::new(0);

/// Set once this process cleared masters left by older devlanes versions.
static LEGACY_SWEPT: AtomicBool = AtomicBool::new(false);

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
    /// Set after the first tunnel opened: from then on a failing DNS lookup is a
    /// network blip (sleep, Wi-Fi change), not a misspelt host.
    connected: AtomicBool,
}

impl SshTransport {
    /// Targets `destination`, any name `ssh` accepts.
    pub fn new(destination: impl Into<String>, options: SshOptions) -> Self {
        Self {
            destination: destination.into(),
            options,
            master: Mutex::new(None),
            connected: AtomicBool::new(false),
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
        let stream = self.open_tunnel().await?;
        self.connected.store(true, Ordering::Relaxed);
        Ok(stream)
    }

    /// [`SshTransport::open`] without the bookkeeping.
    async fn open_tunnel(&self) -> Result<SshStream, SshError> {
        let mut guard = self.master.lock().await;
        let master = self.ensure_master(&mut guard).await?;
        let probe = self.ensure_probe(master).await?.clone();
        if let Some(program) = probe.program {
            return spawn_tunnel(master.session.clone(), &program).await;
        }
        let plan = bootstrap_plan(&self.destination, &self.options, probe.platform).await;
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
            let master = match self.resume_master().await {
                Some(master) => master,
                None => self.launch_master().await?,
            };
            *slot = Some(master);
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

    /// The control socket and log of this destination's master.
    ///
    /// The name is stable for a destination and its options, so a restarted
    /// daemon finds a master that is still persisting and reuses it.
    fn control_paths(&self) -> (PathBuf, PathBuf) {
        let mut key = self.destination.clone();
        for option in &self.options.ssh_options {
            key.push('\0');
            key.push_str(option);
        }
        let name = format!("m-{:016x}", fnv1a(key.as_bytes()));
        (
            self.options.control_dir.join(&name),
            self.options.control_dir.join(format!("{name}.log")),
        )
    }

    /// A master left by an earlier daemon for this destination, if it still answers.
    async fn resume_master(&self) -> Option<Master> {
        let (ctl, log) = self.control_paths();
        if !tokio::fs::try_exists(&ctl).await.unwrap_or(false) {
            return None;
        }
        let session = Session::resume(
            ctl.clone().into_boxed_path(),
            Some(log.clone().into_boxed_path()),
        );
        if session.check().await.is_err() {
            // A dead socket blocks a new master at the same path.
            let _ = tokio::fs::remove_file(&ctl).await;
            return None;
        }
        Some(Master {
            session: Arc::new(session),
            ctl,
            log,
            ssh_program: self.options.ssh_program.clone(),
            probe: None,
        })
    }

    /// Starts `ssh -M -f -N` with askpass routing and resumes it as an openssh session.
    ///
    /// openssh's own builder forces `BatchMode=yes` and cannot set the master's
    /// environment, which would rule out askpass, so the master is launched here.
    async fn launch_master(&self) -> Result<Master, SshError> {
        let (ctl, log) = self.control_paths();
        if ctl.as_os_str().len() > MAX_CONTROL_PATH {
            return Err(SshError::ControlPathTooLong(ctl));
        }
        let dir = self.options.control_dir.clone();
        tokio::task::spawn_blocking(move || private_dir(&dir))
            .await
            .map_err(io::Error::other)??;
        if !LEGACY_SWEPT.swap(true, Ordering::Relaxed) {
            sweep_legacy_masters(&self.options.control_dir, &self.options.ssh_program).await;
        }
        let id = format!(
            "{}-{}",
            std::process::id(),
            NEXT_ASKPASS.fetch_add(1, Ordering::Relaxed)
        );
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
            .args(["-o", CONTROL_PERSIST])
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
            let first = !self.connected.load(Ordering::Relaxed);
            return Err(master_error(&self.destination, status, &text, first));
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

/// 64-bit FNV-1a: stable across builds, unlike the std hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Asks masters named by older devlanes versions (`m-<pid>-<n>`, persisting
/// forever) to exit, and removes their logs. Stable `m-<hash>` masters are left
/// for reuse; they expire after [`CONTROL_PERSIST`].
async fn sweep_legacy_masters(dir: &Path, ssh_program: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("m-") else {
            continue;
        };
        let legacy = rest
            .trim_end_matches(".log")
            .split_once('-')
            .is_some_and(|(pid, n)| {
                pid.bytes().all(|b| b.is_ascii_digit()) && n.bytes().all(|b| b.is_ascii_digit())
            });
        if !legacy {
            continue;
        }
        if name.ends_with(".log") {
            let _ = tokio::fs::remove_file(entry.path()).await;
            continue;
        }
        let _ = tokio::process::Command::new(ssh_program)
            .arg("-S")
            .arg(entry.path())
            .args(["-O", "exit", "--", "none"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        let _ = tokio::fs::remove_file(entry.path()).await;
    }
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
    Ok(SshStream::new(stdout, stdin, exit_rx, stop_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::askpass::AskpassConfig;
    use std::time::Duration;

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
    async fn dns_failures_are_permanent_only_before_the_first_connection()
    -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir()?;
            let mut options = SshOptions::new(dir.path());
            options.ssh_program = fake_ssh(
                dir.path(),
                "ssh: Could not resolve hostname box: nodename nor servname provided",
            )?;
            let transport = SshTransport::new("box", options);
            let Err(first) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(first.is_permanent(), "a typo should stop the link");
            // After a tunnel once opened, the same failure is a network change.
            transport.connected.store(true, Ordering::Relaxed);
            let Err(later) = transport.connect().await else {
                panic!("fake ssh unexpectedly connected");
            };
            assert!(
                !later.is_permanent(),
                "a reconnect after DNS loss must keep retrying"
            );
            Ok(())
        })
        .await?
    }

    #[test]
    fn control_paths_are_stable_and_short() {
        let options = SshOptions::new("/tmp/h");
        let one = SshTransport::new("box", options.clone()).control_paths();
        let again = SshTransport::new("box", options.clone()).control_paths();
        let other = SshTransport::new("other", options).control_paths();
        assert_eq!(one, again, "a restarted daemon finds the same master");
        assert_ne!(one.0, other.0);
        let name = one.0.file_name().map(|n| n.to_string_lossy().into_owned());
        assert_eq!(name.map(|n| n.len()), Some(18), "m- plus 16 hex digits");
    }

    #[tokio::test]
    async fn legacy_masters_are_swept_and_stable_ones_kept()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let exits = dir.path().join("exits");
        let fake = dir.path().join("fake-ssh");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$2\" >> '{}'\nexit 0\n", exits.display()),
        )?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
        let control = dir.path().join("ssh");
        std::fs::create_dir(&control)?;
        for name in ["m-123-0", "m-123-0.log", "m-0123456789abcdef", "a-1-0.sock"] {
            std::fs::write(control.join(name), b"")?;
        }
        sweep_legacy_masters(&control, &fake).await;
        assert!(!control.join("m-123-0").exists());
        assert!(!control.join("m-123-0.log").exists());
        assert!(
            control.join("m-0123456789abcdef").exists(),
            "reusable master kept"
        );
        let asked = std::fs::read_to_string(&exits)?;
        assert!(
            asked.contains("m-123-0"),
            "legacy master asked to exit: {asked}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn long_control_paths_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let deep = dir.path().join("x".repeat(80));
        let transport = SshTransport::new("box", SshOptions::new(deep));
        let result = transport.launch_master().await;
        assert!(matches!(result, Err(SshError::ControlPathTooLong(_))));
        Ok(())
    }
}
