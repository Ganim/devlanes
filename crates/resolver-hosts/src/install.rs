//! `devlanes trust --hosts` and `devlanes untrust --hosts`: the exact commands that
//! install and remove the helper and its sudo rule.
//!
//! Plans are plain data so tests can compare argv; [`install`] and [`uninstall`] run
//! them through an [`Executor`] in the user's terminal, where sudo can prompt.
//! Every file lands under a temporary dot-name first and is renamed into place:
//! sudo ignores `sudoers.d` entries containing a dot, so a half-written rule can never
//! be parsed, and the rule that `visudo` validated is the root-owned copy that gets
//! renamed, not the user-writable staging file.

use std::io;
use std::path::{Path, PathBuf};

use devlanes_hosts_helper::{INSTALL_PATH, PROTOCOL, SUDOERS_PATH};

use crate::client::{HelperVersion, SUDO};
use crate::exec::{CommandOutput, CommandSpec, Executor, FileKind, FsProbe};

/// Directory the helper is installed in.
pub const INSTALL_DIR: &str = "/usr/local/libexec";

/// Directory the sudo rule is installed in.
pub const SUDOERS_DIR: &str = "/etc/sudoers.d";

/// Temp name for the helper while it is copied.
const HELPER_STAGING: &str = "/usr/local/libexec/.devlanes-hosts.new";

/// Temp name for the rule; the dot keeps sudo from reading it.
const SUDOERS_STAGING: &str = "/etc/sudoers.d/.devlanes.new";

/// Name of the helper binary next to `devlanes`.
pub const HELPER_BINARY: &str = "devlanes-hosts";

/// Environment variable naming a helper binary to install instead of the sibling.
pub const HELPER_SOURCE_ENV: &str = "DEVLANES_HOSTS_HELPER";

const INSTALL: &str = "/usr/bin/install";
const VISUDO: &str = "/usr/sbin/visudo";
const MV: &str = "/bin/mv";
const RM: &str = "/bin/rm";
const ID: &str = "/usr/bin/id";

/// Platforms with an install layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS: group `wheel`.
    Macos,
    /// Linux: group `root`.
    Linux,
}

impl Platform {
    /// The platform this binary was built for, if supported.
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }

    /// Group for root-owned files.
    pub fn root_group(self) -> &'static str {
        match self {
            Self::Macos => "wheel",
            Self::Linux => "root",
        }
    }
}

/// Failure to install or remove the helper.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// No install layout for this OS.
    #[error("the hosts helper is only supported on macOS and Linux")]
    Unsupported,
    /// The user name cannot be written into a sudoers rule safely.
    #[error("user name {0:?} cannot be used in a sudoers rule")]
    InvalidUser(String),
    /// A directory on the install path is not root-owned or is writable by others.
    #[error("refusing to install: {} {reason}", path.display())]
    UnsafeDirectory {
        /// The directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// No helper binary to install.
    #[error("cannot find the devlanes-hosts binary (looked in {}); build it with `cargo build --release -p devlanes-hosts-helper` and set {HELPER_SOURCE_ENV} to its path", list(.0))]
    SourceMissing(Vec<PathBuf>),
    /// The helper binary to install is unsuitable.
    #[error("refusing to install {}: {reason}", path.display())]
    SourceRejected {
        /// The binary.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// A command of the plan failed; its rollback already ran.
    #[error("{purpose} failed: `{command}` exited with {code:?}: {stderr}")]
    StepFailed {
        /// What the step was for.
        purpose: &'static str,
        /// The command.
        command: String,
        /// Exit code.
        code: Option<i32>,
        /// Its stderr.
        stderr: String,
    },
    /// Everything was installed but `sudo -n` still asks for a password.
    #[error(
        "the sudo rule was installed but is not in effect ({stderr}); check that /etc/sudoers contains `#includedir /etc/sudoers.d` or `@includedir /etc/sudoers.d`"
    )]
    RuleInactive {
        /// sudo's message.
        stderr: String,
    },
    /// The installed files do not have the expected owner or mode.
    #[error("{} was installed with unexpected owner or mode", .0.display())]
    Unverified(PathBuf),
    /// Local I/O (staging file, `lstat`).
    #[error("{action}: {source}")]
    Io {
        /// What was being done.
        action: String,
        /// The error.
        #[source]
        source: io::Error,
    },
}

