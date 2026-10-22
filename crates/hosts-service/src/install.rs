//! Installing and removing the service. Runs in the elevated `devlanes.exe`
//! child that `devlanes certs trust --hosts` starts through the UAC prompt; never
//! in the service itself.
//!
//! The source binary sits in a user-writable directory, so it is opened without
//! following links, read into memory once, and only those bytes are trusted:
//! they are written to a staged copy in the admin-only directory, the staged
//! copy's `--version` is checked, and the copy is read back and compared before
//! it replaces anything. Every file and key this creates gets Administrators as
//! its owner at creation, whatever the "default owner" policy says. A failure
//! after the old binary was moved aside puts it, the old `UserSid` and the old
//! running state back; a service this run created is deleted.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use devlanes_hosts_helper::protocol::{PIPE_PROTOCOL, VersionLine, parse_version_line};
use devlanes_sys::windows::process::CREATE_NO_WINDOW;
use devlanes_sys::windows::registry::machine;
use devlanes_sys::windows::security::{
    ADMIN_ONLY_FILE_SDDL, PlainFileProblem, SecurityDescriptor, create_admin_only_dir,
    create_new_with_descriptor, open_plain_file,
};
use devlanes_sys::windows::token::{self, Sid};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::checks::{self, CheckError};
use crate::config::{
    self, BACKUP_BINARY, ConfigError, DESCRIPTION, DISPLAY_NAME, PARAMETERS_KEY, SERVICE_BINARY,
    SERVICE_NAME, STAGED_BINARY, USER_SID_VALUE,
};

/// The only privilege the service keeps in its token; the service manager
/// removes the rest of LocalSystem's at start.
///
/// The service needs none for what it does: SYSTEM's file and registry access
/// comes from access lists, the pipe's owner is its own account and its label is
/// below its own level, and `ipconfig` runs with the same token. Impersonating a
/// pipe client needs `SeImpersonatePrivilege` only for an impersonation-level
/// client; the daemon connects at identification level
/// (`devlanes_sys::windows::pipe::open_client`), which needs none, and any other
/// client is downgraded to identification, which is enough to read its token.
/// `SeChangeNotifyPrivilege` (traverse checking) every process keeps anyway.
pub const REQUIRED_PRIVILEGES: [&str; 1] = ["SeChangeNotifyPrivilege"];

/// Restarts after a failure or a refusal: three, five seconds apart, counted
/// over a day. A process that held the pipe name while the service was down has
/// usually let go by then.
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
    /// The install directory or binary could be changed by others.
    #[error("refusing to install: {0}")]
    UnsafeDirectory(#[source] CheckError),
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
    /// The service runs but did not answer as expected.
    #[error("the service started but its pipe check failed: {0}")]
    Unverified(String),
    /// A registry setting could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Local file or registry I/O.
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
            Self::NotElevated => "hosts_install_not_elevated",
            Self::NotAnAccount(_) => "hosts_install_user_rejected",
            Self::UnsafeDirectory(_) => "hosts_unsafe_directory",
            Self::SourceMissing { .. } => "service_source_missing",
            Self::SourceRejected { .. } => "service_source_rejected",
            Self::Mismatch(_) => "hosts_install_mismatch",
            Self::Service { .. } | Self::State(_) => "hosts_install_step_failed",
            Self::Unverified(_) => "hosts_install_unverified",
            Self::Config(_) | Self::Io { .. } => "io",
        }
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
        let directory = config::install_dir()?;
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
    /// The old `UserSid`, if any.
    user: Option<String>,
    /// Whether the service existed before.
    existed: bool,
    /// Whether it was running.
    running: bool,
}

