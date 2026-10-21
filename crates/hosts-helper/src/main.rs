//! `devlanes-hosts` binary; all logic is in the library so it can be tested.

/// Runs the helper command line.
#[cfg(unix)]
fn main() -> std::process::ExitCode {
    devlanes_hosts_helper::cli::main()
}

/// Refuses to run: the helper relies on sudo and Unix file ownership. Cargo can't
/// limit a `[[bin]]` to some platforms, so this stub keeps Windows builds working.
#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("devlanes-hosts: not supported on this platform");
    std::process::ExitCode::from(2)
}
