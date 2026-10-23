//! The console-control helper: a short-lived process without a console of its
//! own that attaches to a unit's console and sends it Ctrl-C or Ctrl-Break.
//!
//! The daemon has no console and a process can attach to only one at a time,
//! so the runner starts a helper without a console (`DETACHED_PROCESS`) per
//! event. The helper is any program that calls [`console_ctrl_main`] when the
//! request variable is set: `devlanes __console-ctrl` in the shipped binary, the
//! test binary in tests.

use std::{
    ffi::OsString,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use devlanes_core::ProcessIdentity;
use devlanes_sys::windows::proc::Held;

/// The environment variable carrying the request,
/// `<pid> <c|break> [<milliseconds>]`: the target, the event, and how long the
/// target may still be setting up its console (none: no wait). An environment
/// variable rather than arguments, so any program can be the helper whatever
/// its own argument parsing.
pub const REQUEST_VAR: &str = "DEVLANES_CONSOLE_CTRL";

/// The hidden argument `devlanes` dispatches to [`console_ctrl_main`] on.
pub const HELPER_ARG: &str = "__console-ctrl";

/// The console control event a helper is asked to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Send `CTRL_C_EVENT`.
    CtrlC,
    /// Send `CTRL_BREAK_EVENT`.
    CtrlBreak,
}

impl Request {
    /// The request's word in [`REQUEST_VAR`].
    fn word(self) -> &'static str {
        match self {
            Self::CtrlC => "c",
            Self::CtrlBreak => "break",
        }
    }

    /// Parses `<pid> <word> [<milliseconds>]`.
    fn parse(text: &str) -> Option<(u32, Self, Duration)> {
        let mut words = text.split_whitespace();
        let pid = words.next()?.parse().ok()?;
        let request = match words.next()? {
            "c" => Self::CtrlC,
            "break" => Self::CtrlBreak,
            _ => return None,
        };
        let wait = match words.next() {
            Some(millis) => Duration::from_millis(millis.parse().ok()?),
            None => Duration::ZERO,
        };
        if words.next().is_some() {
            return None;
        }
        Some((pid, request, wait))
    }
}

/// How long a target may still be setting up its console. A process creates
/// it during its own startup, so a unit's leader asked about right after it
/// was started may not have one yet.
pub(crate) const CONSOLE_SETUP: Duration = Duration::from_secs(2);

/// The helper's body: sends the event in [`REQUEST_VAR`] and returns the exit
/// code the runner reads (0, or 3/4/5 for the attach failures in
/// `devlanes_sys::windows::ctrl::CtrlError`, 2 for a malformed request).
pub fn console_ctrl_main() -> u8 {
    use devlanes_sys::windows::ctrl::CtrlEvent;
    let Some((pid, request, wait)) = std::env::var(REQUEST_VAR)
        .ok()
        .and_then(|text| Request::parse(&text))
    else {
        return 2;
    };
    let event = match request {
        Request::CtrlC => CtrlEvent::CtrlC,
        Request::CtrlBreak => CtrlEvent::CtrlBreak,
    };
    match attach_patiently(pid, wait).and_then(|attached| attached.send(event)) {
        Ok(()) => 0,
        Err(error) => error.exit_code(),
    }
}

