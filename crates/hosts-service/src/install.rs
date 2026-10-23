//! Installing and removing the service. Runs in the elevated `devlanes.exe`
//! child that `devlanes certs trust --hosts` starts through the UAC prompt; never
//! in the service itself.
//!
//! The source binary sits in a user-writable directory, so it is read into
//! memory once and only those bytes are trusted: they are written to a staged
//! copy in the admin-only directory, the staged copy's `--version` is checked,
//! and the copy is read back and compared before it replaces anything. The
//! staged copy is owned by Administrators from its creation, whatever the
//! "default owner" policy says. A failure after the old binary was moved aside
//! puts it and the old running state back; a service this run created is
//! deleted.

use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use devlanes_hosts_helper::protocol::{PIPE_PROTOCOL, VersionLine, parse_version_line};
use devlanes_sys::windows::process::CREATE_NO_WINDOW;
use devlanes_sys::windows::security::{
    ADMIN_ONLY_FILE_SDDL, AdminOnlyProblem, SecurityDescriptor, check_admin_only,
    create_admin_only_dir, create_new_with_descriptor,
};
use devlanes_sys::windows::token::{self, Sid};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::config::{
    self, BACKUP_BINARY, DESCRIPTION, DISPLAY_NAME, SERVICE_BINARY, SERVICE_NAME, STAGED_BINARY,
    USER_SID_ARG,
};

/// Restarts after a failure: three, five seconds apart, counted over a day. A
/// process that held the pipe name while the service was down has usually let
/// go by then.
fn failure_actions() -> ServiceFailureActions {
    let restart = ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(5),
    };
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart.clone(), restart.clone(), restart]),
    }
}

/// Largest service binary accepted; a release build is a few MiB.
const MAX_BINARY_BYTES: u64 = 64 << 20;

/// Longest a start or stop may take.
const STATE_TIMEOUT: Duration = Duration::from_secs(30);

/// `ERROR_SERVICE_DOES_NOT_EXIST`.
const NO_SUCH_SERVICE: i32 = 1060;

/// `ERROR_SERVICE_NOT_ACTIVE`: stopping a stopped service.
const NOT_ACTIVE: i32 = 1062;

/// Failure to install or remove the service.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// This process is not elevated.
    #[error("installing the hosts service needs administrator rights")]
    NotElevated,
    /// The SID to serve is not a person's user account.
    #[error("{0} is not a user account's SID; the service serves one user account")]
    NotAnAccount(String),
    /// `%ProgramFiles%`, the install directory or the binary could be changed by
    /// others.
    #[error("refusing to install: {} {problem}", path.display())]
    Unsafe {
        /// The file or directory.
        path: PathBuf,
        /// What is wrong.
        problem: AdminOnlyProblem,
    },
    /// No binary at the source path.
    #[error("cannot read the service binary {}: {source}", path.display())]
    SourceMissing {
        /// The path given.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The source is not a service binary of this protocol.
    #[error("refusing to install {}: {reason}", path.display())]
    SourceRejected {
        /// The source path.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The staged copy differs from the bytes that were read.
    #[error("{} does not match the checked binary; nothing was activated", .0.display())]
    Mismatch(PathBuf),
    /// A service manager call failed.
    #[error("{action}: {source}")]
    Service {
        /// What was being done.
        action: &'static str,
        /// The error.
        #[source]
        source: windows_service::Error,
    },
    /// The service did not reach the expected state.
    #[error("the service did not {0}")]
    State(&'static str),
    /// Local file I/O.
    #[error("{action}: {source}")]
    Io {
        /// What was being done.
        action: String,
        /// The error.
        #[source]
        source: io::Error,
    },
}

/// The elevated installer's exit codes, from [`FIRST_EXIT_CODE`] on: the CLI
/// error code and message each stands for. The installer's console is hidden,
/// so its exit code is all the CLI learns.
const FAILURES: [(&str, &str); 8] = [
    (
        "hosts_install_not_elevated",
        "the installer did not run as administrator",
    ),
    (
        "hosts_install_user_rejected",
        "the service only serves a user account, not a built-in account or group",
    ),
    (
        "hosts_unsafe_directory",
        r"%ProgramFiles%, %ProgramFiles%\devlanes or the service binary can be changed by accounts other than administrators; fix their owner or access list as administrator",
    ),
    (
        "service_source_missing",
        "cannot read devlanes-hosts-service.exe next to devlanes.exe",
    ),
    (
        "service_source_rejected",
        "devlanes-hosts-service.exe next to devlanes.exe does not report this devlanes' pipe protocol; reinstall devlanes",
    ),
    (
        "hosts_install_mismatch",
        "the copied service binary did not match the one that was read; nothing was activated",
    ),
    (
        "hosts_install_step_failed",
        "the service could not be created, stopped or started; see `sc.exe query devlanes-hosts` and the System event log",
    ),
    (
        "io",
        r"reading or writing a file in %ProgramFiles%\devlanes failed",
    ),
];

/// Exit code of the first entry of [`FAILURES`]; clear of the 1 and 2 any
/// program may exit with.
const FIRST_EXIT_CODE: u8 = 10;

impl InstallError {
    /// Stable identifier for CLI error output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotElevated => "hosts_install_not_elevated",
            Self::NotAnAccount(_) => "hosts_install_user_rejected",
            Self::Unsafe { .. } => "hosts_unsafe_directory",
            Self::SourceMissing { .. } => "service_source_missing",
            Self::SourceRejected { .. } => "service_source_rejected",
            Self::Mismatch(_) => "hosts_install_mismatch",
            Self::Service { .. } | Self::State(_) => "hosts_install_step_failed",
            Self::Io { .. } => "io",
        }
    }

    /// The elevated installer's exit code for this failure.
    pub fn exit_code(&self) -> u8 {
        let index = FAILURES
            .iter()
            .position(|(code, _)| *code == self.code())
            .unwrap_or(FAILURES.len() - 1);
        FIRST_EXIT_CODE + index as u8
    }

    /// Builds an [`InstallError::Io`].
    fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }

    /// Builds an [`InstallError::Service`].
    fn service(action: &'static str) -> impl FnOnce(windows_service::Error) -> Self {
        move |source| Self::Service { action, source }
    }
}

