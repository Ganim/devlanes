//! Daemon-side calls to the hosts writer: the installed helper through `sudo -n`
//! on Unix, the `devlanes-hosts` service over its pipe on Windows. Both answer
//! with the helper's exit codes, so failures map to the same [`HostsError`]s.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use devlanes_hosts_helper::names::{self, MAX_NAMES, NameError, Suffixes};
use devlanes_hosts_helper::{INSTALL_PATH, PROTOCOL, exit};

use crate::exec::{CommandOutput, CommandSpec, Executor, FileKind, FsProbe};

/// sudo, looked up on `PATH` so layouts like NixOS's `/run/wrappers/bin` work.
pub const SUDO: &str = "sudo";

/// Upper bound for one helper run. `sudo -n` never prompts, so only a stuck
/// filesystem or cache flush gets near it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Failure to run the helper, with a stable [`HostsError::code`] for RPC replies.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostsError {
    /// The configured TLD is not one the helper accepts.
    #[error("cannot write .{tld} names to /etc/hosts: {reason}")]
    InvalidTld {
        /// The configured TLD.
        tld: String,
        /// Why the helper would refuse it.
        reason: &'static str,
    },
    /// The helper is not installed.
    #[error("the hosts helper is not installed at {}; run `devlanes certs trust --hosts`", path.display())]
    HelperMissing {
        /// Expected path.
        path: PathBuf,
    },
    /// sudo would not run the helper without a password, or is not installed.
    #[error(
        "sudo will not run the hosts helper without a password ({message}); run `devlanes certs trust --hosts`"
    )]
    SudoDenied {
        /// sudo's message.
        message: String,
    },
    /// The helper refused the names; a devlanes bug, since names are filtered first.
    #[error("the hosts helper refused the names: {message}")]
    InvalidNames {
        /// The helper's message.
        message: String,
    },
    /// The helper refused to edit `/etc/hosts` (symlink, malformed markers).
    #[error("the hosts helper refused to edit /etc/hosts: {message}")]
    HostsFileRefused {
        /// The helper's message.
        message: String,
    },
    /// The installed helper speaks another protocol version.
    #[error(
        "the installed hosts helper speaks protocol {found}, this devlanes needs {PROTOCOL}; run `devlanes certs trust --hosts` again"
    )]
    Outdated {
        /// What `version` reported.
        found: String,
    },
    /// Any other failure.
    #[error("the hosts helper failed (exit {code:?}): {message}")]
    HelperFailed {
        /// Exit code, `None` when killed or not started.
        code: Option<i32>,
        /// stderr or the I/O error.
        message: String,
    },
    /// The helper did not finish in time.
    #[error("the hosts helper did not finish within {0:?}")]
    Timeout(Duration),
    /// This platform has no hosts writer.
    #[error("the hosts file fallback is not supported on this platform")]
    Unsupported,
    /// Windows: nothing serves the hosts service's pipe (not installed, or stopped).
    #[error(
        "the devlanes hosts service is not running ({pipe} does not exist); run `devlanes certs trust --hosts`"
    )]
    ServiceMissing {
        /// The pipe looked for.
        pipe: String,
    },
    /// Windows: the hosts service refused this account, which is not the user
    /// who installed it (or is a sandboxed or low-integrity process of that user).
    #[error(
        "the devlanes hosts service refused this account ({message}); it serves the user who ran `devlanes certs trust --hosts`"
    )]
    ServiceDenied {
        /// The service's or the system's message.
        message: String,
    },
    /// Windows: the pipe is served by something other than a SYSTEM process, so
    /// it is not the hosts service; nothing was sent to it.
    #[error(
        "the process serving the hosts service's pipe runs as {owner}, not SYSTEM; refusing to use it (another program took the name; restart the devlanes-hosts service or reboot)"
    )]
    ServiceUntrusted {
        /// The account that owns the pipe.
        owner: String,
    },
}