impl InstallError {
    /// Stable identifier for CLI error output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "hosts_unsupported",
            Self::InvalidUser(_) => "hosts_invalid_user",
            Self::UnsafeDirectory { .. } => "hosts_unsafe_directory",
            Self::SourceMissing(_) => "helper_source_missing",
            Self::SourceRejected { .. } => "helper_source_rejected",
            Self::StepFailed { .. } => "hosts_install_step_failed",
            Self::RuleInactive { .. } => "sudo_rule_inactive",
            Self::Unverified(_) => "hosts_install_unverified",
            Self::Io { .. } => "io",
        }
    }

    /// Builds an [`InstallError::Io`].
    fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }
}

/// Comma-separated paths for messages.
fn list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One command of a plan, with cleanup to run if it fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What the step does, for messages.
    pub purpose: &'static str,
    /// The command.
    pub command: CommandSpec,
    /// Commands run (best effort) when `command` fails, before stopping.
    pub rollback: Vec<CommandSpec>,
    /// Keep going when the command fails (uninstall only).
    pub continue_on_failure: bool,
}

impl Step {
    /// A step that stops the plan on failure.
    fn new(purpose: &'static str, command: CommandSpec) -> Self {
        Self {
            purpose,
            command,
            rollback: Vec::new(),
            continue_on_failure: false,
        }
    }

    /// Adds cleanup commands.
    fn rollback(mut self, commands: impl IntoIterator<Item = CommandSpec>) -> Self {
        self.rollback.extend(commands);
        self
    }
}

/// `sudo <program> <args>`.
fn sudo(program: &str, args: &[&str]) -> CommandSpec {
    CommandSpec::new(SUDO)
        .arg(program)
        .args(args.iter().copied())
}

/// `sudo /bin/rm -f <path>`.
fn remove(path: &str) -> CommandSpec {
    sudo(RM, &["-f", path])
}

/// Checks a user name before it goes into a sudoers rule.
///
/// Only POSIX portable characters, not starting with `-`, `.` or a digit-only
/// string; sudoers metacharacters (`,` `:` `=` `!` `%` `#` `\` whitespace) never pass.
pub fn validate_user(user: &str) -> Result<(), InstallError> {
    let ok = !user.is_empty()
        && user.len() <= 64
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !user.starts_with(['-', '.'])
        && !user.bytes().all(|b| b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(InstallError::InvalidUser(user.to_owned()))
    }
}

/// The sudoers rule: `user` may run the helper as root without a password.
///
/// A command without arguments in sudoers allows any arguments; the helper's own
/// parser is the gate, and release builds accept no path override.
pub fn sudoers_contents(user: &str) -> Result<String, InstallError> {
    validate_user(user)?;
    Ok(format!(
        "# Installed by `devlanes trust --hosts`; remove with `devlanes untrust --hosts`.\n\
         # Lets {user} run the devlanes /etc/hosts helper as root without a password.\n\
         # The helper only edits the block between `# devlanes begin` and `# devlanes end`.\n\
         Defaults!{INSTALL_PATH} !requiretty\n\
         {user} ALL=(root) NOPASSWD: {INSTALL_PATH}\n"
    ))
}

/// Inputs of an install plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInputs {
    /// Target layout.
    pub platform: Platform,
    /// Login name granted the rule.
    pub user: String,
    /// Release build of `devlanes-hosts` to copy.
    pub helper_source: PathBuf,
    /// User-owned file already holding [`sudoers_contents`].
    pub sudoers_staging: PathBuf,
    /// Whether [`INSTALL_DIR`] must be created; an existing one is left as is.
    pub create_install_dir: bool,
}

/// The commands `devlanes trust --hosts` runs, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// Privileged steps.
    pub steps: Vec<Step>,
    /// Run last without a password; success proves the rule is in effect.
    pub verify: CommandSpec,
}

