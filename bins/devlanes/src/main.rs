//! Composition root: the only place that knows every integration crate.
//! Client commands run here; `devlanes daemon run` builds the daemon from the same binary
//! so client and daemon versions match on a fresh install.

mod attach;
mod bundle;
mod choose;
mod commands;
mod completion;
mod daemon;
mod daemon_log;
mod driver;
mod hosts;
#[cfg(windows)]
mod hosts_service;
mod local;
mod local_doctor;
mod notices;
mod orphans;
mod output;
mod progress;
mod remote;
#[cfg(windows)]
mod resolver_nrpt;
mod start_ui;
mod verbose;
mod wiring;
mod wsl_doctor;

use std::process::ExitCode;

use devlanes_cli::{Action, Cli, Runtime, Signals, clap::Parser};

/// Parses, resolves output policy once, then dispatches to client or daemon code.
fn main() -> ExitCode {
    // The daemon's console-control helper: no parsing, no threads, no output
    // beyond what the daemon reads, and it must not be listed as a command.
    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == devlanes_runner_windows::ctrl::HELPER_ARG)
    {
        return ExitCode::from(devlanes_runner_windows::console_ctrl_main());
    }
    // The elevated hosts service installer that `certs trust --hosts` starts
    // through the UAC prompt: hidden, so not a listed command.
    #[cfg(windows)]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if let Some(subcommand) = args.first().filter(|arg| {
            *arg == hosts_service::INSTALL_ARG || *arg == hosts_service::UNINSTALL_ARG
        }) {
            return hosts_service::elevated_main(subcommand, &args[1..]);
        }
    }
    // ssh runs this binary as SSH_ASKPASS with the prompt as its only argument,
    // which clap would read as an unknown subcommand.
    #[cfg(windows)]
    if let Some(code) = remote::askpass_from_env() {
        return ExitCode::from(code);
    }
    // The agent plugin's PreToolUse hook: needs no daemon, runtime or output
    // policy, and is not a listed command.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == devlanes_cli::agent_hook::ARG)
    {
        return ExitCode::from(devlanes_cli::agent_hook::run());
    }
    // CompleteEnv must run before any thread exists; it exits when completing.
    completion::install();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            use devlanes_cli::clap::error::ErrorKind;
            // Help, version and similar displays are not errors.
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                error.exit();
            }
            let args: Vec<String> = std::env::args().skip(1).collect();
            eprint!(
                "{}",
                devlanes_cli::usage_error(&error.render().to_string(), &args)
            );
            return ExitCode::from(2);
        }
    };
    // Git Bash starts native programs in a new process group, which sets the
    // inherited "ignore Ctrl-C" flag: without clearing it, Ctrl-C at a prompt or
    // in an attached command would never reach the CLI. Failing is harmless.
    #[cfg(windows)]
    let _ = devlanes_sys::windows::process::accept_ctrl_c();
    let signals = Signals {
        no_escapes: !output::enable_escapes(),
        ..Signals::detect()
    };
    let runtime = match Runtime::resolve(&cli, signals) {
        Ok(runtime) => runtime,
        Err(error) => return output::local_error(&error.to_string(), error.exit_code()),
    };
    let action = match cli.action(&runtime) {
        Ok(action) => action,
        Err(error) => return output::local_error(&error.to_string(), error.exit_code()),
    };
    if matches!(action, Action::DaemonRun) {
        return daemon::run();
    }
    // Not for the daemon, which logs to its file and inherits DEVLANES_VERBOSE,
    // nor for the TUI, whose screen stderr lines would garble.
    let verbose = if matches!(action, Action::Ui | Action::StartUi { .. }) {
        0
    } else {
        runtime.verbose
    };
    verbose::init(verbose);
    tracing::debug!(
        mode = runtime.mode.as_str(),
        reason = runtime.reason,
        color = runtime.color,
        actor = ?runtime.actor.kind,
        agent = runtime.actor.agent_id.as_deref(),
        ci = runtime.ci.map(|ci| ci.as_str()),
        stdout_tty = runtime.stdout_tty,
        version = env!("CARGO_PKG_VERSION"),
        "output mode"
    );
    let tokio = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(tokio) => tokio,
        Err(error) => return output::local_error(&error.to_string(), 1),
    };
    let code = tokio.block_on(commands::dispatch(&cli, &runtime, action));
    // Detached helper threads (stdin readers) must not keep the process alive.
    tokio.shutdown_background();
    verbose::finish(verbose, code);
    ExitCode::from(code)
}