impl HostsError {
    /// Stable identifier for RPC errors and `doctor`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidTld { .. } => "hosts_tld_unsupported",
            Self::HelperMissing { .. } => "helper_missing",
            Self::SudoDenied { .. } => "sudo_denied",
            Self::InvalidNames { .. } => "hosts_invalid_names",
            Self::HostsFileRefused { .. } => "hosts_file_refused",
            Self::Outdated { .. } => "helper_outdated",
            Self::HelperFailed { .. } => "helper_failed",
            Self::Timeout(_) => "helper_timeout",
            Self::Unsupported => "hosts_unsupported",
            Self::ServiceMissing { .. } => "service_missing",
            Self::ServiceDenied { .. } => "service_denied",
            Self::ServiceUntrusted { .. } => "service_untrusted",
        }
    }

    /// Whether the writer is not installed (`helper_missing`, `service_missing`).
    pub fn is_missing(&self) -> bool {
        matches!(
            self,
            Self::HelperMissing { .. } | Self::ServiceMissing { .. }
        )
    }

    /// Whether the writer refused this user, so retrying soon only repeats the
    /// refusal (and, on Unix, fills the auth log).
    pub fn is_denied(&self) -> bool {
        matches!(
            self,
            Self::SudoDenied { .. } | Self::ServiceDenied { .. } | Self::ServiceUntrusted { .. }
        )
    }

    /// From a helper-side validation error on the TLD.
    fn tld(tld: &str, error: NameError) -> Self {
        Self::InvalidTld {
            tld: tld.to_owned(),
            reason: error.reason,
        }
    }
}

/// Result of `apply` or `clear`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// Whether `/etc/hosts` was rewritten.
    pub changed: bool,
    /// Names in the block afterwards.
    pub names: usize,
}

/// `devlanes-hosts version` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperVersion {
    /// Crate version.
    pub version: String,
    /// Protocol number.
    pub protocol: u32,
    /// `release` or `debug`.
    pub build: String,
}

impl HelperVersion {
    /// Parses `devlanes-hosts <version> protocol=<n> build=<profile>`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut parts = line.split_whitespace();
        if parts.next()? != "devlanes-hosts" {
            return None;
        }
        let version = parts.next()?.to_owned();
        let protocol = parts.next()?.strip_prefix("protocol=")?.parse().ok()?;
        let build = parts.next()?.strip_prefix("build=")?.to_owned();
        Some(Self {
            version,
            protocol,
            build,
        })
    }
}

/// Runs the hosts writer: the installed helper through sudo, or on Windows the
/// hosts service over its pipe. Cheap to clone.
#[derive(Clone)]
pub struct HostsClient {
    backend: Backend,
    tld: Option<String>,
    suffixes: Suffixes,
    timeout: Duration,
}

/// How requests reach the writer.
#[derive(Clone)]
enum Backend {
    /// `sudo -n <helper> ...`.
    Sudo(Sudo),
    /// The `devlanes-hosts` service's pipe.
    #[cfg(windows)]
    Service(crate::pipe::PipeHostsClient),
}

/// The helper through sudo.
#[derive(Clone)]
struct Sudo {
    executor: Arc<dyn Executor>,
    fs: Arc<dyn FsProbe>,
    helper: PathBuf,
    sudo: String,
}

impl std::fmt::Debug for HostsClient {
    /// Paths and TLD; the executor is opaque.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("HostsClient");
        match &self.backend {
            Backend::Sudo(sudo) => debug
                .field("helper", &sudo.helper)
                .field("sudo", &sudo.sudo),
            #[cfg(windows)]
            Backend::Service(service) => debug.field("service", service),
        };
        debug.field("tld", &self.tld).finish_non_exhaustive()
    }
}

/// The suffixes for `tld`, failing like the writer would.
fn suffixes_for(tld: Option<&str>) -> Result<Suffixes, HostsError> {
    match tld {
        Some(tld) => Suffixes::new(Some(tld)).map_err(|error| HostsError::tld(tld, error)),
        None => Suffixes::new(None).map_err(|error| HostsError::tld("localhost", error)),
    }
}

