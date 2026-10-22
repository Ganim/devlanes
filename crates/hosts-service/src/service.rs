//! What the binary runs: the service control dispatcher, the install and
//! configuration checks, and the pipe server until the service manager says stop.
//!
//! The binary takes nothing from its command line but `--version`: the hosts
//! file comes from `DataBasePath` and the user from `Parameters\UserSid`, both
//! admin-only registry values. A refusal stops the service with a
//! service-specific exit code, which the service manager records in the System
//! event log (event 7024) and shows in `sc.exe query devlanes-hosts`.

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use devlanes_hosts_helper::protocol::version_line;
use devlanes_sys::windows::token::{self, Sid};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

use crate::config::{self, PIPE_NAME, SERVICE_NAME};
use crate::handler::Target;
use crate::server::{ServerConfig, serve_with};

/// Why the service stopped without serving; the service-specific exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Refusal {
    /// Not running as LocalSystem.
    NotSystem = 1,
    /// The binary or its directory is misplaced or writable by others.
    UnsafeInstall = 2,
    /// `UserSid` or `DataBasePath` is missing or unusable.
    Config = 3,
    /// The pipe could not be created (another process holds the name).
    Pipe = 4,
    /// The async runtime could not start.
    Runtime = 5,
    /// The hosts file's directory is not admin-only.
    UnsafeHostsDirectory = 6,
}

/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`: started from a console, not by
/// the service manager.
const NOT_A_SERVICE: i32 = 1063;

/// Entry point of the binary.
pub fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--version" => {
            println!("{}", version_line(env!("CARGO_PKG_VERSION")));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("devlanes-hosts-service: takes no arguments besides --version");
            return ExitCode::from(2);
        }
    }
    match windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => ExitCode::SUCCESS,
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(NOT_A_SERVICE) =>
        {
            eprintln!(
                "devlanes-hosts-service: this is a Windows service; install it with `devlanes certs trust --hosts`"
            );
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("devlanes-hosts-service: {error}");
            ExitCode::from(1)
        }
    }
}

/// The service entry point the dispatcher calls on its own thread. Written by
/// hand instead of with `define_windows_service!` because the service ignores
/// its start arguments, and reading them would need `unsafe` here.
extern "system" fn ffi_service_main(_argc: u32, _argv: *mut *mut u16) {
    service_main();
}

/// Registers the control handler, checks the install and configuration, then
/// serves until stopped.
fn service_main() {
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopping = stop.clone();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            // Stored if the server is not waiting yet, so the stop is not lost.
            stopping.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    // Without a status handle the service manager cannot be told anything; it
    // reports the start as failed by itself.
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else {
        return;
    };
    let report = |state, accepted, exit| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: exit,
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        });
    };
    report(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::NO_ERROR,
    );
    let exit = match run(stop, || {
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            ServiceExitCode::NO_ERROR,
        );
    }) {
        Ok(()) => ServiceExitCode::NO_ERROR,
        Err(refusal) => ServiceExitCode::ServiceSpecific(refusal as u32),
    };
    report(ServiceState::Stopped, ServiceControlAccept::empty(), exit);
}

/// The checks, then the pipe server until `stop`. `running` is called once the
/// pipe exists, so a client never sees a running service without its pipe.
fn run(stop: Arc<tokio::sync::Notify>, running: impl FnOnce()) -> Result<(), Refusal> {
    let me = token::current_user().map_err(|_| Refusal::NotSystem)?;
    if me != Sid::local_system() {
        return Err(Refusal::NotSystem);
    }
    let exe = std::env::current_exe().map_err(|_| Refusal::UnsafeInstall)?;
    crate::checks::check_install(&exe).map_err(|_| Refusal::UnsafeInstall)?;
    let user = config::configured_user().map_err(|_| Refusal::Config)?;
    let hosts = config::hosts_path().map_err(|_| Refusal::Config)?;
    crate::checks::check_hosts_directory(&hosts).map_err(|_| Refusal::UnsafeHostsDirectory)?;
    // A crash mid-replace may have left the content under a backup name. Not
    // fatal here: the next write tries again and reports what it finds.
    let _ = crate::write::recover(&hosts);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Refusal::Runtime)?;
    let config = ServerConfig {
        pipe_name: PIPE_NAME.into(),
        user,
        target: Target {
            hosts,
            flush: true,
            check_directory: true,
        },
    };
    let result = runtime.block_on(async move {
        let (created, ready) = tokio::sync::oneshot::channel();
        let server = serve_reporting(config, stop, created);
        tokio::pin!(server);
        // The server reports once its first pipe instance exists.
        tokio::select! {
            result = &mut server => return result,
            ready = ready => if ready.is_ok() { running() },
        }
        server.await
    });
    // Requests still being written finish on their blocking threads first.
    runtime.shutdown_timeout(Duration::from_secs(10));
    result
}

/// [`serve`], sending on `created` once the first instance exists.
async fn serve_reporting(
    config: ServerConfig,
    stop: Arc<tokio::sync::Notify>,
    created: tokio::sync::oneshot::Sender<()>,
) -> Result<(), Refusal> {
    serve_with(config, stop.notified(), move || {
        let _ = created.send(());
    })
    .await
    .map_err(|_| Refusal::Pipe)
}