/// Builds the install plan.
pub fn install_plan(inputs: &InstallInputs) -> Result<InstallPlan, InstallError> {
    validate_user(&inputs.user)?;
    let group = inputs.platform.root_group();
    let source = path_arg(&inputs.helper_source)?;
    let staging = path_arg(&inputs.sudoers_staging)?;
    let mut steps = Vec::new();
    if inputs.create_install_dir {
        steps.push(Step::new(
            "creating /usr/local/libexec",
            sudo(
                INSTALL,
                &["-d", "-o", "root", "-g", group, "-m", "0755", INSTALL_DIR],
            ),
        ));
    }
    steps.extend([
        Step::new(
            "copying the helper",
            sudo(
                INSTALL,
                &[
                    "-o",
                    "root",
                    "-g",
                    group,
                    "-m",
                    "0755",
                    &source,
                    HELPER_STAGING,
                ],
            ),
        )
        .rollback([remove(HELPER_STAGING)]),
        Step::new(
            "moving the helper into place",
            sudo(MV, &["-f", HELPER_STAGING, INSTALL_PATH]),
        )
        .rollback([remove(HELPER_STAGING)]),
        Step::new(
            "copying the sudo rule",
            sudo(
                INSTALL,
                &[
                    "-o",
                    "root",
                    "-g",
                    group,
                    "-m",
                    "0440",
                    &staging,
                    SUDOERS_STAGING,
                ],
            ),
        )
        .rollback([remove(SUDOERS_STAGING)]),
        Step::new(
            "validating the sudo rule",
            sudo(VISUDO, &["-c", "-f", SUDOERS_STAGING]),
        )
        .rollback([remove(SUDOERS_STAGING)]),
        Step::new(
            "moving the sudo rule into place",
            sudo(MV, &["-f", SUDOERS_STAGING, SUDOERS_PATH]),
        )
        .rollback([remove(SUDOERS_STAGING)]),
        Step::new(
            "validating the full sudo configuration",
            sudo(VISUDO, &["-c"]),
        )
        .rollback([remove(SUDOERS_PATH)]),
    ]);
    Ok(InstallPlan {
        steps,
        // -k ignores the password just typed, so only a working NOPASSWD rule passes.
        verify: CommandSpec::new(SUDO).args(["-n", "-k", INSTALL_PATH, "version"]),
    })
}

/// The commands `devlanes untrust --hosts` runs. Clearing the block needs the helper,
/// so it is skipped when the helper is gone; removals always run.
pub fn uninstall_plan(helper_installed: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    if helper_installed {
        let mut clear = Step::new(
            "removing the devlanes block from /etc/hosts",
            sudo(INSTALL_PATH, &["clear"]),
        );
        clear.continue_on_failure = true;
        steps.push(clear);
    }
    steps.push(Step::new("removing the sudo rule", remove(SUDOERS_PATH)));
    steps.push(Step::new("removing the helper", remove(INSTALL_PATH)));
    steps
}

/// UTF-8 path for argv.
fn path_arg(path: &Path) -> Result<String, InstallError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        InstallError::io(
            format!("using {}", path.display()),
            io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8"),
        )
    })
}

/// Checks a directory that will hold a root-run file.
fn check_dir(fs: &dyn FsProbe, path: &Path) -> Result<bool, InstallError> {
    let unsafe_dir = |reason| InstallError::UnsafeDirectory {
        path: path.to_owned(),
        reason,
    };
    let info = fs
        .lstat(path)
        .map_err(|error| InstallError::io(format!("checking {}", path.display()), error))?;
    let Some(info) = info else {
        return Ok(false);
    };
    match info.kind {
        FileKind::Dir => {}
        FileKind::Symlink => return Err(unsafe_dir("is a symlink")),
        FileKind::File | FileKind::Other => return Err(unsafe_dir("is not a directory")),
    }
    if info.uid != 0 {
        return Err(unsafe_dir("is not owned by root"));
    }
    if info.mode & 0o022 != 0 {
        return Err(unsafe_dir("is writable by its group or by everyone"));
    }
    Ok(true)
}

