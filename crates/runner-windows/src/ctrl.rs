//! The console-control helper: a short-lived process that attaches to a
//! unit's console and sends it Ctrl-C or Ctrl-Break.
//!
//! A process can be attached to one console only, so the daemon cannot send
//! the event itself. The runner starts the helper without a console of its
//! own and does not wait for it in the unit's supervisor.

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    time::{Duration, Instant},
};

use devlanes_sys_windows::{
    console::{AttachError, Attached, CtrlEvent},
    proc::Held,
    spawn::{self as win, CreationFlags, Spawn, Stdio, Stream},
};

/// The hidden first argument `devlanes` dispatches to [`console_ctrl_main`] on,
/// before parsing anything else: `devlanes __console-ctrl <pid> <c|break>`.
pub const HELPER_ARG: &str = "__console-ctrl";

/// How long the helper waits for its own event to come back, which shows the
/// console host handled it. The host drops a request whose sender has left,
/// so the helper must not exit before that.
const EVENT_WAIT: Duration = Duration::from_secs(5);

/// `STATUS_CONTROL_C_EXIT`: a process ended by a console control event. The
/// helper can be ended by the event it sent while it detaches; it went out.
const CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Helper exit codes the runner tells apart in its logs.
mod code {
    pub const SENT: u8 = 0;
    pub const USAGE: u8 = 2;
    pub const NO_CONSOLE: u8 = 3;
    pub const NO_PROCESS: u8 = 4;
    pub const DENIED: u8 = 5;
    pub const FAILED: u8 = 6;
}

/// The helper's body for `<pid> <c|break>`, the arguments after
/// [`HELPER_ARG`]: attaches to `pid`'s console and sends the event to every
/// process on it. Exits 0 once the event arrived, 2 for bad arguments, 3 when
/// the process has no console, 4 when it does not exist, 5 when attaching was
/// refused and 6 for any other failure. The process should exit with the
/// returned code.
pub fn console_ctrl_main(args: impl IntoIterator<Item = OsString>) -> u8 {
    let Some((pid, event)) = parse(args) else {
        return code::USAGE;
    };
    let attached = match Attached::to(pid) {
        Ok(attached) => attached,
        Err(AttachError::NoConsole) => return code::NO_CONSOLE,
        Err(AttachError::NoProcess) => return code::NO_PROCESS,
        Err(AttachError::Denied) => return code::DENIED,
        Err(AttachError::Io(_)) => return code::FAILED,
    };
    match attached.generate(event, EVENT_WAIT) {
        Ok(true) => code::SENT,
        Ok(false) | Err(_) => code::FAILED,
    }
}

/// `<pid> <c|break>` and nothing else.
fn parse(args: impl IntoIterator<Item = OsString>) -> Option<(u32, CtrlEvent)> {
    let mut args = args.into_iter();
    let pid = args.next()?.to_str()?.parse().ok()?;
    let event = match args.next()?.to_str()? {
        "c" => CtrlEvent::CtrlC,
        "break" => CtrlEvent::CtrlBreak,
        _ => return None,
    };
    args.next().is_none().then_some((pid, event))
}

/// How the runner starts the helper: `program`, then `args`, then the
/// target's PID and the event.
#[derive(Debug, Clone)]
pub struct ConsoleHelper {
    /// The executable.
    pub program: PathBuf,
    /// Arguments before the target, such as [`HELPER_ARG`].
    pub args: Vec<OsString>,
}

impl ConsoleHelper {
    /// This executable as its own helper: `<exe> __console-ctrl`. The daemon
    /// reads the path once at start, so after `install.ps1` swaps the binary
    /// under a running daemon, the newer devlanes at that path is the helper.
    /// The `__console-ctrl <pid> <c|break>` form and the exit codes must
    /// therefore stay readable by older and newer daemons alike.
    pub fn current_exe() -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec![HELPER_ARG.into()],
        })
    }

    /// Sends `event` to the console `target` is attached to and waits for the
    /// helper until `deadline`, ending it if it is still running then.
    /// Holding `target` keeps its PID from naming another process while the
    /// helper attaches to it by PID. Blocking.
    pub(crate) fn send(
        &self,
        target: &Held,
        event: CtrlEvent,
        deadline: Instant,
    ) -> Result<(), HelperError> {
        let word = match event {
            CtrlEvent::CtrlC => "c",
            CtrlEvent::CtrlBreak => "break",
        };
        let mut line = format!("\"{}\"", self.program.display());
        for arg in &self.args {
            line.push(' ');
            line.push_str(&crate::command::quote(&arg.to_string_lossy()));
        }
        line.push_str(&format!(" {} {word}", target.pid()));
        let child = win::spawn(&Spawn {
            program: &self.program,
            command_line: OsStr::new(&line),
            environment: None,
            cwd: None,
            stdio: Stdio::Handles {
                stdin: Stream::Null,
                stdout: Stream::Null,
                stderr: Stream::Null,
            },
            inherit: &[],
            job: None,
            // No console of its own, so it can attach to the unit's.
            flags: CreationFlags {
                detached_process: true,
                ..CreationFlags::default()
            },
        })
        .map_err(HelperError::Spawn)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match child.wait_timeout(remaining).map_err(HelperError::Spawn)? {
            Some(code) if code == u32::from(code::SENT) || code == CONTROL_C_EXIT => Ok(()),
            Some(code) => Err(HelperError::Exit(code)),
            None => {
                let _ = child.terminate(1);
                Err(HelperError::Timeout)
            }
        }
    }
}

/// Why a helper did not deliver its event.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HelperError {
    #[error("starting the console helper failed: {0}")]
    Spawn(std::io::Error),
    #[error("the console helper did not finish before the stop timeout")]
    Timeout,
    /// 3 no console, 4 no such process, 5 refused, 6 failed.
    #[error("the console helper exited with {0}")]
    Exit(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `<pid> <c|break>` is accepted.
    #[test]
    fn arguments_parse() {
        let parse = |args: &[&str]| parse(args.iter().map(OsString::from));
        assert_eq!(parse(&["42", "c"]), Some((42, CtrlEvent::CtrlC)));
        assert_eq!(parse(&["42", "break"]), Some((42, CtrlEvent::CtrlBreak)));
        for bad in [
            &["42"][..],
            &["x", "break"],
            &["42", "kill"],
            &["42", "break", "extra"],
            &[],
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
