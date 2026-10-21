//! Composition root: the only place that knows every integration crate.
//! Client commands run here; `devlanes daemon run` builds the daemon from the same binary
//! so client and daemon versions match on a fresh install.

mod attach;
mod commands;
mod completion;
mod daemon;
mod driver;
mod hosts;
mod local;
mod notices;
mod output;
mod progress;
mod remote;
mod wiring;

use std::process::ExitCode;

use devlanes_cli::{Action, Cli, Runtime, Signals, clap::Parser};

/// Parses, resolves output policy once, then dispatches to client or daemon code.
fn main() -> ExitCode {
    // CompleteEnv must run before any thread exists; it exits when completing.
    completion::install();
    let cli = Cli::parse();
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
    ExitCode::from(code)
}
