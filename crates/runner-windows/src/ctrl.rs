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

    /// Sends `request` to the console each target is attached to, waiting up
    /// to its duration for a console still being set up; `true` when any event
    /// went out. Every helper is started before any is waited for, and all
    /// share one deadline, so this takes at most [`HELPER_TIMEOUT`] however
    /// many consoles there are. Each target is held open meanwhile, so a
    /// helper's attach by bare PID cannot reach a stranger that got the PID in
    /// between. Blocking.
    pub fn send_held(&self, targets: &[(ProcessIdentity, Duration)], request: Request) -> bool {
        let deadline = Instant::now() + HELPER_TIMEOUT;
        let not_sent = |pid: u32, error: &dyn std::fmt::Display| {
            tracing::debug!(%error, pid, "console control event not sent");
        };
        let mut running = Vec::new();
        for (target, wait) in targets {
            match Held::open(target) {
                Ok(Some(held)) => match self.start(target.pid, request, *wait) {
                    Ok(child) => running.push((target.pid, held, child)),
                    Err(error) => not_sent(target.pid, &error),
                },
                Ok(None) => {}
                Err(error) => not_sent(target.pid, &error),
            }
        }
        let mut sent = false;
        while !running.is_empty() {
            let late = Instant::now() >= deadline;
            running.retain_mut(|(pid, _held, child)| {
                let result = match child.try_wait() {
                    Ok(None) if !late => return true,
                    Ok(None) => Err(HelperError::Timeout),
                    Ok(Some(status)) => outcome(status),
                    Err(error) => Err(HelperError::Spawn(error)),
                };
                if result.is_err() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                match result {
                    Ok(()) => sent = true,
                    Err(error) => not_sent(*pid, &error),
                }
                false
            });
            if !running.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        sent
    }

    /// Starts a helper that sends Ctrl-C or Ctrl-Break to `pid`'s console,
    /// waiting up to `wait` for a console still being set up.
    fn start(
        &self,
        pid: u32,
        request: Request,
        wait: Duration,
    ) -> Result<std::process::Child, HelperError> {
        use devlanes_sys::windows::process::DETACHED_PROCESS;
        use std::os::windows::process::CommandExt;
        std::process::Command::new(&self.program)
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
            .map_err(HelperError::Spawn)
    }
}

/// Whether a helper that exited with `status` sent its event.
fn outcome(status: std::process::ExitStatus) -> Result<(), HelperError> {
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

    /// Kills the children when the test is done, passed or failed.
    struct Children(Vec<std::process::Child>);

    impl Drop for Children {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Helpers reach the consoles of processes right after they started, one
    /// console each, and the events they send there (which reach the helpers
    /// too, attached to the same consoles) count as sent.
    #[test]
    fn helpers_signal_consoles() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::windows::process::CommandExt;
        // Ping inherits the "ignore Ctrl-C" flag from a Git Bash parent; the
        // daemon clears it the same way.
        devlanes_sys::windows::process::accept_ctrl_c()?;
        let mut pings = Children(Vec::new());
        let mut targets = Vec::new();
        for _ in 0..2 {
            let ping = std::process::Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .creation_flags(devlanes_sys::windows::process::CREATE_NO_WINDOW)
                .spawn()?;
            let identity = devlanes_sys::identity(ping.id())?.ok_or("ping missing")?;
            pings.0.push(ping);
            // Started just now: its console may still be on its way.
            targets.push((identity, CONSOLE_SETUP));
        }
        let helper = ConsoleHelper {
            program: std::env::current_exe()?,
            args: ["--exact", "ctrl::tests::helper_entry", "--nocapture"]
                .map(Into::into)
                .into(),
        };
        assert!(helper.send_held(&targets, Request::CtrlC), "nothing sent");
        for (ping, _) in &targets {
            let ended = devlanes_sys::windows::process::wait_exit(ping.pid, HELPER_TIMEOUT)?;
            assert!(ended, "Ctrl-C did not end ping {}", ping.pid);
        }
        Ok(())
    }

    /// Helpers that never answer are given up on together, after one
    /// [`HELPER_TIMEOUT`] rather than one each.
    #[test]
    fn hung_helpers_share_one_deadline() -> Result<(), Box<dyn std::error::Error>> {
        let me = devlanes_sys::identity(std::process::id())?.ok_or("own identity missing")?;
        // Ignores the request and runs longer than the timeout.
        let helper = ConsoleHelper {
            program: "ping".into(),
            args: ["-n", "60", "127.0.0.1"].map(Into::into).into(),
        };
        let started = Instant::now();
        assert!(!helper.send_held(&[(me, Duration::ZERO); 3], Request::CtrlBreak));
        let took = started.elapsed();
        assert!(
            took >= HELPER_TIMEOUT && took < HELPER_TIMEOUT + Duration::from_secs(5),
            "{took:?}"
        );
        Ok(())
    }
}