impl HostsClient {
    /// A client for the helper at [`INSTALL_PATH`], allowing `.localhost` plus `tld`.
    ///
    /// Pass the user's `hostnames.tld` only with the `resolver` strategy. A TLD the
    /// helper would refuse (anything outside `localhost`, `test`, `internal` and
    /// `home.arpa`) fails here, before any sudo call.
    pub fn new(
        executor: Arc<dyn Executor>,
        fs: Arc<dyn FsProbe>,
        tld: Option<&str>,
    ) -> Result<Self, HostsError> {
        Ok(Self {
            backend: Backend::Sudo(Sudo {
                executor,
                fs,
                helper: PathBuf::from(INSTALL_PATH),
                sudo: SUDO.to_owned(),
            }),
            tld: tld.filter(|tld| *tld != "localhost").map(str::to_owned),
            suffixes: suffixes_for(tld)?,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// A client for the `devlanes-hosts` Windows service through `pipe`
    /// ([`crate::pipe::PipeHostsClient::system`] for the real one), allowing
    /// `.localhost` plus `tld` as [`HostsClient::new`] does.
    #[cfg(windows)]
    pub fn service(
        pipe: crate::pipe::PipeHostsClient,
        tld: Option<&str>,
    ) -> Result<Self, HostsError> {
        Ok(Self {
            backend: Backend::Service(pipe),
            tld: tld.filter(|tld| *tld != "localhost").map(str::to_owned),
            suffixes: suffixes_for(tld)?,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Uses another helper path (tests). Only the sudo client has one.
    #[must_use]
    pub fn with_helper(mut self, path: impl Into<PathBuf>) -> Self {
        match &mut self.backend {
            Backend::Sudo(sudo) => sudo.helper = path.into(),
            #[cfg(windows)]
            Backend::Service(_) => {}
        }
        self
    }

    /// Uses another per-run timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Suffixes names must sit under; pass them to [`crate::filter_names`].
    pub fn suffixes(&self) -> &Suffixes {
        &self.suffixes
    }

    /// Sets the block to `names`, which must already be filtered.
    ///
    /// Invalid names fail before sudo runs (or anything is sent to the service),
    /// so a bug never reaches the writer.
    pub async fn apply(&self, names: &BTreeSet<String>) -> Result<Applied, HostsError> {
        if names.len() > MAX_NAMES {
            return Err(HostsError::InvalidNames {
                message: format!("{} names exceed the cap of {MAX_NAMES}", names.len()),
            });
        }
        if let Some(error) = names
            .iter()
            .find_map(|name| names::validate_name(name, &self.suffixes).err())
        {
            return Err(HostsError::InvalidNames {
                message: error.to_string(),
            });
        }
        match &self.backend {
            Backend::Sudo(sudo) => {
                let command = sudo.apply_command(names, self.tld.as_deref());
                sudo.privileged(&command, self.timeout).await
            }
            #[cfg(windows)]
            Backend::Service(service) => {
                let request = devlanes_hosts_helper::protocol::Request::Apply {
                    tld: self.tld.clone(),
                    names: name_lines(names),
                };
                service.applied(&request, self.timeout).await
            }
        }
    }

    /// Removes the block.
    pub async fn clear(&self) -> Result<Applied, HostsError> {
        match &self.backend {
            Backend::Sudo(sudo) => sudo.privileged(&sudo.clear_command(), self.timeout).await,
            #[cfg(windows)]
            Backend::Service(service) => {
                let request = devlanes_hosts_helper::protocol::Request::Clear;
                service.applied(&request, self.timeout).await
            }
        }
    }

    /// The current block as printed by `devlanes-hosts status`; no sudo needed.
    pub async fn status(&self) -> Result<String, HostsError> {
        match &self.backend {
            Backend::Sudo(sudo) => sudo.status(self.timeout).await,
            #[cfg(windows)]
            Backend::Service(service) => service.status(self.timeout).await,
        }
    }

    /// Asks the writer for its version and checks the protocol: the helper's
    /// `version` without sudo, or the service's `version` request.
    pub async fn version(&self) -> Result<HelperVersion, HostsError> {
        match &self.backend {
            Backend::Sudo(sudo) => sudo.version(self.timeout).await,
            #[cfg(windows)]
            Backend::Service(service) => service.version(self.timeout).await,
        }
    }
}

/// One name per line, the helper's stdin format.
fn name_lines(names: &BTreeSet<String>) -> Vec<u8> {
    let mut lines = Vec::new();
    for name in names {
        lines.extend_from_slice(name.as_bytes());
        lines.push(b'\n');
    }
    lines
}

impl Sudo {
    /// `sudo -n <helper> apply [--tld <tld>]` with one name per line on stdin.
    fn apply_command(&self, names: &BTreeSet<String>, tld: Option<&str>) -> CommandSpec {
        let mut command = self.sudo_command("apply");
        if let Some(tld) = tld {
            command = command.args(["--tld", tld]);
        }
        command.stdin(name_lines(names))
    }

    /// `sudo -n <helper> clear`.
    fn clear_command(&self) -> CommandSpec {
        self.sudo_command("clear")
    }

    /// `sudo -n <helper> <subcommand>`; `-n` makes sudo fail instead of prompting,
    /// since the daemon has no terminal.
    fn sudo_command(&self, subcommand: &str) -> CommandSpec {
        CommandSpec::new(&self.sudo)
            .arg("-n")
            .arg(self.helper.to_string_lossy())
            .arg(subcommand)
    }

    /// `devlanes-hosts status`.
    async fn status(&self, timeout: Duration) -> Result<String, HostsError> {
        self.ensure_installed()?;
        let command = CommandSpec::new(self.helper.to_string_lossy()).arg("status");
        let output = self.run(&command, timeout).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(helper_failure(&output))
        }
    }

    /// Runs `devlanes-hosts version` without sudo and checks the protocol.
    async fn version(&self, timeout: Duration) -> Result<HelperVersion, HostsError> {
        self.ensure_installed()?;
        let command = CommandSpec::new(self.helper.to_string_lossy()).arg("version");
        let output = self.run(&command, timeout).await?;
        let found = output.stdout.trim();
        let version = HelperVersion::parse(found).ok_or_else(|| HostsError::Outdated {
            found: if found.is_empty() {
                "unknown".into()
            } else {
                found.into()
            },
        })?;
        if version.protocol != PROTOCOL {
            return Err(HostsError::Outdated {
                found: version.protocol.to_string(),
            });
        }
        Ok(version)
    }

    /// Fails with [`HostsError::HelperMissing`] unless the helper is a regular file.
    ///
    /// Off Unix, an `Unsupported` probe (what [`crate::SystemFs`] returns there) fails
    /// with [`HostsError::Unsupported`] instead, so every call says the fallback
    /// doesn't exist on this platform rather than asking for an install.
    fn ensure_installed(&self) -> Result<(), HostsError> {
        match self.fs.lstat(&self.helper) {
            Ok(Some(info)) if info.kind == FileKind::File => Ok(()),
            #[cfg(not(unix))]
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                Err(HostsError::Unsupported)
            }
            _ => Err(HostsError::HelperMissing {
                path: self.helper.clone(),
            }),
        }
    }

    /// Checks installation, runs `command` and classifies the result.
    async fn privileged(
        &self,
        command: &CommandSpec,
        timeout: Duration,
    ) -> Result<Applied, HostsError> {
        self.ensure_installed()?;
        let output = self.run(command, timeout).await?;
        classify(&output)
    }

    /// Runs `command` under `timeout`.
    async fn run(
        &self,
        command: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, HostsError> {
        match tokio::time::timeout(timeout, self.executor.run(command)).await {
            Err(_) => Err(HostsError::Timeout(timeout)),
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                Err(if command.program == self.sudo {
                    HostsError::SudoDenied {
                        message: "sudo is not installed".into(),
                    }
                } else {
                    HostsError::HelperMissing {
                        path: self.helper.clone(),
                    }
                })
            }
            Ok(Err(error)) => Err(HostsError::HelperFailed {
                code: None,
                message: error.to_string(),
            }),
        }
    }
}

/// The helper's message without its `devlanes-hosts: ` prefix.
fn message(output: &CommandOutput) -> String {
    let text = output.stderr.trim();
    text.lines()
        .map(|line| line.strip_prefix("devlanes-hosts: ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Maps a non-zero helper exit.
fn helper_failure(output: &CommandOutput) -> HostsError {
    let message = message(output);
    match output.code.and_then(|code| u8::try_from(code).ok()) {
        Some(exit::INVALID_NAMES) => HostsError::InvalidNames { message },
        Some(exit::REFUSED_FILE) => HostsError::HostsFileRefused { message },
        _ => HostsError::HelperFailed {
            code: output.code,
            message,
        },
    }
}

/// Classifies `sudo -n <helper> apply|clear` output.
///
/// The helper never exits 1, so exit 1 is sudo refusing (no NOPASSWD rule, user
/// not in sudoers, helper path not matching the rule).
pub fn classify(output: &CommandOutput) -> Result<Applied, HostsError> {
    match output.code {
        Some(0) => parse_applied(&output.stdout).ok_or_else(|| HostsError::HelperFailed {
            code: Some(0),
            message: format!("unexpected output {:?}", output.stdout.trim()),
        }),
        Some(1) => Err(HostsError::SudoDenied {
            message: output.stderr.trim().to_owned(),
        }),
        _ => Err(helper_failure(output)),
    }
}

/// Parses `changed names=N` or `unchanged names=N`.
pub(crate) fn parse_applied(stdout: &str) -> Option<Applied> {
    let line = stdout.lines().next()?;
    let (word, count) = line.split_once(' ')?;
    let changed = match word {
        "changed" => true,
        "unchanged" => false,
        _ => return None,
    };
    let names = count.strip_prefix("names=")?.parse().ok()?;
    Some(Applied { changed, names })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::FileKind;
    use crate::fake::{FakeExecutor, FakeFs};

    fn installed_fs() -> Arc<FakeFs> {
        let fs = FakeFs::standard();
        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
        Arc::new(fs)
    }

    fn client(executor: Arc<FakeExecutor>, tld: Option<&str>) -> HostsClient {
        HostsClient::new(executor, installed_fs(), tld).expect("client")
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[tokio::test]
    async fn apply_runs_sudo_non_interactively_with_names_on_stdin() {
        let executor = Arc::new(FakeExecutor::new(|_| {
            Ok(CommandOutput::ok("changed names=2\n"))
        }));
        let client = client(executor.clone(), Some("test"));
        let applied = client
            .apply(&names(&["b.acme.test", "a.acme.localhost"]))
            .await
            .expect("apply");
        assert_eq!(
            applied,
            Applied {
                changed: true,
                names: 2
            }
        );
        let calls = executor.calls();
        assert_eq!(
            calls[0].argv(),
            [
                "sudo",
                "-n",
                "/usr/local/libexec/devlanes-hosts",
                "apply",
                "--tld",
                "test"
            ]
        );
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some(&b"a.acme.localhost\nb.acme.test\n"[..])
        );
    }

    #[tokio::test]
    async fn localhost_needs_no_tld_argument() {
        let executor = Arc::new(FakeExecutor::new(|_| {
            Ok(CommandOutput::ok("unchanged names=0\n"))
        }));
        for tld in [None, Some("localhost")] {
            let client = client(executor.clone(), tld);
            client.clear().await.expect("clear");
            client.apply(&BTreeSet::new()).await.expect("apply");
        }
        assert_eq!(
            executor.argvs()[1],
            ["sudo", "-n", "/usr/local/libexec/devlanes-hosts", "apply"]
        );
        assert_eq!(
            executor.argvs()[0],
            ["sudo", "-n", "/usr/local/libexec/devlanes-hosts", "clear"]
        );
    }

    #[test]
    fn unsupported_tlds_fail_before_any_command() {
        let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
        let error =
            HostsClient::new(executor.clone(), installed_fs(), Some("dev")).expect_err("dev");
        assert_eq!(error.code(), "hosts_tld_unsupported");
        assert!(executor.calls().is_empty());
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn the_system_probe_reports_unsupported_without_running_anything() {
        let executor = Arc::new(FakeExecutor::new(|_| Ok(CommandOutput::ok(""))));
        let client =
            HostsClient::new(executor.clone(), Arc::new(crate::SystemFs), None).expect("client");
        let names = names(&["web.acme.localhost"]);
        assert_eq!(client.apply(&names).await, Err(HostsError::Unsupported));
        assert_eq!(client.clear().await, Err(HostsError::Unsupported));
        assert_eq!(client.version().await, Err(HostsError::Unsupported));
        assert!(executor.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_helper_is_reported_without_sudo() {
        let executor = Arc::new(FakeExecutor::new(|_| {
            Ok(CommandOutput::ok("changed names=1\n"))
        }));
        let client =
            HostsClient::new(executor.clone(), Arc::new(FakeFs::standard()), None).expect("client");
        let error = client
            .apply(&names(&["a.localhost"]))
            .await
            .expect_err("missing");
        assert_eq!(error.code(), "helper_missing");
        assert!(executor.calls().is_empty());
    }

    #[tokio::test]
    async fn invalid_names_never_reach_sudo() {
        let executor = Arc::new(FakeExecutor::new(|_| {
            Ok(CommandOutput::ok("changed names=1\n"))
        }));
        let client = client(executor.clone(), None);
        let error = client
            .apply(&names(&["*.a.localhost"]))
            .await
            .expect_err("wildcard");
        assert_eq!(error.code(), "hosts_invalid_names");
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn classifies_exits() {
        let cases = [
            (
                CommandOutput::failed(1, "sudo: a password is required\n"),
                "sudo_denied",
            ),
            (
                CommandOutput::failed(1, "sudo: alice is not in the sudoers file.\n"),
                "sudo_denied",
            ),
            (
                CommandOutput::failed(3, "devlanes-hosts: line 1: invalid name\n"),
                "hosts_invalid_names",
            ),
            (
                CommandOutput::failed(4, "devlanes-hosts: /etc/hosts is a symlink\n"),
                "hosts_file_refused",
            ),
            (
                CommandOutput::failed(5, "devlanes-hosts: io\n"),
                "helper_failed",
            ),
            (CommandOutput::ok("garbage"), "helper_failed"),
        ];
        for (output, code) in cases {
            assert_eq!(
                classify(&output).expect_err("error").code(),
                code,
                "{output:?}"
            );
        }
        let refused = classify(&CommandOutput::failed(4, "devlanes-hosts: bad markers\n"));
        assert_eq!(
            refused,
            Err(HostsError::HostsFileRefused {
                message: "bad markers".into()
            })
        );
        assert_eq!(
            classify(&CommandOutput::ok("unchanged names=7\n")),
            Ok(Applied {
                changed: false,
                names: 7
            })
        );
    }

    #[tokio::test]
    async fn missing_sudo_is_sudo_denied() {
        let executor = Arc::new(FakeExecutor::new(|_| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }));
        let error = client(executor, None).clear().await.expect_err("no sudo");
        assert_eq!(
            error,
            HostsError::SudoDenied {
                message: "sudo is not installed".into()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hung_helper_times_out() {
        struct Hang;
        #[async_trait::async_trait]
        impl Executor for Hang {
            async fn run(&self, _: &CommandSpec) -> io::Result<CommandOutput> {
                std::future::pending().await
            }
        }
        let client = HostsClient::new(Arc::new(Hang), installed_fs(), None)
            .expect("client")
            .with_timeout(Duration::from_secs(1));
        assert_eq!(
            client.clear().await,
            Err(HostsError::Timeout(Duration::from_secs(1)))
        );
    }

    #[tokio::test]
    async fn version_and_status_run_without_sudo() {
        let executor = Arc::new(FakeExecutor::new(|command| {
            Ok(match command.args.first().map(String::as_str) {
                Some("version") => CommandOutput::ok(format!(
                    "devlanes-hosts 0.1.0 protocol={PROTOCOL} build=release\n"
                )),
                _ => CommandOutput::ok("# devlanes begin\n# devlanes end\n"),
            })
        }));
        let client = client(executor.clone(), None);
        let version = client.version().await.expect("version");
        assert_eq!(version.build, "release");
        assert_eq!(
            client.status().await.expect("status"),
            "# devlanes begin\n# devlanes end\n"
        );
        assert_eq!(
            executor.argvs(),
            [
                vec!["/usr/local/libexec/devlanes-hosts", "version"],
                vec!["/usr/local/libexec/devlanes-hosts", "status"],
            ]
        );
    }

    #[tokio::test]
    async fn protocol_mismatch_is_outdated() {
        let executor = Arc::new(FakeExecutor::new(|_| {
            Ok(CommandOutput::ok(
                "devlanes-hosts 9.0.0 protocol=99 build=release\n",
            ))
        }));
        let error = client(executor, None)
            .version()
            .await
            .expect_err("outdated");
        assert_eq!(error, HostsError::Outdated { found: "99".into() });
    }
}