/// The CLI error code and message the elevated installer's `exit` code stands
/// for, if it is one of [`InstallError::exit_code`]'s.
pub fn exit_failure(exit: u32) -> Option<(&'static str, &'static str)> {
    let index = exit.checked_sub(u32::from(FIRST_EXIT_CODE))?;
    FAILURES.get(index as usize).copied()
}

/// Where the files go.
#[derive(Debug, Clone)]
struct Layout {
    directory: PathBuf,
    binary: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
}

impl Layout {
    /// The paths under `%ProgramFiles%\devlanes`.
    fn new() -> Result<Self, InstallError> {
        let directory = config::install_dir()
            .map_err(|error| InstallError::io("finding the Program Files directory", error))?;
        Ok(Self {
            binary: directory.join(SERVICE_BINARY),
            staged: directory.join(STAGED_BINARY),
            backup: directory.join(BACKUP_BINARY),
            directory,
        })
    }
}

/// What a successful install put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The installed binary.
    pub path: PathBuf,
    /// Its `--version`.
    pub version: VersionLine,
}

/// The service as it was before this install, for the rollback.
struct Previous {
    /// The old binary was moved to the backup name.
    binary: bool,
    /// The new binary was moved into place.
    placed: bool,
    /// Whether the service existed before.
    existed: bool,
    /// Whether it was running.
    running: bool,
}

/// Checks that `path` (a directory when `is_dir`) is owned by SYSTEM,
/// Administrators or TrustedInstaller and that nobody else may change it: the
/// counterpart of the helper's "root-owned, not group or world writable".
fn admin_only(path: &Path, is_dir: bool) -> Result<(), InstallError> {
    match check_admin_only(path, is_dir) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(problem)) => Err(InstallError::Unsafe {
            path: path.to_owned(),
            problem,
        }),
        Err(error) => Err(InstallError::io(
            format!("checking {}", path.display()),
            error,
        )),
    }
}

