//! `devlanes certs trust --hosts` and `devlanes certs untrust --hosts`: the exact commands that
//! install and remove the helper and its sudo rule.
//!
//! Plans are plain data so tests can compare argv; [`install`] and [`uninstall`] run
//! them through an [`Executor`] in the user's terminal, where sudo can prompt.
//!
//! The source binary and the staged rule are user-writable, so neither is trusted
//! after it has been checked. The helper is read once into memory and copied to a
//! private temp file; that copy is the one `version` checks and `install` copies.
//! Both files land under root-owned dot-names first, and before anything is moved
//! into place their contents are read back and compared with the checked bytes and
//! the intended rule. sudo ignores `sudoers.d` entries containing a dot, so a staged
//! rule is never read before it is renamed. A failure after activation starts puts
//! the previous helper and rule back.

use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
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

/// Copy of the previous helper while the new one is activated.
const HELPER_BACKUP: &str = "/usr/local/libexec/.devlanes-hosts.old";

/// Temp name for the rule; the dot keeps sudo from reading it.
const SUDOERS_STAGING: &str = "/etc/sudoers.d/.devlanes.new";

/// Copy of the previous rule while the new one is activated; also ignored by sudo.
const SUDOERS_BACKUP: &str = "/etc/sudoers.d/.devlanes.old";

/// Name of the helper binary next to `devlanes`.
pub const HELPER_BINARY: &str = "devlanes-hosts";

/// Environment variable naming a helper binary to install instead of the sibling.
pub const HELPER_SOURCE_ENV: &str = "DEVLANES_HOSTS_HELPER";

const INSTALL: &str = "/usr/bin/install";
const VISUDO: &str = "/usr/sbin/visudo";
const MV: &str = "/bin/mv";
const CP: &str = "/bin/cp";
const RM: &str = "/bin/rm";
const CAT: &str = "/bin/cat";
/// macOS ships `test` only in `/bin`; Linux coreutils installs it in `/usr/bin`,
/// which merged-`/usr` systems also reach as `/bin`.
const TEST: &str = if cfg!(target_os = "macos") {
    "/bin/test"
} else {
    "/usr/bin/test"
};
const ID: &str = "/usr/bin/id";

/// Every absolute program the install and uninstall plans run as root; each must
/// exist on the platform this binary was built for.
pub const SYSTEM_TOOLS: [&str; 8] = [INSTALL, VISUDO, MV, CP, RM, CAT, TEST, ID];

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
    /// A root-owned staged copy differs from what was checked; nothing was activated.
    #[error(
        "{} does not match the checked {what}; it changed during the install, so nothing was activated",
        path.display()
    )]
    Mismatch {
        /// The staged copy.
        path: PathBuf,
        /// `helper` or `sudo rule`.
        what: &'static str,
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
    /// Local I/O (private copies, `lstat`).
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
            Self::Mismatch { .. } => "hosts_install_mismatch",
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

/// `sudo /bin/rm -f -- <paths>`.
fn remove(paths: &[&str]) -> CommandSpec {
    sudo(RM, &[&["-f", "--"][..], paths].concat())
}

/// `sudo /bin/mv -f -- <from> <to>`.
fn rename(from: &str, to: &str) -> CommandSpec {
    sudo(MV, &["-f", "--", from, to])
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
        "# Installed by `devlanes certs trust --hosts`; remove with `devlanes certs untrust --hosts`.\n\
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
    /// Absolute path of the checked private copy of `devlanes-hosts`.
    pub helper_source: PathBuf,
    /// Absolute path of a private file holding [`sudoers_contents`].
    pub sudoers_staging: PathBuf,
    /// Whether [`INSTALL_DIR`] must be created; an existing one is left as is.
    pub create_install_dir: bool,
    /// Whether a helper is installed now; it is backed up and restored on failure.
    pub previous_helper: bool,
    /// Whether a rule is installed now; it is backed up and restored on failure.
    pub previous_rule: bool,
}

/// The commands `devlanes certs trust --hosts` runs, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// Copies both files to root-owned dot-names and checks the rule's syntax.
    /// Nothing that sudo or the daemon uses changes here.
    pub stage: Vec<Step>,
    /// Prints the staged rule, which users may not be able to read, for comparison.
    pub read_rule: CommandSpec,
    /// Removes the staged copies when a content comparison fails.
    pub discard: Vec<CommandSpec>,
    /// Backs up the previous files, then moves the rule and, last, the helper into
    /// place; each step's rollback restores the previous state.
    pub activate: Vec<Step>,
    /// Puts the previous helper and rule back (or removes the new ones) when a check
    /// after activation fails.
    pub restore: Vec<CommandSpec>,
    /// Removes the backups once the install has been checked.
    pub cleanup: Vec<CommandSpec>,
    /// Run last without a password; success proves the rule is in effect.
    pub verify: CommandSpec,
}

