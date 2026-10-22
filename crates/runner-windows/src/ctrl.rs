//! The console-control helper: a short-lived process that sends Ctrl-C or
//! Ctrl-Break to a unit's console, or lists the processes attached to it.
//!
//! The daemon has no console and a process can attach to only one at a time,
//! so the runner starts a helper without a console (`DETACHED_PROCESS`) for each
//! request. The helper is any program that calls [`console_ctrl_main`] when the
//! request variable is set: `devlanes __console-ctrl` in the shipped binary, the
//! test binary in tests.

use std::{
    ffi::OsString,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

/// The environment variable carrying the request, `<pid> <c|break|list>`. An
/// environment variable rather than arguments, so any program can be the helper
/// whatever its own argument parsing.
pub const REQUEST_VAR: &str = "DEVLANES_CONSOLE_CTRL";

/// The hidden argument `devlanes` dispatches to [`console_ctrl_main`] on.
pub const HELPER_ARG: &str = "__console-ctrl";

/// Prefix of the line a `list` request prints, followed by space-separated PIDs.
const LIST_PREFIX: &str = "console-pids:";

/// What a helper is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Send `CTRL_C_EVENT`.
    CtrlC,
    /// Send `CTRL_BREAK_EVENT`.
    CtrlBreak,
    /// List the processes attached to the console.
    List,
}

impl Request {
    /// The request's word in [`REQUEST_VAR`].
    fn word(self) -> &'static str {
        match self {
            Self::CtrlC => "c",
            Self::CtrlBreak => "break",
            Self::List => "list",
        }
    }

    /// Parses `<pid> <word>`.
    fn parse(text: &str) -> Option<(u32, Self)> {
        let (pid, word) = text.trim().split_once(' ')?;
        let request = match word {
            "c" => Self::CtrlC,
            "break" => Self::CtrlBreak,
            "list" => Self::List,
            _ => return None,
        };
        Some((pid.parse().ok()?, request))
    }
}

/// PIDs from a helper's output: the last `console-pids:` line. Other lines (a
/// test harness's own output) are ignored.
fn parse_list(output: &str) -> Option<Vec<u32>> {
    let line = output
        .lines()
        .rev()
        .find_map(|line| line.split_once(LIST_PREFIX).map(|(_, rest)| rest))?;
    line.split_whitespace()
        .map(|pid| pid.parse().ok())
        .collect()
}

/// The helper's body: performs the request in [`REQUEST_VAR`] and returns the
/// exit code the runner reads (0, or 3/4/5 for the attach failures in
/// `devlanes_sys::windows::ctrl::CtrlError`, 2 for a malformed request).
pub fn console_ctrl_main() -> u8 {
    use devlanes_sys::windows::ctrl::{self, CtrlEvent};
    let Some((pid, request)) = std::env::var(REQUEST_VAR)
        .ok()
        .and_then(|text| Request::parse(&text))
    else {
        return 2;
    };
    let result = match request {
        Request::CtrlC => ctrl::send(pid, CtrlEvent::CtrlC),
        Request::CtrlBreak => ctrl::send(pid, CtrlEvent::CtrlBreak),
        Request::List => ctrl::console_processes(pid).map(|pids| {
            let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
            println!("{LIST_PREFIX} {}", pids.join(" "));
        }),
    };
    match result {
        Ok(()) => 0,
        Err(error) => error.exit_code(),
    }
}

/// How the runner starts the helper: `program` with `args` before anything else.
#[derive(Debug, Clone)]
pub struct ConsoleHelper {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// How long a helper may take before it is given up on and ended.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

impl ConsoleHelper {
    /// The shipped binary as its own helper: `<exe> __console-ctrl`.
    pub fn current_exe() -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec![HELPER_ARG.into()],
        })
    }

    /// Sends Ctrl-C or Ctrl-Break to `pid`'s console. Blocking.
    pub(crate) fn send(&self, pid: u32, request: Request) -> Result<(), HelperError> {
        self.run(pid, request).map(drop)
    }

    /// PIDs attached to `pid`'s console. Blocking.
    pub(crate) fn list(&self, pid: u32) -> Result<Vec<u32>, HelperError> {
        let output = self.run(pid, Request::List)?;
        parse_list(&output).ok_or(HelperError::Output)
    }

    /// Runs one request and returns the helper's standard output.
    fn run(&self, pid: u32, request: Request) -> Result<String, HelperError> {
        use devlanes_sys::windows::process::DETACHED_PROCESS;
        use std::io::Read;
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new(&self.program)
            .args(&self.args)
            .env(REQUEST_VAR, format!("{pid} {}", request.word()))
            // No console of its own, so it can attach to the unit's.
            .creation_flags(DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
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
        let mut output = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_string(&mut output);
        }
        /// `STATUS_CONTROL_C_EXIT`: ended by a console control event.
        const ENDED_BY_EVENT: i32 = 0xC000_013A_u32.cast_signed();
        match status.code() {
            Some(0) => Ok(output),
            // The helper is attached to the console it signals, and Windows
            // delivers the event on a thread of its own that can race the
            // helper's exit and end it. Being ended by the event proves it went
            // out, so for a send this is success.
            Some(ENDED_BY_EVENT) if request != Request::List => Ok(output),
            Some(code) => Err(
                match devlanes_sys::windows::ctrl::CtrlError::from_exit_code(code) {
                    Some(error) => HelperError::Ctrl(error),
                    None => HelperError::Exit(code),
                },
            ),
            None => Err(HelperError::Exit(-1)),
        }
    }
}

/// Why a helper request failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HelperError {
    #[error("starting the console helper failed: {0}")]
    Spawn(std::io::Error),
    #[error("the console helper did not finish in time")]
    Timeout,
    #[error("the console helper could not attach: {0}")]
    Ctrl(devlanes_sys::windows::ctrl::CtrlError),
    #[error("the console helper exited with {0}")]
    Exit(i32),
    #[error("the console helper printed no process list")]
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests round-trip through their words; anything else is refused.
    #[test]
    fn requests_parse() {
        for request in [Request::CtrlC, Request::CtrlBreak, Request::List] {
            assert_eq!(
                Request::parse(&format!("42 {}", request.word())),
                Some((42, request))
            );
        }
        assert_eq!(Request::parse("42"), None);
        assert_eq!(Request::parse("x break"), None);
        assert_eq!(Request::parse("42 kill"), None);
    }

    /// The last list line counts, whatever else the helper printed.
    #[test]
    fn lists_parse_from_noisy_output() {
        let output = "running 1 test\ntest x ... console-pids: 1 2\nconsole-pids: 3 4 5\nok\n";
        assert_eq!(parse_list(output), Some(vec![3, 4, 5]));
        assert_eq!(parse_list("console-pids:"), Some(vec![]));
        assert_eq!(parse_list("nothing"), None);
        assert_eq!(parse_list("console-pids: 1 x"), None);
    }
}