/// Installs or updates the service for `user` from the binary at `source`, then
/// starts it. Must run elevated.
pub fn install(source: &Path, user: &Sid) -> Result<Installed, InstallError> {
    if !token::elevated().map_err(|error| InstallError::io("reading the elevation", error))? {
        return Err(InstallError::NotElevated);
    }
    let sid = user
        .to_sddl()
        .map_err(|error| InstallError::io("formatting the user SID", error))?;
    // The service would open its pipe to that SID.
    if !user.is_user_account() {
        return Err(InstallError::NotAnAccount(sid));
    }
    let layout = Layout::new()?;
    let program_files = layout.directory.parent().unwrap_or(&layout.directory);
    admin_only(program_files, true)?;
    create_admin_only_dir(&layout.directory).map_err(|error| {
        InstallError::io(format!("creating {}", layout.directory.display()), error)
    })?;
    admin_only(&layout.directory, true)?;
    let bytes = read_source(source)?;
    let version = stage(&layout, source, &bytes)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(InstallError::service("connecting to the service manager"))?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE;
    let existing = open_existing(&manager, access)?;
    let mut previous = Previous {
        binary: false,
        placed: false,
        existed: existing.is_some(),
        running: false,
    };
    if let Some(service) = &existing {
        previous.running = state(service)? != ServiceState::Stopped;
        stop(service)?;
    }
    match activate(&layout, &manager, existing, access, &sid, &mut previous) {
        Ok(()) => {
            let _ = fs::remove_file(&layout.backup);
            Ok(Installed {
                path: layout.binary,
                version,
            })
        }
        Err((error, service)) => {
            rollback(&layout, service.as_ref(), &previous);
            let _ = fs::remove_file(&layout.staged);
            Err(error)
        }
    }
}

/// Moves the staged binary into place, configures and starts the service. On
/// failure, returns the service handle (if any) for the rollback.
fn activate(
    layout: &Layout,
    manager: &ServiceManager,
    existing: Option<Service>,
    access: ServiceAccess,
    sid: &str,
    previous: &mut Previous,
) -> Result<(), (InstallError, Option<Service>)> {
    if layout.binary.exists() {
        let _ = fs::remove_file(&layout.backup);
        if let Err(error) = devlanes_sys::rename_retrying(&layout.binary, &layout.backup) {
            return Err((
                InstallError::io("moving the previous binary aside", error),
                existing,
            ));
        }
        previous.binary = true;
    }
    if let Err(error) = devlanes_sys::rename_retrying(&layout.staged, &layout.binary) {
        return Err((
            InstallError::io("moving the new binary into place", error),
            existing,
        ));
    }
    previous.placed = true;
    if let Err(error) = admin_only(&layout.binary, false) {
        return Err((error, existing));
    }
    let info = service_info(&layout.binary, sid);
    let service = match existing {
        Some(service) => match service.change_config(&info) {
            Ok(()) => service,
            Err(source) => {
                return Err((
                    InstallError::Service {
                        action: "updating the service",
                        source,
                    },
                    Some(service),
                ));
            }
        },
        None => match manager.create_service(&info, access) {
            Ok(service) => service,
            Err(source) => {
                return Err((
                    InstallError::Service {
                        action: "creating the service",
                        source,
                    },
                    None,
                ));
            }
        },
    };
    // A missing description only makes the Services console less helpful.
    let _ = service.set_description(DESCRIPTION);
    let started = service
        .update_failure_actions(failure_actions())
        .and_then(|()| service.set_failure_actions_on_non_crash_failures(true))
        .map_err(InstallError::service("setting the restart after failures"))
        .and_then(|()| {
            service
                .start::<&str>(&[])
                .map_err(InstallError::service("starting the service"))
        })
        .and_then(|()| wait_for(&service, ServiceState::Running, "start"));
    match started {
        Ok(()) => Ok(()),
        Err(error) => Err((error, Some(service))),
    }
}

/// Puts the previous binary and running state back; deletes a service this run
/// created. Best effort: every step runs.
fn rollback(layout: &Layout, service: Option<&Service>, previous: &Previous) {
    if let Some(service) = service {
        let _ = stop(service);
    }
    if previous.binary {
        // Over the new binary when it was placed, back to its own name otherwise.
        let _ = devlanes_sys::rename_retrying(&layout.backup, &layout.binary);
    } else if previous.placed {
        let _ = devlanes_sys::remove_file_retrying(&layout.binary);
    }
    if let Some(service) = service {
        if !previous.existed {
            let _ = service.delete();
        } else if previous.running {
            let _ = service.start::<&str>(&[]);
        }
    }
}

/// Removes the service and, best effort, its files, including leftover temp
/// files next to the hosts file. Clearing the block is the
/// caller's job (over the pipe, before this runs); so is noticing files left
/// in place.
pub fn uninstall() -> Result<(), InstallError> {
    if !token::elevated().map_err(|error| InstallError::io("reading the elevation", error))? {
        return Err(InstallError::NotElevated);
    }
    let layout = Layout::new()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(InstallError::service("connecting to the service manager"))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    if let Some(service) = open_existing(&manager, access)? {
        // A service that will not stop is still deleted once it does.
        let _ = stop(&service);
        service
            .delete()
            .map_err(InstallError::service("deleting the service"))?;
    }
    for path in [&layout.binary, &layout.backup, &layout.staged] {
        let _ = devlanes_sys::remove_file_retrying(path);
    }
    let _ = fs::remove_dir(&layout.directory);
    // Temp files a service stopped mid-write left in `etc`.
    if let Ok(hosts) = config::hosts_path() {
        crate::write::remove_leftover_temps(&hosts);
    }
    Ok(())
}

