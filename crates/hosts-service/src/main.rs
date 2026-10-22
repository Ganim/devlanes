//! `devlanes-hosts-service` binary: started by the Windows service manager. It
//! takes no paths and no configuration on its command line; `--version` is the
//! only argument, for the installer's check of a staged copy.

/// Runs the service, or prints the version line for `--version`.
#[cfg(windows)]
fn main() -> std::process::ExitCode {
    devlanes_hosts_service::service::main()
}

/// Refuses to run: the service is Windows only. Cargo can't limit a `[[bin]]`
/// to some platforms, so this stub keeps other builds working.
#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("devlanes-hosts-service: only runs on Windows");
    std::process::ExitCode::from(2)
}