/// Checks every existing directory from `/` down to [`INSTALL_DIR`], and
/// [`SUDOERS_DIR`]. Returns whether `INSTALL_DIR` exists.
///
/// If any directory on the path were writable by the user, they could swap the
/// helper after install and run anything as root through the NOPASSWD rule.
pub fn check_install_dirs(fs: &dyn FsProbe) -> Result<bool, InstallError> {
    let mut exists = true;
    for dir in Path::new(INSTALL_DIR)
        .ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if !exists {
            break;
        }
        exists = check_dir(fs, dir)?;
    }
    if !check_dir(fs, Path::new(SUDOERS_DIR))? {
        return Err(InstallError::UnsafeDirectory {
            path: SUDOERS_DIR.into(),
            reason: "does not exist, so sudo would not read a rule there",
        });
    }
    Ok(exists)
}

/// Finds the helper to install: `override_path` (from [`HELPER_SOURCE_ENV`]), else
/// `devlanes-hosts` next to the running `devlanes`, also after resolving symlinks
/// such as Homebrew's `bin` links.
pub fn locate_helper_source(
    fs: &dyn FsProbe,
    override_path: Option<PathBuf>,
    current_exe: &Path,
) -> Result<PathBuf, InstallError> {
    let mut candidates = Vec::new();
    if let Some(path) = override_path {
        candidates.push(path);
    } else {
        let resolved = std::fs::canonicalize(current_exe).ok();
        for exe in std::iter::once(current_exe.to_owned()).chain(resolved) {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(HELPER_BINARY);
                if !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
            }
        }
    }
    for candidate in &candidates {
        if let Ok(Some(info)) = fs.lstat(candidate)
            && matches!(info.kind, FileKind::File | FileKind::Symlink)
        {
            return Ok(candidate.clone());
        }
    }
    Err(InstallError::SourceMissing(candidates))
}

/// The login name, from `id -un` rather than `$USER`, which any parent can set.
pub async fn current_user(executor: &dyn Executor) -> Result<String, InstallError> {
    let command = CommandSpec::new(ID).arg("-un");
    let output = executor
        .run(&command)
        .await
        .map_err(|error| InstallError::io("running `id -un`", error))?;
    let user = output.stdout.trim().to_owned();
    if !output.success() || user.is_empty() {
        return Err(InstallError::io(
            "running `id -un`",
            io::Error::other(output.stderr.trim().to_owned()),
        ));
    }
    validate_user(&user)?;
    Ok(user)
}

/// Runs `<source> version` and refuses debug builds and other protocol versions.
///
/// Debug builds accept `--hosts-file`; installed behind NOPASSWD that would let the
/// user write any file as root, even with the helper's own `SUDO_UID` check.
pub async fn check_source(
    executor: &dyn Executor,
    source: &Path,
) -> Result<HelperVersion, InstallError> {
    let rejected = |reason: String| InstallError::SourceRejected {
        path: source.to_owned(),
        reason,
    };
    let command = CommandSpec::new(path_arg(source)?).arg("version");
    let output = executor
        .run(&command)
        .await
        .map_err(|error| rejected(format!("running it failed: {error}")))?;
    let version = HelperVersion::parse(output.stdout.trim())
        .filter(|_| output.success())
        .ok_or_else(|| rejected(format!("`version` printed {:?}", output.stdout.trim())))?;
    if version.protocol != PROTOCOL {
        return Err(rejected(format!(
            "it speaks protocol {}, this devlanes needs {PROTOCOL}",
            version.protocol
        )));
    }
    if version.build != "release" {
        return Err(rejected(format!(
            "it is a {} build; install a release build (`cargo build --release -p devlanes-hosts-helper`)",
            version.build
        )));
    }
    Ok(version)
}

/// Runs `steps`; a failing step runs its rollback and stops the plan unless it
/// allows failure. Returns the messages of failures that were allowed.
pub async fn run_steps(
    executor: &dyn Executor,
    steps: &[Step],
) -> Result<Vec<String>, InstallError> {
    let mut warnings = Vec::new();
    for step in steps {
        let output = executor
            .run(&step.command)
            .await
            .unwrap_or_else(|error| CommandOutput {
                code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            });
        if output.success() {
            continue;
        }
        let failure = InstallError::StepFailed {
            purpose: step.purpose,
            command: step.command.to_string(),
            code: output.code,
            stderr: output.stderr.trim().to_owned(),
        };
        if step.continue_on_failure {
            warnings.push(failure.to_string());
            continue;
        }
        for cleanup in &step.rollback {
            // Best effort; the original failure is the one worth reporting.
            let _ = executor.run(cleanup).await;
        }
        return Err(failure);
    }
    Ok(warnings)
}