/// Whether the service is installed, and its state; `None` when it is not.
/// Needs no elevation.
pub fn service_state() -> Result<Option<ServiceState>, InstallError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(InstallError::service("connecting to the service manager"))?;
    match open_existing(&manager, ServiceAccess::QUERY_STATUS)? {
        Some(service) => Ok(Some(state(&service)?)),
        None => Ok(None),
    }
}

/// The service's settings: its own process as LocalSystem, started at boot,
/// serving `sid`. `windows-service` quotes the path because it contains spaces.
fn service_info(binary: &Path, sid: &str) -> ServiceInfo {
    ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: binary.to_owned(),
        launch_arguments: vec![USER_SID_ARG.into(), sid.into()],
        dependencies: Vec::new(),
        // None is LocalSystem.
        account_name: None,
        account_password: None,
    }
}

/// Reads the source once; only these bytes are trusted from here on.
fn read_source(source: &Path) -> Result<Vec<u8>, InstallError> {
    let reject = |reason: &str| InstallError::SourceRejected {
        path: source.to_owned(),
        reason: reason.to_owned(),
    };
    let mut bytes = Vec::new();
    File::open(source)
        .and_then(|file| file.take(MAX_BINARY_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| InstallError::SourceMissing {
            path: source.to_owned(),
            source: error,
        })?;
    if bytes.len() as u64 > MAX_BINARY_BYTES {
        return Err(reject("it is larger than 64 MiB"));
    }
    if !bytes.starts_with(b"MZ") {
        return Err(reject("it is not a Windows program"));
    }
    Ok(bytes)
}

/// Writes `bytes` to the staged name, created with [`ADMIN_ONLY_FILE_SDDL`]
/// (owned by Administrators from the first moment), runs its `--version`, and
/// compares it with `bytes`.
fn stage(layout: &Layout, source: &Path, bytes: &[u8]) -> Result<VersionLine, InstallError> {
    let staged = &layout.staged;
    let _ = fs::remove_file(staged);
    let mut file = SecurityDescriptor::from_sddl(ADMIN_ONLY_FILE_SDDL)
        .and_then(|descriptor| create_new_with_descriptor(staged, &descriptor))
        .map_err(|error| InstallError::io(format!("creating {}", staged.display()), error))?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let checked = written
        .map_err(|error| InstallError::io(format!("writing {}", staged.display()), error))
        .and_then(|()| {
            staged_version(staged).map_err(|reason| InstallError::SourceRejected {
                path: source.to_owned(),
                reason,
            })
        })
        .and_then(|version| {
            let copy = fs::read(staged).map_err(|error| {
                InstallError::io(format!("reading {}", staged.display()), error)
            })?;
            if copy == bytes {
                Ok(version)
            } else {
                Err(InstallError::Mismatch(staged.clone()))
            }
        });
    if checked.is_err() {
        let _ = fs::remove_file(staged);
    }
    checked
}

/// Runs `<staged> --version` and checks the protocol.
fn staged_version(staged: &Path) -> Result<VersionLine, String> {
    let mut child = Command::new(staged)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("it does not run: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`--version` did not finish".into());
            }
        }
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if !status.success() {
        return Err(format!("`--version` exited with {status}"));
    }
    let version = parse_version_line(stdout.trim())
        .ok_or_else(|| format!("unexpected `--version` output {:?}", stdout.trim()))?;
    if version.protocol != PIPE_PROTOCOL {
        return Err(format!(
            "it speaks pipe protocol {}, this devlanes needs {PIPE_PROTOCOL}",
            version.protocol
        ));
    }
    Ok(version)
}

/// The service, or `None` when it does not exist.
fn open_existing(
    manager: &ServiceManager,
    access: ServiceAccess,
) -> Result<Option<Service>, InstallError> {
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => Ok(Some(service)),
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(NO_SUCH_SERVICE) =>
        {
            Ok(None)
        }
        Err(source) => Err(InstallError::Service {
            action: "opening the service",
            source,
        }),
    }
}