/// Installs or updates the service for `user` from the binary at `source`, then
/// starts it and runs `verify` (the pipe `version` check). Must run elevated.
pub fn install(
    source: &Path,
    user: &Sid,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<Installed, InstallError> {
    if !token::elevated().map_err(|error| InstallError::io("reading the elevation", error))? {
        return Err(InstallError::NotElevated);
    }
    check_user(user)?;
    let layout = Layout::new()?;
    create_admin_only_dir(&layout.directory).map_err(|error| {
        InstallError::io(format!("creating {}", layout.directory.display()), error)
    })?;
    checks::check_install_directory(&layout.directory).map_err(InstallError::UnsafeDirectory)?;
    let bytes = read_source(source)?;
    let version = stage(&layout, source, &bytes)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(InstallError::service("connecting to the service manager"))?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::QUERY_CONFIG
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE;
    let existing = open_existing(&manager, access)?;
    let mut previous = Previous {
        binary: false,
        placed: false,
        user: machine::read_string(PARAMETERS_KEY, USER_SID_VALUE)
            .map_err(|error| InstallError::io("reading the previous UserSid", error))?,
        existed: existing.is_some(),
        running: false,
    };
    if let Some(service) = &existing {
        previous.running = state(service)? != ServiceState::Stopped;
        stop(service)?;
    }
    let result = activate(
        &layout,
        &manager,
        existing,
        access,
        user,
        &mut previous,
        verify,
    );
    match result {
        Ok(service) => {
            drop(service);
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

/// Moves the staged binary into place, configures and starts the service, and
/// verifies it. On failure, returns the service handle (if any) for the rollback.
#[allow(clippy::type_complexity)]
fn activate(
    layout: &Layout,
    manager: &ServiceManager,
    existing: Option<Service>,
    access: ServiceAccess,
    user: &Sid,
    previous: &mut Previous,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<Service, (InstallError, Option<Service>)> {
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
    if let Err(error) = checks::check_install(&layout.binary) {
        return Err((InstallError::UnsafeDirectory(error), existing));
    }
    let info = service_info(&layout.binary);
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
    if let Err(source) = service
        .update_failure_actions(failure_actions())
        .and_then(|()| service.set_failure_actions_on_non_crash_failures(true))
    {
        return Err((
            InstallError::Service {
                action: "setting the restart after failures",
                source,
            },
            Some(service),
        ));
    }
    if let Err(error) =
        devlanes_sys::windows::service::set_required_privileges(SERVICE_NAME, &REQUIRED_PRIVILEGES)
    {
        return Err((
            InstallError::io("limiting the service's privileges", error),
            Some(service),
        ));
    }
    let sid = match user.to_sddl() {
        Ok(sid) => sid,
        Err(error) => {
            return Err((
                InstallError::io("formatting the user SID", error),
                Some(service),
            ));
        }
    };
    // Recreated rather than reused, so the key is owned by Administrators even
    // if an earlier install created it under another default owner.
    if let Err(error) = machine::delete_tree(PARAMETERS_KEY)
        .and_then(|()| machine::write_string(PARAMETERS_KEY, USER_SID_VALUE, &sid))
    {
        return Err((InstallError::io("writing UserSid", error), Some(service)));
    }
    if let Err(source) = service.start::<&str>(&[]) {
        return Err((
            InstallError::Service {
                action: "starting the service",
                source,
            },
            Some(service),
        ));
    }
    if let Err(error) = wait_for(&service, ServiceState::Running, "start") {
        return Err((error, Some(service)));
    }
    if let Err(message) = verify() {
        return Err((InstallError::Unverified(message), Some(service)));
    }
    Ok(service)
}

/// Puts the previous binary, `UserSid` and running state back; deletes a
/// service this run created. Best effort: every step runs.
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
    match &previous.user {
        Some(user) => {
            let _ = machine::write_string(PARAMETERS_KEY, USER_SID_VALUE, user);
        }
        None => {
            let _ = machine::delete_value(PARAMETERS_KEY, USER_SID_VALUE);
        }
    }
    if let Some(service) = service {
        if !previous.existed {
            let _ = service.delete();
        } else if previous.running {
            let _ = service.start::<&str>(&[]);
        }
    }
}

/// Removes the service and its files. Clearing the block is the caller's job
/// (over the pipe, before this runs). Returns what could not be removed.
pub fn uninstall() -> Result<Vec<String>, InstallError> {
    if !token::elevated().map_err(|error| InstallError::io("reading the elevation", error))? {
        return Err(InstallError::NotElevated);
    }
    let layout = Layout::new()?;
    let mut warnings = Vec::new();
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(InstallError::service("connecting to the service manager"))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    if let Some(service) = open_existing(&manager, access)? {
        if let Err(error) = stop(&service) {
            warnings.push(error.to_string());
        }
        if let Err(error) = machine::delete_tree(PARAMETERS_KEY) {
            warnings.push(format!("removing HKLM\\{PARAMETERS_KEY}: {error}"));
        }
        service
            .delete()
            .map_err(InstallError::service("deleting the service"))?;
    }
    for path in [&layout.binary, &layout.backup, &layout.staged] {
        match devlanes_sys::remove_file_retrying(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warnings.push(format!("removing {}: {error}", path.display())),
        }
    }
    match fs::remove_dir(&layout.directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!(
            "{} was left in place: {error}",
            layout.directory.display()
        )),
    }
    Ok(warnings)
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

/// The service's settings: its own process as LocalSystem, started at boot, no
/// arguments. `windows-service` quotes the path because it contains spaces.
fn service_info(binary: &Path) -> ServiceInfo {
    ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: binary.to_owned(),
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        // None is LocalSystem.
        account_name: None,
        account_password: None,
    }
}

/// Refuses a SID that is not a person's user account: by shape (a built-in
/// account or a well-known group), then by the account database (a domain
/// group). The service would open its pipe to that SID.
fn check_user(user: &Sid) -> Result<(), InstallError> {
    let text = user
        .to_sddl()
        .map_err(|error| InstallError::io("formatting the user SID", error))?;
    if !user.has_account_shape() {
        return Err(InstallError::NotAnAccount(text));
    }
    match user.names_a_user() {
        Ok(true) => Ok(()),
        Ok(false) => Err(InstallError::NotAnAccount(text)),
        Err(error) => Err(InstallError::io(format!("looking up {text}"), error)),
    }
}

/// Reads the source once; only these bytes are trusted from here on. The file
/// itself is opened, never a link's target, and must have a single name.
fn read_source(source: &Path) -> Result<Vec<u8>, InstallError> {
    let missing = |error| InstallError::SourceMissing {
        path: source.to_owned(),
        source: error,
    };
    let reject = |reason: &str| InstallError::SourceRejected {
        path: source.to_owned(),
        reason: reason.to_owned(),
    };
    let file = match open_plain_file(source).map_err(missing)? {
        Ok(file) => file,
        Err(PlainFileProblem::ReparsePoint) => return Err(reject("it is a symlink or junction")),
        Err(PlainFileProblem::NotAFile) => return Err(reject("it is not a file")),
        Err(PlainFileProblem::HardLinks(_)) => {
            return Err(reject("it has more than one name (hard link)"));
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(missing)?;
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
    let cleanup = |error: InstallError| {
        let _ = fs::remove_file(staged);
        error
    };
    written.map_err(|error| {
        cleanup(InstallError::io(
            format!("writing {}", staged.display()),
            error,
        ))
    })?;
    let version = staged_version(staged).map_err(|reason| {
        cleanup(InstallError::SourceRejected {
            path: source.to_owned(),
            reason,
        })
    })?;
    let copy = fs::read(staged).map_err(|error| {
        cleanup(InstallError::io(
            format!("reading {}", staged.display()),
            error,
        ))
    })?;
    if copy != bytes {
        return Err(cleanup(InstallError::Mismatch(staged.clone())));
    }
    Ok(version)
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
        assert_eq!(read_source(&program).expect("one name"), b"MZ");
        fs::hard_link(&program, temp.path().join("other.exe")).expect("hard link");
        let error = read_source(&program).expect_err("two names");
        assert_eq!(error.code(), "service_source_rejected");
        assert!(error.to_string().contains("hard link"), "{error}");
        assert_eq!(
            read_source(temp.path()).expect_err("directory").code(),
            "service_source_rejected"
        );
    }

    /// Only a person's account is served; SYSTEM, groups and built-in aliases
    /// are refused before anything is installed.
    #[test]
    fn only_user_accounts_are_served() {
        check_user(&token::current_user().expect("user")).expect("this user");
        for other in ["S-1-5-18", "S-1-5-32-544", "S-1-1-0", "S-1-5-11"] {
            let error = check_user(&Sid::parse(other).expect("sid")).expect_err(other);
            assert_eq!(error.code(), "hosts_install_user_rejected", "{other}");
        }
    }

    #[test]
    fn failures_restart_the_service_three_times() {
        let actions = failure_actions().actions.expect("actions");
        assert_eq!(actions.len(), 3);
        assert!(actions.iter().all(|action| {
            action.action_type == ServiceActionType::Restart
                && action.delay == Duration::from_secs(5)
        }));
        assert_eq!(REQUIRED_PRIVILEGES, ["SeChangeNotifyPrivilege"]);
    }

    #[test]
    fn the_service_runs_as_local_system_at_boot() {
        let info = service_info(Path::new(
            r"C:\Program Files\devlanes\devlanes-hosts-service.exe",
        ));
        assert_eq!(info.account_name, None);
        assert_eq!(info.start_type, ServiceStartType::AutoStart);
        assert!(info.launch_arguments.is_empty());
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
}