/// `sudo /bin/test -e /etc/sudoers.d/devlanes` (`/usr/bin/test` on Linux): exit 0
/// when a rule is installed.
/// Users often cannot list `/etc/sudoers.d` (Debian: `0750`), so this needs sudo.
pub fn rule_exists_command() -> CommandSpec {
    sudo(TEST, &["-e", SUDOERS_PATH])
}

/// Builds the install plan.
pub fn install_plan(inputs: &InstallInputs) -> Result<InstallPlan, InstallError> {
    validate_user(&inputs.user)?;
    let group = inputs.platform.root_group();
    let source = absolute_arg(&inputs.helper_source)?;
    let staging = absolute_arg(&inputs.sudoers_staging)?;

    let discard = vec![remove(&[HELPER_STAGING, SUDOERS_STAGING])];
    let drop_backups = remove(&[HELPER_BACKUP, SUDOERS_BACKUP]);
    let restore_rule = if inputs.previous_rule {
        rename(SUDOERS_BACKUP, SUDOERS_PATH)
    } else {
        remove(&[SUDOERS_PATH])
    };
    let restore_helper = if inputs.previous_helper {
        rename(HELPER_BACKUP, INSTALL_PATH)
    } else {
        remove(&[INSTALL_PATH])
    };
    // After the rule moved: put the old rule back, then drop staging and backups.
    let undo_rule: Vec<CommandSpec> = [restore_rule.clone()]
        .into_iter()
        .chain(discard.iter().cloned())
        .chain([drop_backups.clone()])
        .collect();

    let mut stage = Vec::new();
    if inputs.create_install_dir {
        stage.push(Step::new(
            "creating /usr/local/libexec",
            sudo(
                INSTALL,
                &[
                    "-d",
                    "-o",
                    "root",
                    "-g",
                    group,
                    "-m",
                    "0755",
                    "--",
                    INSTALL_DIR,
                ],
            ),
        ));
    }
    stage.extend([
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
                    "--",
                    &source,
                    HELPER_STAGING,
                ],
            ),
        )
        .rollback(discard.clone()),
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
                    "--",
                    &staging,
                    SUDOERS_STAGING,
                ],
            ),
        )
        .rollback(discard.clone()),
        Step::new(
            "validating the sudo rule",
            sudo(VISUDO, &["-c", "-f", SUDOERS_STAGING]),
        )
        .rollback(discard.clone()),
    ]);

    let backout: Vec<CommandSpec> = discard
        .iter()
        .cloned()
        .chain([drop_backups.clone()])
        .collect();
    let mut activate = Vec::new();
    if inputs.previous_rule {
        activate.push(
            Step::new(
                "backing up the previous sudo rule",
                sudo(CP, &["-p", "--", SUDOERS_PATH, SUDOERS_BACKUP]),
            )
            .rollback(backout.clone()),
        );
    }
    if inputs.previous_helper {
        activate.push(
            Step::new(
                "backing up the previous helper",
                sudo(CP, &["-p", "--", INSTALL_PATH, HELPER_BACKUP]),
            )
            .rollback(backout.clone()),
        );
    }
    activate.extend([
        Step::new(
            "moving the sudo rule into place",
            rename(SUDOERS_STAGING, SUDOERS_PATH),
        )
        .rollback(undo_rule.clone()),
        Step::new(
            "validating the full sudo configuration",
            sudo(VISUDO, &["-c"]),
        )
        .rollback(undo_rule.clone()),
        Step::new(
            "moving the helper into place",
            rename(HELPER_STAGING, INSTALL_PATH),
        )
        .rollback(undo_rule),
    ]);

    Ok(InstallPlan {
        stage,
        read_rule: sudo(CAT, &["--", SUDOERS_STAGING]),
        discard,
        activate,
        restore: vec![restore_rule, restore_helper, drop_backups.clone()],
        cleanup: vec![drop_backups],
        // -k ignores the password just typed, so only a working NOPASSWD rule passes.
        verify: CommandSpec::new(SUDO).args(["-n", "-k", INSTALL_PATH, "version"]),
    })
}

