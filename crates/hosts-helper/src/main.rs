//! `devlanes-hosts` binary; all logic is in the library so it can be tested.

fn main() -> std::process::ExitCode {
    devlanes_hosts_helper::cli::main()
}