/// Attaches to `pid`'s console, waiting up to `wait` for a process that has
/// none yet and is still running. The runner gives a wait only for a target it
/// started moments ago; a process that never gets a console (a GUI program,
/// `DETACHED_PROCESS`) costs at most that.
fn attach_patiently(
    pid: u32,
    wait: Duration,
) -> Result<devlanes_sys::windows::ctrl::Attached, devlanes_sys::windows::ctrl::CtrlError> {
    use devlanes_sys::windows::{
        ctrl::{Attached, CtrlError},
        process::wait_exit,
    };
    let deadline = Instant::now() + wait;
    loop {
        match Attached::to(pid) {
            Err(CtrlError::NoConsole)
                if Instant::now() < deadline && !wait_exit(pid, Duration::ZERO).unwrap_or(true) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

/// How the runner starts the helper: `program` with `args` before anything else.
#[derive(Debug, Clone)]
pub struct ConsoleHelper {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// How long a helper may take before it is given up on. Generous because the
/// first start of a new executable can take seconds while an antivirus scans
/// it; a helper that works answers in milliseconds.
pub(crate) const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

impl ConsoleHelper {
    /// The shipped binary as its own helper: `<exe> __console-ctrl`.
    pub fn current_exe() -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec![HELPER_ARG.into()],
        })
    }

    /// Sends `request` to the console `target` is attached to, waiting up to
    /// `wait` for a console still being set up; `true` when the event went out.
    /// `target` is held open meanwhile, so the helper's attach by bare PID
    /// cannot reach a stranger that got the PID in between. Blocking.
    pub fn send_held(&self, target: &ProcessIdentity, request: Request, wait: Duration) -> bool {
        let held = match Held::open(target) {
            Ok(Some(held)) => held,
            Ok(None) => return false,
            Err(error) => {
                tracing::debug!(%error, pid = target.pid, "console control event not sent");
                return false;
            }
        };
        let sent = self.send(target.pid, request, wait);
        drop(held);
        sent.map_err(|error| {
            tracing::debug!(%error, pid = target.pid, "console control event not sent");
        })
        .is_ok()
    }

    /// Sends Ctrl-C or Ctrl-Break to `pid`'s console through a helper, waiting
    /// up to `wait` for a console still being set up. Blocking.
    fn send(&self, pid: u32, request: Request, wait: Duration) -> Result<(), HelperError> {
        use devlanes_sys::windows::process::DETACHED_PROCESS;
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new(&self.program)
            .args(&self.args)
            .env(
                REQUEST_VAR,
                format!("{pid} {} {}", request.word(), wait.as_millis()),
            )
            // No console of its own, so it can attach to the unit's.
            .creation_flags(DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(HelperError::Spawn)?;
        let deadline = Instant::now() + HELPER_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(HelperError::Spawn)? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HelperError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        /// `STATUS_CONTROL_C_EXIT`: ended by a console control event.
        const ENDED_BY_EVENT: i32 = 0xC000_013A_u32.cast_signed();
        match status.code() {
            // The helper is attached to the console it signals, and Windows
            // delivers the event on a thread of its own that can race the
            // helper's exit and end it. Being ended by an event this soon after
            // sending one means it went out, so this is success.
            Some(0 | ENDED_BY_EVENT) => Ok(()),
            Some(code) => Err(HelperError::Exit(code)),
            None => Err(HelperError::Exit(-1)),
        }
    }
}

/// Why a helper request failed.
#[derive(Debug, thiserror::Error)]
enum HelperError {
    #[error("starting the console helper failed: {0}")]
    Spawn(std::io::Error),
    #[error("the console helper did not finish in time")]
    Timeout,
    /// 3, 4 and 5 are the attach failures of `CtrlError::exit_code`.
    #[error("the console helper exited with {0}")]
    Exit(i32),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests round-trip through their words; anything else is refused.
    #[test]
    fn requests_parse() {
        for request in [Request::CtrlC, Request::CtrlBreak] {
            assert_eq!(
                Request::parse(&format!("42 {}", request.word())),
                Some((42, request, Duration::ZERO))
            );
            assert_eq!(
                Request::parse(&format!("42 {} 1500", request.word())),
                Some((42, request, Duration::from_millis(1500)))
            );
        }
        assert_eq!(Request::parse("42"), None);
        assert_eq!(Request::parse("x break"), None);
        assert_eq!(Request::parse("42 kill"), None);
        assert_eq!(Request::parse("42 break soon"), None);
        assert_eq!(Request::parse("42 break 1 2"), None);
    }

    /// Not a test of its own: this binary as the helper when started with a
    /// request. Without one it does nothing.
    #[test]
    fn helper_entry() {
        if std::env::var_os(REQUEST_VAR).is_some() {
            std::process::exit(i32::from(console_ctrl_main()));
        }
    }

    /// A helper reaches a process's console right after it started, and the
    /// event it sends there (which reaches the helper too, attached to the same
    /// console) counts as sent. Errors are shown whole, since they differ
    /// between machines (CI runners).
    #[test]
    fn helpers_signal_a_console() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::windows::process::CommandExt;
        // Ping inherits the "ignore Ctrl-C" flag from a Git Bash parent; the
        // daemon clears it the same way.
        devlanes_sys::windows::process::accept_ctrl_c()?;
        let mut ping = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .creation_flags(devlanes_sys::windows::process::CREATE_NO_WINDOW)
            .spawn()?;
        let helper = ConsoleHelper {
            program: std::env::current_exe()?,
            args: ["--exact", "ctrl::tests::helper_entry", "--nocapture"]
                .map(Into::into)
                .into(),
        };
        let result = (|| -> Result<(), HelperError> {
            // Started just now: its console may still be on its way.
            helper.send(ping.id(), Request::CtrlC, CONSOLE_SETUP)?;
            let ended = devlanes_sys::windows::process::wait_exit(ping.id(), HELPER_TIMEOUT)
                .map_err(HelperError::Spawn)?;
            assert!(ended, "Ctrl-C did not end ping");
            Ok(())
        })();
        let _ = ping.kill();
        let _ = ping.wait();
        result.map_err(|error| format!("{error:?}: {error}"))?;
        Ok(())
    }
}