/// The commands `devlanes certs untrust --hosts` runs. Clearing the block needs the helper,
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
    steps.push(Step::new("removing the sudo rule", remove(&[SUDOERS_PATH])));
    steps.push(Step::new("removing the helper", remove(&[INSTALL_PATH])));
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

/// Absolute UTF-8 path for argv. A relative program name would be looked up on
/// `PATH`, and a relative argument could start with `-`.
fn absolute_arg(path: &Path) -> Result<String, InstallError> {
    let absolute = std::path::absolute(path)
        .map_err(|error| InstallError::io(format!("resolving {}", path.display()), error))?;
    path_arg(&absolute)
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
    check_binary(executor, source, source).await
}

/// [`check_source`] running `binary` but naming `shown` in errors, so a check of
/// the private copy reports the path the user chose.
async fn check_binary(
    executor: &dyn Executor,
    binary: &Path,
    shown: &Path,
) -> Result<HelperVersion, InstallError> {
    let rejected = |reason: String| InstallError::SourceRejected {
        path: shown.to_owned(),
        reason,
    };
    let command = CommandSpec::new(absolute_arg(binary)?).arg("version");
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
        run_best_effort(executor, &step.rollback).await;
        return Err(failure);
    }
    Ok(warnings)
}

/// Runs cleanup commands, ignoring failures; the error that triggered them is the
/// one worth reporting.
async fn run_best_effort(executor: &dyn Executor, commands: &[CommandSpec]) {
    for command in commands {
        let _ = executor.run(command).await;
    }
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

/// Whether a rule is installed, asked through sudo (the first prompt, if any).
async fn rule_exists(executor: &dyn Executor) -> Result<bool, InstallError> {
    let command = rule_exists_command();
    let output = executor
        .run(&command)
        .await
        .map_err(|error| InstallError::io("checking for a previous sudo rule", error))?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) if output.stderr.trim().is_empty() => Ok(false),
        code => Err(InstallError::StepFailed {
            purpose: "checking for a previous sudo rule",
            command: command.to_string(),
            code,
            stderr: output.stderr.trim().to_owned(),
        }),
    }
}

/// Compares the root-owned staged copies with the checked helper bytes and the
/// intended rule. Both staged files are root-owned in root-owned directories, so
/// what matches here is what gets activated.
async fn check_staged(
    executor: &dyn Executor,
    fs: &dyn FsProbe,
    plan: &InstallPlan,
    helper: &[u8],
    rule: &str,
) -> Result<(), InstallError> {
    let staged_helper = fs
        .read(Path::new(HELPER_STAGING))
        .map_err(|error| InstallError::io(format!("reading {HELPER_STAGING}"), error))?;
    if staged_helper != helper {
        return Err(InstallError::Mismatch {
            path: HELPER_STAGING.into(),
            what: "helper",
        });
    }
    let output = executor
        .run(&plan.read_rule)
        .await
        .map_err(|error| InstallError::io(format!("reading {SUDOERS_STAGING}"), error))?;
    if !output.success() || output.stdout != rule {
        return Err(InstallError::Mismatch {
            path: SUDOERS_STAGING.into(),
            what: "sudo rule",
        });
    }
    Ok(())
}

