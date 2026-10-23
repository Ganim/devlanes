//! Composition root: the only place that knows every integration crate.
//! Client commands run here; `devlanes daemon run` builds the daemon from the same binary
//! so client and daemon versions match on a fresh install.

mod attach;
mod bundle;
mod choose;
mod commands;
mod completion;
// Unix prompts and attach use `/dev/tty` and termios in `commands.rs` and `attach.rs`.
#[cfg(windows)]
mod console;
mod daemon;
mod driver;
#[cfg(windows)]
mod follow;
mod hosts;
mod local;
mod local_doctor;
mod notices;
mod output;
mod progress;
mod remote;
mod start_ui;
mod terminal;
mod user_config;
mod verbose;
mod wiring;

use std::process::ExitCode;

use devlanes_cli::{Action, Cli, Runtime, Signals, clap::Parser};

/// Parses, resolves output policy once, then dispatches to client or daemon code.
fn main() -> ExitCode {
    // The hidden helper the Windows runner starts to send a console event to a
    // unit; it is not part of the CLI, so nothing else is set up for it.
    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == devlanes_runner_windows::HELPER_ARG)
    {
        return ExitCode::from(devlanes_runner_windows::console_ctrl_main(
            std::env::args_os().skip(2),
        ));
    }
    // CompleteEnv must run before any thread exists; it exits when completing.
    completion::install();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            #[cfg(windows)]
            cli_console();
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
    // `daemon run` makes a hidden console of its own instead, and writes no
    // escape sequences to it.
    #[cfg(windows)]
    let escapes = if matches!(
        cli.command,
        Some(devlanes_cli::Command::Daemon {
            command: devlanes_cli::DaemonCommand::Run
        })
    ) {
        true
    } else {
        cli_console();
        devlanes_sys_windows::console::enable_vt_output()
    };
    // Colors and terminal features need VT processing in a classic console;
    // one that refuses it gets plain output.
    #[cfg(windows)]
    let signals = Signals {
        no_escapes: !escapes,
        ..Signals::detect()
    };
    #[cfg(unix)]
    let signals = Signals::detect();
    let mut runtime = match Runtime::resolve(&cli, signals) {
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
    // Links, marks, titles and the like; read once, before any output.
    runtime.terminal = terminal::detect(&runtime);
    // None of them on a console without VT processing, even for an explicit
    // `--output human`.
    #[cfg(windows)]
    if !escapes {
        runtime.terminal = devlanes_term::Features::default();
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

/// Gives the CLI a console without a window when it starts without one: the
/// binary's manifest asks for none (`consoleAllocationPolicy`, there for the
/// logon start), and without a console every program the CLI starts would
/// open a window of its own. Standard handles the parent passed (pipes or
/// `NUL` from a detached parent) stay as they were. Terminals and hidden
/// consoles are kept.
#[cfg(windows)]
fn cli_console() {
    let _ = devlanes_sys_windows::console::alloc_hidden_console();
}