/// Checks the installed helper and, when readable, the rule have root ownership and
/// the planned modes. `/etc/sudoers.d` is often not searchable by users (Debian:
/// 0750), so an unreadable rule is not an error; the verify command covers it.
pub fn verify_installed(fs: &dyn FsProbe) -> Result<(), InstallError> {
    check_install_dirs(fs)?;
    let expect = |path: &str, mode: u32, required: bool| -> Result<(), InstallError> {
        match fs.lstat(Path::new(path)) {
            Ok(Some(info)) if info.kind == FileKind::File && info.uid == 0 && info.mode == mode => {
                Ok(())
            }
            Err(error) if !required && error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
            _ => Err(InstallError::Unverified(path.into())),
        }
    };
    expect(INSTALL_PATH, 0o755, true)?;
    expect(SUDOERS_PATH, 0o440, false)
}

/// Installs the helper and the rule for the current user.
///
/// Order: user and directory checks, source check, staging, privileged steps (sudo
/// prompts once; its timestamp covers the rest), ownership check, then a
/// passwordless `sudo -n -k` run proving the rule works.
pub async fn install(
    executor: &dyn Executor,
    fs: &dyn FsProbe,
    helper_source: &Path,
) -> Result<HelperVersion, InstallError> {
    let platform = Platform::current().ok_or(InstallError::Unsupported)?;
    let user = current_user(executor).await?;
    let install_dir_exists = check_install_dirs(fs)?;
    let version = check_source(executor, helper_source).await?;
    let staging = stage_sudoers(sudoers_contents(&user)?).await?;
    let plan = install_plan(&InstallInputs {
        platform,
        user,
        helper_source: helper_source.to_owned(),
        sudoers_staging: staging.to_path_buf(),
        create_install_dir: !install_dir_exists,
    })?;
    let result = run_steps(executor, &plan.steps).await;
    // The staging copy is the user's own file; dropping it is enough cleanup.
    drop(staging);
    result?;
    verify_installed(fs)?;
    let output = executor
        .run(&plan.verify)
        .await
        .map_err(|error| InstallError::io("running `sudo -n`", error))?;
    if !output.success() {
        return Err(InstallError::RuleInactive {
            stderr: output.stderr.trim().to_owned(),
        });
    }
    Ok(version)
}

/// Removes the block, the rule and the helper.
pub async fn uninstall(
    executor: &dyn Executor,
    fs: &dyn FsProbe,
) -> Result<Vec<String>, InstallError> {
    let helper_installed = matches!(
        fs.lstat(Path::new(INSTALL_PATH)),
        Ok(Some(info)) if info.kind == FileKind::File
    );
    run_steps(executor, &uninstall_plan(helper_installed)).await
}