/// The service's current state.
fn state(service: &Service) -> Result<ServiceState, InstallError> {
    Ok(service
        .query_status()
        .map_err(InstallError::service("querying the service"))?
        .current_state)
}

/// Stops the service and waits until it has; a stopped service is fine.
fn stop(service: &Service) -> Result<(), InstallError> {
    match service.stop() {
        Ok(_) => {}
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(NOT_ACTIVE) => {}
        Err(source) => {
            return Err(InstallError::Service {
                action: "stopping the service",
                source,
            });
        }
    }
    wait_for(service, ServiceState::Stopped, "stop")
}

/// Polls until the service is in `wanted`, failing early when a start ends in
/// `Stopped`.
fn wait_for(
    service: &Service,
    wanted: ServiceState,
    verb: &'static str,
) -> Result<(), InstallError> {
    let deadline = Instant::now() + STATE_TIMEOUT;
    loop {
        let current = state(service)?;
        if current == wanted {
            return Ok(());
        }
        if wanted == ServiceState::Running && current == ServiceState::Stopped {
            return Err(InstallError::State(
                "start; it stopped again (see `sc.exe query devlanes-hosts` for its exit code)",
            ));
        }
        if Instant::now() >= deadline {
            return Err(InstallError::State(verb));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only: whatever this machine has, the query answers without elevation.
    #[test]
    fn the_state_query_needs_no_elevation() {
        service_state().expect("service state");
    }

    #[test]
    fn sources_are_checked_before_anything_is_written() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing.exe");
        assert_eq!(
            read_source(&missing).expect_err("missing").code(),
            "service_source_missing"
        );
        let text = temp.path().join("text.exe");
        fs::write(&text, "not a program").expect("text");
        assert_eq!(
            read_source(&text).expect_err("text").code(),
            "service_source_rejected"
        );
        let program = temp.path().join("program.exe");
        fs::write(&program, "MZ").expect("program");
        assert_eq!(read_source(&program).expect("program"), b"MZ");
    }

    /// Every failure has its own exit code, which the CLI maps back.
    #[test]
    fn exit_codes_map_back_to_the_failure() {
        let errors = [
            InstallError::NotElevated,
            InstallError::NotAnAccount("S-1-5-18".into()),
            InstallError::Mismatch(PathBuf::from("x")),
            InstallError::State("start"),
            InstallError::io("x", io::Error::other("y")),
        ];
        for error in errors {
            let (code, message) =
                exit_failure(u32::from(error.exit_code())).expect("known exit code");
            assert_eq!(code, error.code());
            assert!(!message.is_empty());
        }
        assert_eq!(exit_failure(1), None);
        assert_eq!(exit_failure(u32::from(FIRST_EXIT_CODE) + 8), None);
    }

    #[test]
    fn failures_restart_the_service_three_times() {
        let actions = failure_actions().actions.expect("actions");
        assert_eq!(actions.len(), 3);
        assert!(actions.iter().all(|action| {
            action.action_type == ServiceActionType::Restart
                && action.delay == Duration::from_secs(5)
        }));
    }

    #[test]
    fn the_service_runs_as_local_system_at_boot_for_one_user() {
        let info = service_info(
            Path::new(r"C:\Program Files\devlanes\devlanes-hosts-service.exe"),
            "S-1-5-21-1-2-3-1001",
        );
        assert_eq!(info.account_name, None);
        assert_eq!(info.start_type, ServiceStartType::AutoStart);
        assert_eq!(info.launch_arguments, ["--user-sid", "S-1-5-21-1-2-3-1001"]);
    }

    /// The staged copy of a real binary reports its version; this test binary
    /// is not the service, so its output is refused.
    #[test]
    fn a_binary_without_the_version_line_is_refused() {
        let exe = std::env::current_exe().expect("exe");
        let error = staged_version(&exe).expect_err("not the service");
        assert!(
            error.contains("unexpected") || error.contains("exited"),
            "{error}"
        );
    }

    /// Read-only: `%ProgramFiles%` passes the admin-only check; a directory
    /// this user can write does not.
    #[test]
    fn program_files_is_admin_only_and_temp_is_not() {
        let program_files = devlanes_sys::windows::dirs::program_files().expect("program files");
        admin_only(&program_files, true).expect("program files is admin-only");
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            admin_only(temp.path(), true).expect_err("temp").code(),
            "hosts_unsafe_directory"
        );
    }
}
