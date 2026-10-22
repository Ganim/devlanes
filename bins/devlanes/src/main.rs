//! Composition root: the only place that knows every integration crate.
//! Client commands run here; `devlanes daemon run` builds the daemon from the same binary
//! so client and daemon versions match on a fresh install.

mod attach;
mod bundle;
mod choose;
mod commands;
mod completion;
mod daemon;
mod driver;
mod hosts;
mod local;
mod local_doctor;
mod notices;
mod output;
mod progress;
mod remote;
mod start_ui;
mod verbose;
mod wiring;

use std::process::ExitCode;

use devlanes_cli::{Action, Cli, Runtime, Signals, clap::Parser};

/// Parses, resolves output policy once, then dispatches to client or daemon code.
fn main() -> ExitCode {
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
    let runtime = match Runtime::resolve(&cli, Signals::detect()) {
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