/// Writes the rule to a private temp file on the blocking pool.
async fn stage_sudoers(contents: String) -> Result<tempfile::TempPath, InstallError> {
    tokio::task::spawn_blocking(move || -> io::Result<tempfile::TempPath> {
        use std::io::Write as _;
        let mut file = tempfile::Builder::new()
            .prefix("devlanes-sudoers-")
            .tempfile()?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        Ok(file.into_temp_path())
    })
    .await
    .map_err(|error| InstallError::io("staging the sudo rule", io::Error::other(error)))?
    .map_err(|error| InstallError::io("staging the sudo rule", error))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::fake::{FakeExecutor, FakeFs};

    fn inputs(platform: Platform, create_install_dir: bool) -> InstallInputs {
        InstallInputs {
            platform,
            user: "alice".into(),
            helper_source: "/opt/src/devlanes/target/release/devlanes-hosts".into(),
            sudoers_staging: "/tmp/devlanes-sudoers-x".into(),
            create_install_dir,
        }
    }

    fn argvs(steps: &[Step]) -> Vec<String> {
        steps.iter().map(|step| step.command.to_string()).collect()
    }

    #[test]
    fn sudoers_snapshot() {
        assert_eq!(
            sudoers_contents("alice").expect("rule"),
            "# Installed by `devlanes trust --hosts`; remove with `devlanes untrust --hosts`.\n\
             # Lets alice run the devlanes /etc/hosts helper as root without a password.\n\
             # The helper only edits the block between `# devlanes begin` and `# devlanes end`.\n\
             Defaults!/usr/local/libexec/devlanes-hosts !requiretty\n\
             alice ALL=(root) NOPASSWD: /usr/local/libexec/devlanes-hosts\n"
        );
    }

    #[test]
    fn user_names_that_could_inject_sudoers_syntax_are_refused() {
        for user in ["alice", "first.last", "a_b-c", "user1"] {
            assert!(validate_user(user).is_ok(), "{user}");
        }
        for user in [
            "",
            "-x",
            ".x",
            "1000",
            "a b",
            "a,b",
            "a:b",
            "a=b",
            "!a",
            "%admin",
            "#0",
            "a\\b",
            "a\nALL ALL=(ALL) NOPASSWD: ALL",
            "ALL=(ALL)",
        ] {
            assert!(validate_user(user).is_err(), "{user:?}");
        }
        assert!(sudoers_contents("a\nb").is_err());
    }

    #[test]
    fn macos_install_plan_snapshot() {
        let plan = install_plan(&inputs(Platform::Macos, true)).expect("plan");
        assert_eq!(
            argvs(&plan.steps),
            [
                "sudo /usr/bin/install -d -o root -g wheel -m 0755 /usr/local/libexec",
                "sudo /usr/bin/install -o root -g wheel -m 0755 /opt/src/devlanes/target/release/devlanes-hosts /usr/local/libexec/.devlanes-hosts.new",
                "sudo /bin/mv -f /usr/local/libexec/.devlanes-hosts.new /usr/local/libexec/devlanes-hosts",
                "sudo /usr/bin/install -o root -g wheel -m 0440 /tmp/devlanes-sudoers-x /etc/sudoers.d/.devlanes.new",
                "sudo /usr/sbin/visudo -c -f /etc/sudoers.d/.devlanes.new",
                "sudo /bin/mv -f /etc/sudoers.d/.devlanes.new /etc/sudoers.d/devlanes",
                "sudo /usr/sbin/visudo -c",
            ]
        );
        assert_eq!(
            plan.verify.to_string(),
            "sudo -n -k /usr/local/libexec/devlanes-hosts version"
        );
        let rollbacks: Vec<Vec<String>> = plan
            .steps
            .iter()
            .map(|step| step.rollback.iter().map(ToString::to_string).collect())
            .collect();
        assert_eq!(rollbacks[0], Vec::<String>::new());
        assert_eq!(
            rollbacks[4],
            ["sudo /bin/rm -f /etc/sudoers.d/.devlanes.new"]
        );
        assert_eq!(rollbacks[6], ["sudo /bin/rm -f /etc/sudoers.d/devlanes"]);
    }

    #[test]
    fn linux_plan_uses_group_root_and_keeps_an_existing_dir() {
        let plan = install_plan(&inputs(Platform::Linux, false)).expect("plan");
        let commands = argvs(&plan.steps);
        assert_eq!(commands.len(), 6);
        assert!(commands[0].starts_with("sudo /usr/bin/install -o root -g root -m 0755 "));
        assert!(commands[2].starts_with("sudo /usr/bin/install -o root -g root -m 0440 "));
        assert!(commands.iter().all(|command| !command.contains("wheel")));
    }

    #[test]
    fn uninstall_plan_snapshot() {
        assert_eq!(
            argvs(&uninstall_plan(true)),
            [
                "sudo /usr/local/libexec/devlanes-hosts clear",
                "sudo /bin/rm -f /etc/sudoers.d/devlanes",
                "sudo /bin/rm -f /usr/local/libexec/devlanes-hosts",
            ]
        );
        assert!(uninstall_plan(true)[0].continue_on_failure);
        assert_eq!(argvs(&uninstall_plan(false)).len(), 2);
    }

    #[test]
    fn directory_checks() {
        let fs = FakeFs::standard();
        assert!(!check_install_dirs(&fs).expect("standard"));
        fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
        assert!(check_install_dirs(&fs).expect("with libexec"));

        let cases: [(&str, FileKind, u32, u32, &str); 5] = [
            (
                "/usr/local",
                FileKind::Dir,
                501,
                0o755,
                "is not owned by root",
            ),
            (
                "/usr/local",
                FileKind::Dir,
                0,
                0o775,
                "is writable by its group or by everyone",
            ),
            (
                "/usr/local/libexec",
                FileKind::Dir,
                0,
                0o777,
                "is writable by its group or by everyone",
            ),
            (
                "/usr/local/libexec",
                FileKind::Symlink,
                0,
                0o777,
                "is a symlink",
            ),
            (
                "/etc/sudoers.d",
                FileKind::Dir,
                0,
                0o757,
                "is writable by its group or by everyone",
            ),
        ];
        for (path, kind, uid, mode, expected) in cases {
            let fs = FakeFs::standard();
            fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
            fs.set(path, kind, uid, mode);
            match check_install_dirs(&fs) {
                Err(InstallError::UnsafeDirectory {
                    path: found,
                    reason,
                }) => {
                    assert_eq!((found.as_path(), reason), (Path::new(path), expected));
                }
                other => panic!("{path}: expected refusal, got {other:?}"),
            }
        }

        let no_sudoers = FakeFs::standard();
        no_sudoers.remove(SUDOERS_DIR);
        assert!(matches!(
            check_install_dirs(&no_sudoers),
            Err(InstallError::UnsafeDirectory { .. })
        ));
        // A missing /usr/local is created by `install -d`, so it is not an error.
        let bare = FakeFs::standard();
        bare.remove("/usr/local");
        assert!(!check_install_dirs(&bare).expect("no /usr/local"));
    }

    #[test]
    fn locates_the_sibling_or_the_override() {
        let fs = FakeFs::default();
        fs.set(
            "/opt/devlanes/bin/devlanes-hosts",
            FileKind::File,
            501,
            0o755,
        );
        let found = locate_helper_source(&fs, None, Path::new("/opt/devlanes/bin/devlanes"))
            .expect("sibling");
        assert_eq!(found, Path::new("/opt/devlanes/bin/devlanes-hosts"));
        let missing = locate_helper_source(
            &fs,
            Some("/nope/devlanes-hosts".into()),
            Path::new("/opt/devlanes/bin/devlanes"),
        );
        assert!(
            matches!(missing, Err(InstallError::SourceMissing(paths)) if paths == [PathBuf::from("/nope/devlanes-hosts")])
        );
    }

    fn version_line(build: &str) -> String {
        format!("devlanes-hosts 0.1.0 protocol={PROTOCOL} build={build}\n")
    }

    #[tokio::test]
    async fn debug_builds_are_refused() {
        let executor = FakeExecutor::new(|_| Ok(CommandOutput::ok(version_line("debug"))));
        let error = check_source(&executor, Path::new("/x/devlanes-hosts"))
            .await
            .expect_err("debug");
        assert_eq!(error.code(), "helper_source_rejected");
        assert!(error.to_string().contains("debug build"), "{error}");
        let release = FakeExecutor::new(|_| Ok(CommandOutput::ok(version_line("release"))));
        assert!(
            check_source(&release, Path::new("/x/devlanes-hosts"))
                .await
                .is_ok()
        );
        assert_eq!(release.argvs(), [vec!["/x/devlanes-hosts", "version"]]);
    }

    #[tokio::test]
    async fn a_failed_step_rolls_back_and_stops() {
        let executor = FakeExecutor::new(|command| {
            Ok(
                if command.args.iter().any(|arg| arg == "-f")
                    && command.args.first().map(String::as_str) == Some(VISUDO)
                {
                    CommandOutput::failed(
                        1,
                        ">>> /etc/sudoers.d/.devlanes.new: syntax error near line 4 <<<",
                    )
                } else {
                    CommandOutput::ok("")
                },
            )
        });
        let plan = install_plan(&inputs(Platform::Macos, false)).expect("plan");
        let error = run_steps(&executor, &plan.steps)
            .await
            .expect_err("visudo fails");
        assert!(
            matches!(
                error,
                InstallError::StepFailed {
                    purpose: "validating the sudo rule",
                    ..
                }
            ),
            "{error}"
        );
        let ran: Vec<String> = executor.calls().iter().map(ToString::to_string).collect();
        assert_eq!(
            ran[ran.len() - 2..],
            [
                "sudo /usr/sbin/visudo -c -f /etc/sudoers.d/.devlanes.new",
                "sudo /bin/rm -f /etc/sudoers.d/.devlanes.new",
            ]
        );
        assert!(
            !ran.iter()
                .any(|command| command.contains("/etc/sudoers.d/devlanes"))
        );
    }

    #[tokio::test]
    async fn uninstall_continues_past_a_failed_clear() {
        let executor = FakeExecutor::new(|command| {
            Ok(if command.args.iter().any(|arg| arg == "clear") {
                CommandOutput::failed(4, "devlanes-hosts: bad markers")
            } else {
                CommandOutput::ok("")
            })
        });
        let fs = FakeFs::standard();
        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
        let warnings = uninstall(&executor, &fs).await.expect("uninstall");
        assert_eq!(warnings.len(), 1);
        assert_eq!(executor.calls().len(), 3);
    }

    #[tokio::test]
    async fn full_install_with_fakes() {
        let fs = Arc::new(FakeFs::standard());
        let staged = Arc::new(std::sync::Mutex::new(String::new()));
        let executor = {
            let fs = fs.clone();
            let staged = staged.clone();
            FakeExecutor::new(move |command| {
                let argv = command.argv();
                Ok(match argv.as_slice() {
                    [ID, "-un"] => CommandOutput::ok("alice\n"),
                    [_, "version"] => CommandOutput::ok(version_line("release")),
                    [SUDO, INSTALL, "-d", ..] => {
                        fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
                        CommandOutput::ok("")
                    }
                    [SUDO, INSTALL, .., source, SUDOERS_STAGING] => {
                        let text = std::fs::read_to_string(source).unwrap_or_default();
                        *staged
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
                        CommandOutput::ok("")
                    }
                    [SUDO, MV, "-f", HELPER_STAGING, INSTALL_PATH] => {
                        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
                        CommandOutput::ok("")
                    }
                    [SUDO, MV, "-f", SUDOERS_STAGING, SUDOERS_PATH] => {
                        fs.fail(SUDOERS_PATH, io::ErrorKind::PermissionDenied);
                        CommandOutput::ok("")
                    }
                    [SUDO, "-n", "-k", INSTALL_PATH, "version"] => {
                        CommandOutput::ok(version_line("release"))
                    }
                    _ => CommandOutput::ok(""),
                })
            })
        };
        let version = install(&executor, fs.as_ref(), Path::new("/x/devlanes-hosts"))
            .await
            .expect("install");
        assert_eq!(version.build, "release");
        assert_eq!(
            *staged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            sudoers_contents("alice").expect("rule")
        );
        // id, version, then the seven plan steps and the passwordless verify.
        assert_eq!(executor.calls().len(), 10);
    }

    #[tokio::test]
    async fn an_ineffective_rule_is_reported() {
        let fs = FakeFs::standard();
        fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
        fs.set(SUDOERS_PATH, FileKind::File, 0, 0o440);
        let executor = FakeExecutor::new(|command| {
            let argv = command.argv();
            Ok(match argv.as_slice() {
                [ID, "-un"] => CommandOutput::ok("alice\n"),
                [_, "version"] => CommandOutput::ok(version_line("release")),
                [SUDO, "-n", "-k", ..] => CommandOutput::failed(1, "sudo: a password is required"),
                _ => CommandOutput::ok(""),
            })
        });
        let error = install(&executor, &fs, Path::new("/x/devlanes-hosts"))
            .await
            .expect_err("inactive");
        assert_eq!(error.code(), "sudo_rule_inactive");
    }
}