/// Installs the helper and the rule for the current user.
///
/// Order: user and directory checks; the source is read once into memory, copied to
/// a private temp directory, and that copy is checked with `version`; the rule is
/// written next to it; then the staging steps (sudo prompts once; its timestamp
/// covers the rest), a comparison of both root-owned staged copies with the checked
/// content, activation with backups, an ownership check, and a passwordless
/// `sudo -n -k` run proving the rule works. Failures before activation leave the
/// previous install untouched; failures during or right after activation restore it.
pub async fn install(
    executor: &dyn Executor,
    fs: &dyn FsProbe,
    helper_source: &Path,
) -> Result<HelperVersion, InstallError> {
    let platform = Platform::current().ok_or(InstallError::Unsupported)?;
    let user = current_user(executor).await?;
    let install_dir_exists = check_install_dirs(fs)?;
    let previous_helper = matches!(
        fs.lstat(Path::new(INSTALL_PATH)),
        Ok(Some(info)) if info.kind == FileKind::File
    );
    let source = std::path::absolute(helper_source).map_err(|error| {
        InstallError::io(format!("resolving {}", helper_source.display()), error)
    })?;
    let rule = sudoers_contents(&user)?;
    let private = PrivateCopies::create(&source, &rule).await?;
    let version = check_binary(executor, &private.helper, &source).await?;
    // The copy is the user's own file; reading it back proves the binary that ran
    // `version` still holds the bytes that will be compared after staging.
    if private.read_helper().await? != private.helper_bytes {
        return Err(InstallError::SourceRejected {
            path: source,
            reason: "its private copy changed while it was being checked".into(),
        });
    }
    let previous_rule = rule_exists(executor).await?;
    let plan = install_plan(&InstallInputs {
        platform,
        user,
        helper_source: private.helper.clone(),
        sudoers_staging: private.rule.clone(),
        create_install_dir: !install_dir_exists,
        previous_helper,
        previous_rule,
    })?;
    run_steps(executor, &plan.stage).await?;
    if let Err(error) = check_staged(executor, fs, &plan, &private.helper_bytes, &rule).await {
        run_best_effort(executor, &plan.discard).await;
        return Err(error);
    }
    run_steps(executor, &plan.activate).await?;
    if let Err(error) = verify_installed(fs) {
        run_best_effort(executor, &plan.restore).await;
        return Err(error);
    }
    run_best_effort(executor, &plan.cleanup).await;
    drop(private);
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

/// A `0700` temp directory holding the private helper copy and the staged rule.
/// Removed on drop.
struct PrivateCopies {
    _dir: tempfile::TempDir,
    helper: PathBuf,
    helper_bytes: Vec<u8>,
    rule: PathBuf,
}

impl PrivateCopies {
    /// Reads `source` once and writes the copy (`0700`) and the rule (`0600`) on the
    /// blocking pool. Files are created exclusively, so nothing pre-existing is reused.
    async fn create(source: &Path, rule: &str) -> Result<Self, InstallError> {
        let source = source.to_owned();
        let rule = rule.to_owned();
        tokio::task::spawn_blocking(move || -> Result<Self, InstallError> {
            let mut helper_bytes = Vec::new();
            std::fs::File::open(&source)
                .and_then(|mut file| file.read_to_end(&mut helper_bytes))
                .map_err(|error| {
                    InstallError::io(format!("reading {}", source.display()), error)
                })?;
            let dir = tempfile::Builder::new()
                .prefix("devlanes-hosts-install-")
                .tempdir()
                .map_err(|error| InstallError::io("creating a private temp directory", error))?;
            let helper = dir.path().join(HELPER_BINARY);
            let rule_path = dir.path().join("sudoers");
            write_new(&helper, &helper_bytes, 0o700)?;
            write_new(&rule_path, rule.as_bytes(), 0o600)?;
            Ok(Self {
                _dir: dir,
                helper,
                helper_bytes,
                rule: rule_path,
            })
        })
        .await
        .map_err(|error| InstallError::io("copying the helper", io::Error::other(error)))?
    }

    /// The private helper copy as it is now.
    async fn read_helper(&self) -> Result<Vec<u8>, InstallError> {
        let path = self.helper.clone();
        tokio::fs::read(&path)
            .await
            .map_err(|error| InstallError::io(format!("reading {}", path.display()), error))
    }
}

/// Creates `path` exclusively with `mode` and writes `bytes`.
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), InstallError> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.flush()))
        .map_err(|error| InstallError::io(format!("writing {}", path.display()), error))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;
    use crate::fake::{FakeExecutor, FakeFs};

    fn inputs(platform: Platform, create_install_dir: bool) -> InstallInputs {
        InstallInputs {
            platform,
            user: "alice".into(),
            helper_source: "/tmp/devlanes-hosts-install-x/devlanes-hosts".into(),
            sudoers_staging: "/tmp/devlanes-hosts-install-x/sudoers".into(),
            create_install_dir,
            previous_helper: false,
            previous_rule: false,
        }
    }

    fn argvs(steps: &[Step]) -> Vec<String> {
        steps.iter().map(|step| step.command.to_string()).collect()
    }

    fn strings(commands: &[CommandSpec]) -> Vec<String> {
        commands.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn sudoers_snapshot() {
        assert_eq!(
            sudoers_contents("alice").expect("rule"),
            "# Installed by `devlanes certs trust --hosts`; remove with `devlanes certs untrust --hosts`.\n\
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
            argvs(&plan.stage),
            [
                "sudo /usr/bin/install -d -o root -g wheel -m 0755 -- /usr/local/libexec",
                "sudo /usr/bin/install -o root -g wheel -m 0755 -- /tmp/devlanes-hosts-install-x/devlanes-hosts /usr/local/libexec/.devlanes-hosts.new",
                "sudo /usr/bin/install -o root -g wheel -m 0440 -- /tmp/devlanes-hosts-install-x/sudoers /etc/sudoers.d/.devlanes.new",
                "sudo /usr/sbin/visudo -c -f /etc/sudoers.d/.devlanes.new",
            ]
        );
        assert_eq!(
            plan.read_rule.to_string(),
            "sudo /bin/cat -- /etc/sudoers.d/.devlanes.new"
        );
        assert_eq!(
            strings(&plan.discard),
            [
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.new /etc/sudoers.d/.devlanes.new"
            ]
        );
        assert_eq!(
            argvs(&plan.activate),
            [
                "sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.new /etc/sudoers.d/devlanes",
                "sudo /usr/sbin/visudo -c",
                "sudo /bin/mv -f -- /usr/local/libexec/.devlanes-hosts.new /usr/local/libexec/devlanes-hosts",
            ]
        );
        assert_eq!(
            plan.verify.to_string(),
            "sudo -n -k /usr/local/libexec/devlanes-hosts version"
        );
        // Without a previous install, undoing means removing the new files.
        assert_eq!(
            strings(&plan.activate[1].rollback),
            [
                "sudo /bin/rm -f -- /etc/sudoers.d/devlanes",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.new /etc/sudoers.d/.devlanes.new",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.old /etc/sudoers.d/.devlanes.old",
            ]
        );
        assert_eq!(
            strings(&plan.restore)[..2],
            [
                "sudo /bin/rm -f -- /etc/sudoers.d/devlanes",
                "sudo /bin/rm -f -- /usr/local/libexec/devlanes-hosts",
            ]
        );
    }

    #[test]
    fn upgrades_back_up_and_restore_the_previous_install() {
        let plan = install_plan(&InstallInputs {
            previous_helper: true,
            previous_rule: true,
            ..inputs(Platform::Macos, false)
        })
        .expect("plan");
        assert_eq!(
            argvs(&plan.activate),
            [
                "sudo /bin/cp -p -- /etc/sudoers.d/devlanes /etc/sudoers.d/.devlanes.old",
                "sudo /bin/cp -p -- /usr/local/libexec/devlanes-hosts /usr/local/libexec/.devlanes-hosts.old",
                "sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.new /etc/sudoers.d/devlanes",
                "sudo /usr/sbin/visudo -c",
                "sudo /bin/mv -f -- /usr/local/libexec/.devlanes-hosts.new /usr/local/libexec/devlanes-hosts",
            ]
        );
        assert_eq!(
            strings(&plan.activate[3].rollback)[0],
            "sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.old /etc/sudoers.d/devlanes"
        );
        assert_eq!(
            strings(&plan.restore),
            [
                "sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.old /etc/sudoers.d/devlanes",
                "sudo /bin/mv -f -- /usr/local/libexec/.devlanes-hosts.old /usr/local/libexec/devlanes-hosts",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.old /etc/sudoers.d/.devlanes.old",
            ]
        );
        assert_eq!(
            strings(&plan.cleanup),
            [
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.old /etc/sudoers.d/.devlanes.old"
            ]
        );
    }

    #[test]
    fn linux_plan_uses_group_root_and_keeps_an_existing_dir() {
        let plan = install_plan(&inputs(Platform::Linux, false)).expect("plan");
        let commands = argvs(&plan.stage);
        assert_eq!(commands.len(), 3);
        assert!(commands[0].starts_with("sudo /usr/bin/install -o root -g root -m 0755 -- "));
        assert!(commands[1].starts_with("sudo /usr/bin/install -o root -g root -m 0440 -- "));
        assert!(commands.iter().all(|command| !command.contains("wheel")));
    }

    #[test]
    fn relative_paths_become_absolute() {
        let plan = install_plan(&InstallInputs {
            helper_source: "-rf".into(),
            ..inputs(Platform::Linux, false)
        })
        .expect("plan");
        let source = &plan.stage[0].command.args[8];
        assert!(source.starts_with('/'), "{source}");
        assert_eq!(plan.stage[0].command.args[7], "--");
    }

    #[test]
    /// A path that does not exist fails under sudo as "command not found", so every
    /// tool must be where this platform keeps it.
    fn system_tools_exist_on_this_platform() {
        let missing: Vec<&str> = SYSTEM_TOOLS
            .into_iter()
            .filter(|tool| !Path::new(tool).is_file())
            .collect();
        assert!(missing.is_empty(), "missing on this OS: {missing:?}");
    }

    #[test]
    fn uninstall_plan_snapshot() {
        assert_eq!(
            argvs(&uninstall_plan(true)),
            [
                "sudo /usr/local/libexec/devlanes-hosts clear",
                "sudo /bin/rm -f -- /etc/sudoers.d/devlanes",
                "sudo /bin/rm -f -- /usr/local/libexec/devlanes-hosts",
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
        // A relative name would be looked up on PATH; it is resolved first.
        assert!(
            check_source(&release, Path::new("devlanes-hosts"))
                .await
                .is_ok()
        );
        let argvs = release.argvs();
        assert_eq!(argvs[0], ["/x/devlanes-hosts", "version"]);
        assert!(argvs[1][0].starts_with('/'), "{:?}", argvs[1]);
    }

    #[tokio::test]
    async fn a_failed_stage_step_rolls_back_and_stops() {
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
        let error = run_steps(&executor, &plan.stage)
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
        let ran = strings(&executor.calls());
        assert_eq!(
            ran[ran.len() - 2..],
            [
                "sudo /usr/sbin/visudo -c -f /etc/sudoers.d/.devlanes.new",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.new /etc/sudoers.d/.devlanes.new",
            ]
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

    /// What a fake install run sees and does.
    #[derive(Default)]
    struct World {
        /// Bytes `install` copied into the helper staging path.
        staged_helper: Option<Vec<u8>>,
        /// Text `install` copied into the rule staging path.
        staged_rule: String,
        /// Whether a previous rule exists.
        previous_rule: bool,
        /// Bytes appended to the staged helper, as if swapped mid-install.
        tamper_helper: bool,
        /// Text appended to the staged rule, as if edited mid-install.
        tamper_rule: bool,
        /// Makes the full `visudo -c` fail.
        full_check_fails: bool,
        /// Rewrites the private copy while `version` runs.
        tamper_private_copy: bool,
        /// Rewrites the original source while `version` runs.
        rewrite_source: Option<PathBuf>,
    }

    /// A fake executor and filesystem acting out the plan against `world`.
    fn fake_install(world: Arc<Mutex<World>>) -> (FakeExecutor, Arc<FakeFs>) {
        let fs = Arc::new(FakeFs::standard());
        let executor = {
            let fs = fs.clone();
            FakeExecutor::new(move |command| {
                let argv = command.argv();
                let mut world = world.lock().unwrap_or_else(PoisonError::into_inner);
                Ok(match argv.as_slice() {
                    [ID, "-un"] => CommandOutput::ok("alice\n"),
                    [binary, "version"] => {
                        if world.tamper_private_copy {
                            std::fs::write(binary, b"swapped")?;
                        }
                        if let Some(source) = &world.rewrite_source {
                            std::fs::write(source, b"rebuilt after the check")?;
                        }
                        CommandOutput::ok(version_line("release"))
                    }
                    [SUDO, TEST, "-e", SUDOERS_PATH] => {
                        if world.previous_rule {
                            CommandOutput::ok("")
                        } else {
                            CommandOutput::failed(1, "")
                        }
                    }
                    [SUDO, INSTALL, "-d", ..] => {
                        fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
                        CommandOutput::ok("")
                    }
                    [SUDO, INSTALL, .., "--", source, HELPER_STAGING] => {
                        let mut bytes = std::fs::read(source)?;
                        if world.tamper_helper {
                            bytes.extend_from_slice(b"evil");
                        }
                        fs.write(HELPER_STAGING, bytes.clone());
                        world.staged_helper = Some(bytes);
                        CommandOutput::ok("")
                    }
                    [SUDO, INSTALL, .., "--", source, SUDOERS_STAGING] => {
                        world.staged_rule = std::fs::read_to_string(source)?;
                        if world.tamper_rule {
                            world
                                .staged_rule
                                .push_str("alice ALL=(ALL) NOPASSWD: ALL\n");
                        }
                        CommandOutput::ok("")
                    }
                    [SUDO, CAT, "--", SUDOERS_STAGING] => {
                        CommandOutput::ok(world.staged_rule.clone())
                    }
                    [SUDO, VISUDO, "-c"] if world.full_check_fails => {
                        CommandOutput::failed(1, "parse error in /etc/sudoers")
                    }
                    [SUDO, MV, "-f", "--", HELPER_STAGING, INSTALL_PATH] => {
                        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
                        CommandOutput::ok("")
                    }
                    [SUDO, MV, "-f", "--", SUDOERS_STAGING, SUDOERS_PATH] => {
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
        (executor, fs)
    }

    /// A helper binary in a temp dir; the dir must outlive the install.
    fn source() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devlanes-hosts");
        std::fs::write(&path, b"release helper bytes").expect("source");
        (dir, path)
    }

    fn ran(executor: &FakeExecutor) -> Vec<String> {
        strings(&executor.calls())
    }

    #[tokio::test]
    async fn full_install_with_fakes() {
        let world = Arc::new(Mutex::new(World::default()));
        let (executor, fs) = fake_install(world.clone());
        let (_dir, source) = source();
        let version = install(&executor, fs.as_ref(), &source)
            .await
            .expect("install");
        assert_eq!(version.build, "release");
        let world = world.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            world.staged_helper.as_deref(),
            Some(&b"release helper bytes"[..])
        );
        assert_eq!(world.staged_rule, sudoers_contents("alice").expect("rule"));
        let ran = ran(&executor);
        // The checked binary is the private copy, never the user's path.
        assert!(
            !ran.iter()
                .any(|command| command.contains(&*source.to_string_lossy()))
        );
        assert!(ran[1].ends_with("/devlanes-hosts version"), "{}", ran[1]);
        let private_dir = ran[1]
            .strip_suffix("/devlanes-hosts version")
            .expect("private copy");
        let group = Platform::current().expect("platform").root_group();
        assert_eq!(
            ran[2..],
            [
                &*format!("sudo {TEST} -e /etc/sudoers.d/devlanes"),
                &format!(
                    "sudo /usr/bin/install -d -o root -g {group} -m 0755 -- /usr/local/libexec"
                ),
                &format!(
                    "sudo /usr/bin/install -o root -g {group} -m 0755 -- {private_dir}/devlanes-hosts /usr/local/libexec/.devlanes-hosts.new"
                ),
                &format!(
                    "sudo /usr/bin/install -o root -g {group} -m 0440 -- {private_dir}/sudoers /etc/sudoers.d/.devlanes.new"
                ),
                "sudo /usr/sbin/visudo -c -f /etc/sudoers.d/.devlanes.new",
                "sudo /bin/cat -- /etc/sudoers.d/.devlanes.new",
                "sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.new /etc/sudoers.d/devlanes",
                "sudo /usr/sbin/visudo -c",
                "sudo /bin/mv -f -- /usr/local/libexec/.devlanes-hosts.new /usr/local/libexec/devlanes-hosts",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.old /etc/sudoers.d/.devlanes.old",
                "sudo -n -k /usr/local/libexec/devlanes-hosts version",
            ]
        );
    }

    #[tokio::test]
    async fn a_source_rebuilt_after_the_check_does_not_reach_root() {
        let (_dir, source) = source();
        let world = Arc::new(Mutex::new(World {
            rewrite_source: Some(source.clone()),
            ..World::default()
        }));
        let (executor, fs) = fake_install(world.clone());
        install(&executor, fs.as_ref(), &source)
            .await
            .expect("install");
        let world = world.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            world.staged_helper.as_deref(),
            Some(&b"release helper bytes"[..])
        );
    }

    #[tokio::test]
    async fn a_private_copy_changed_during_the_check_is_refused() {
        let world = Arc::new(Mutex::new(World {
            tamper_private_copy: true,
            ..World::default()
        }));
        let (executor, fs) = fake_install(world);
        let (_dir, source) = source();
        let error = install(&executor, fs.as_ref(), &source)
            .await
            .expect_err("tampered copy");
        assert_eq!(error.code(), "helper_source_rejected");
        assert!(
            !ran(&executor)
                .iter()
                .any(|command| command.starts_with("sudo /usr/bin/install"))
        );
    }

    #[tokio::test]
    async fn a_swapped_staged_helper_is_discarded_before_activation() {
        let world = Arc::new(Mutex::new(World {
            tamper_helper: true,
            ..World::default()
        }));
        let (executor, fs) = fake_install(world);
        let (_dir, source) = source();
        let error = install(&executor, fs.as_ref(), &source)
            .await
            .expect_err("mismatch");
        assert!(
            matches!(&error, InstallError::Mismatch { what: "helper", .. }),
            "{error}"
        );
        let ran = ran(&executor);
        assert_eq!(
            ran.last().map(String::as_str),
            Some(
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.new /etc/sudoers.d/.devlanes.new"
            )
        );
        assert!(
            !ran.iter()
                .any(|command| command.starts_with("sudo /bin/mv"))
        );
    }

    #[tokio::test]
    async fn an_edited_staged_rule_is_discarded_before_activation() {
        let world = Arc::new(Mutex::new(World {
            tamper_rule: true,
            ..World::default()
        }));
        let (executor, fs) = fake_install(world);
        let (_dir, source) = source();
        let error = install(&executor, fs.as_ref(), &source)
            .await
            .expect_err("mismatch");
        assert!(
            matches!(
                &error,
                InstallError::Mismatch {
                    what: "sudo rule",
                    ..
                }
            ),
            "{error}"
        );
        assert!(
            !ran(&executor)
                .iter()
                .any(|command| command.starts_with("sudo /bin/mv"))
        );
    }

    #[tokio::test]
    async fn a_failing_full_check_restores_the_previous_rule() {
        let world = Arc::new(Mutex::new(World {
            previous_rule: true,
            full_check_fails: true,
            ..World::default()
        }));
        let (executor, fs) = fake_install(world);
        fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
        fs.write(INSTALL_PATH, b"old helper".to_vec());
        let (_dir, source) = source();
        let error = install(&executor, fs.as_ref(), &source)
            .await
            .expect_err("full check fails");
        assert!(
            matches!(
                error,
                InstallError::StepFailed {
                    purpose: "validating the full sudo configuration",
                    ..
                }
            ),
            "{error}"
        );
        let ran = ran(&executor);
        let tail = &ran[ran.len() - 4..];
        assert_eq!(
            tail,
            [
                "sudo /usr/sbin/visudo -c",
                "sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.old /etc/sudoers.d/devlanes",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.new /etc/sudoers.d/.devlanes.new",
                "sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.old /etc/sudoers.d/.devlanes.old",
            ]
        );
        // The helper in place was backed up but never replaced.
        assert!(!ran.iter().any(|command| {
            command.ends_with(".devlanes-hosts.new /usr/local/libexec/devlanes-hosts")
        }));
    }

    #[tokio::test]
    async fn an_ineffective_rule_is_reported() {
        let fs = FakeFs::standard();
        fs.set(INSTALL_DIR, FileKind::Dir, 0, 0o755);
        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
        fs.set(SUDOERS_PATH, FileKind::File, 0, 0o440);
        let fs = Arc::new(fs);
        let staged_rule = Arc::new(Mutex::new(String::new()));
        let executor = {
            let fs = fs.clone();
            FakeExecutor::new(move |command| {
                let argv = command.argv();
                let mut rule = staged_rule.lock().unwrap_or_else(PoisonError::into_inner);
                Ok(match argv.as_slice() {
                    [ID, "-un"] => CommandOutput::ok("alice\n"),
                    [_, "version"] => CommandOutput::ok(version_line("release")),
                    [SUDO, INSTALL, .., "--", source, HELPER_STAGING] => {
                        fs.write(HELPER_STAGING, std::fs::read(source)?);
                        CommandOutput::ok("")
                    }
                    [SUDO, INSTALL, .., "--", source, SUDOERS_STAGING] => {
                        *rule = std::fs::read_to_string(source)?;
                        CommandOutput::ok("")
                    }
                    [SUDO, CAT, ..] => CommandOutput::ok(rule.clone()),
                    [SUDO, "-n", "-k", ..] => {
                        CommandOutput::failed(1, "sudo: a password is required")
                    }
                    _ => CommandOutput::ok(""),
                })
            })
        };
        let (_dir, source) = source();
        let error = install(&executor, fs.as_ref(), &source)
            .await
            .expect_err("inactive");
        assert_eq!(error.code(), "sudo_rule_inactive");
    }
}
