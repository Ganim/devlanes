//! Daemon-side calls to the installed helper through `sudo -n`.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
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
    #[error("the hosts helper is not installed at {}; run `devlanes trust --hosts`", path.display())]
    HelperMissing {
        /// Expected path.
        path: PathBuf,
    },
    /// sudo would not run the helper without a password, or is not installed.
    #[error(
        "sudo will not run the hosts helper without a password ({message}); run `devlanes trust --hosts`"
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
        "the installed hosts helper speaks protocol {found}, this devlanes needs {PROTOCOL}; run `devlanes trust --hosts` again"
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
        }
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

/// Runs the installed helper. Cheap to clone.
#[derive(Clone)]
pub struct HostsClient {
    executor: Arc<dyn Executor>,
    fs: Arc<dyn FsProbe>,
    helper: PathBuf,
    sudo: String,
    tld: Option<String>,
    suffixes: Suffixes,
    timeout: Duration,
}

impl std::fmt::Debug for HostsClient {
    /// Paths and TLD; the executor is opaque.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostsClient")
            .field("helper", &self.helper)
            .field("sudo", &self.sudo)
            .field("tld", &self.tld)
            .finish_non_exhaustive()
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
        let suffixes = match tld {
            Some(tld) => Suffixes::new(Some(tld)).map_err(|error| HostsError::tld(tld, error))?,
            None => Suffixes::new(None).map_err(|error| HostsError::tld("localhost", error))?,
        };
        Ok(Self {
            executor,
            fs,
            helper: PathBuf::from(INSTALL_PATH),
            sudo: SUDO.to_owned(),
            tld: tld.filter(|tld| *tld != "localhost").map(str::to_owned),
            suffixes,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Uses another helper path (tests).
    #[must_use]
    pub fn with_helper(mut self, path: impl Into<PathBuf>) -> Self {
        self.helper = path.into();
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

    /// Helper path.
    pub fn helper(&self) -> &Path {
        &self.helper
    }

    /// `sudo -n <helper> apply [--tld <tld>]` with one name per line on stdin.
    pub fn apply_command(&self, names: &BTreeSet<String>) -> CommandSpec {
        let mut stdin = Vec::new();
        for name in names {
            stdin.extend_from_slice(name.as_bytes());
            stdin.push(b'\n');
        }
        let mut command = self.sudo_command("apply");
        if let Some(tld) = &self.tld {
            command = command.args(["--tld", tld.as_str()]);
        }
        command.stdin(stdin)
    }

    /// `sudo -n <helper> clear`.
    pub fn clear_command(&self) -> CommandSpec {
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

    /// Sets the block to `names`, which must already be filtered.
    ///
    /// Invalid names fail before sudo runs, so a bug never reaches the helper.
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
        self.privileged(&self.apply_command(names)).await
    }

    /// Removes the block.
    pub async fn clear(&self) -> Result<Applied, HostsError> {
        self.privileged(&self.clear_command()).await
    }

    /// The current block as printed by `devlanes-hosts status`; no sudo needed.
    pub async fn status(&self) -> Result<String, HostsError> {
        self.ensure_installed()?;
        let command = CommandSpec::new(self.helper.to_string_lossy()).arg("status");
        let output = self.run(&command).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(helper_failure(&output))
        }
    }

    /// Runs `devlanes-hosts version` without sudo and checks the protocol.
    pub async fn version(&self) -> Result<HelperVersion, HostsError> {
        self.ensure_installed()?;
        let command = CommandSpec::new(self.helper.to_string_lossy()).arg("version");
        let output = self.run(&command).await?;
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
    fn ensure_installed(&self) -> Result<(), HostsError> {
        match self.fs.lstat(&self.helper) {
            Ok(Some(info)) if info.kind == FileKind::File => Ok(()),
            _ => Err(HostsError::HelperMissing {
                path: self.helper.clone(),
            }),
        }
    }

    /// Checks installation, runs `command` and classifies the result.
    async fn privileged(&self, command: &CommandSpec) -> Result<Applied, HostsError> {
        self.ensure_installed()?;
        let output = self.run(command).await?;
        classify(&output)
    }

    /// Runs `command` under the timeout.
    async fn run(&self, command: &CommandSpec) -> Result<CommandOutput, HostsError> {
        match tokio::time::timeout(self.timeout, self.executor.run(command)).await {
            Err(_) => Err(HostsError::Timeout(self.timeout)),
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
fn parse_applied(stdout: &str) -> Option<Applied> {
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
            assert_eq!(
                client.apply_command(&BTreeSet::new()).argv(),
                ["sudo", "-n", "/usr/local/libexec/devlanes-hosts", "apply"]
            );
        }
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
