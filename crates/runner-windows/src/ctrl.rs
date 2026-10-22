//! The console-control helper: a process without a console of its own that
//! attaches to a unit's console to send it Ctrl-C or Ctrl-Break, or that stays
//! attached (an anchor) to list the processes on that console when asked.
//!
//! The daemon has no console and a process can attach to only one at a time,
//! so the runner starts helpers without a console (`DETACHED_PROCESS`). A send is
//! one short-lived helper. A pipe task gets one anchor for its whole run: it
//! attaches while the task's leader is alive and keeps the console, and so its
//! process list, alive after the leader exits, which is when the runner needs
//! it (decision Q9). The helper is any program that calls [`console_ctrl_main`]
//! when the request variable is set: `devlanes __console-ctrl` in the shipped
//! binary, the test binary in tests.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// The environment variable carrying the request, `<pid> <c|break|anchor>`. An
/// environment variable rather than arguments, so any program can be the helper
/// whatever its own argument parsing.
pub const REQUEST_VAR: &str = "DEVLANES_CONSOLE_CTRL";

/// The hidden argument `devlanes` dispatches to [`console_ctrl_main`] on.
pub const HELPER_ARG: &str = "__console-ctrl";

/// Prefix of an anchor's answers. Lines without it (a test harness's own
/// output) are ignored.
const ANSWER_PREFIX: &str = "console-anchor:";

/// What a helper is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Send `CTRL_C_EVENT`.
    CtrlC,
    /// Send `CTRL_BREAK_EVENT`.
    CtrlBreak,
    /// Stay attached and answer commands on standard input until it closes.
    Anchor,
}

impl Request {
    /// The request's word in [`REQUEST_VAR`], also an anchor command for sends.
    fn word(self) -> &'static str {
        match self {
            Self::CtrlC => "c",
            Self::CtrlBreak => "break",
            Self::Anchor => "anchor",
        }
    }

    /// Parses `<pid> <word>`.
    fn parse(text: &str) -> Option<(u32, Self)> {
        let (pid, word) = text.trim().split_once(' ')?;
        let request = match word {
            "c" => Self::CtrlC,
            "break" => Self::CtrlBreak,
            "anchor" => Self::Anchor,
            _ => return None,
        };
        Some((pid.parse().ok()?, request))
    }
}

/// What follows [`ANSWER_PREFIX`] on an answer line, trimmed.
fn answer(line: &str) -> Option<&str> {
    line.split_once(ANSWER_PREFIX).map(|(_, rest)| rest.trim())
}

/// PIDs from a `pids 1 2 3` answer.
fn parse_pids(answer: &str) -> Option<Vec<u32>> {
    answer
        .strip_prefix("pids")?
        .split_whitespace()
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
        Request::Anchor => ctrl::Attached::to(pid).map(|attached| serve_anchor(&attached)),
    };
    match result {
        Ok(()) => 0,
        Err(error) => error.exit_code(),
    }
}

/// Answers `list`, `c` and `break` lines on standard input until it closes.
fn serve_anchor(attached: &devlanes_sys::windows::ctrl::Attached) {
    use devlanes_sys::windows::ctrl::CtrlEvent;
    let mut out = std::io::stdout();
    if writeln!(out, "{ANSWER_PREFIX} attached").is_err() {
        return;
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        let reply = match line.trim() {
            "list" => match attached.processes() {
                Ok(pids) => {
                    let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
                    format!("pids {}", pids.join(" "))
                }
                Err(error) => format!("error {error}"),
            },
            word @ ("c" | "break") => {
                let event = if word == "c" {
                    CtrlEvent::CtrlC
                } else {
                    CtrlEvent::CtrlBreak
                };
                match attached.send(event) {
                    Ok(()) => "sent".to_owned(),
                    Err(error) => format!("error {error}"),
                }
            }
            other => format!("error unknown command {other}"),
        };
        if writeln!(out, "{ANSWER_PREFIX} {reply}")
            .and_then(|()| out.flush())
            .is_err()
        {
            return;
        }
    }
}

/// How the runner starts the helper: `program` with `args` before anything else.
#[derive(Debug, Clone)]
pub struct ConsoleHelper {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// How long a helper (or one anchor answer) may take before it is given up on.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

impl ConsoleHelper {
    /// The shipped binary as its own helper: `<exe> __console-ctrl`.
    pub fn current_exe() -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec![HELPER_ARG.into()],
        })
    }

    /// Sends Ctrl-Break to every process attached to `pid`'s console, the
    /// graceful stop for `SIGTERM`; `true` when the event went out. For processes
    /// no runner supervises any more (a previous daemon's orphans); hold the
    /// process open meanwhile so its PID cannot name a stranger. Blocking.
    pub fn ask_to_stop(&self, pid: u32) -> bool {
        self.send(pid, Request::CtrlBreak).is_ok()
    }

    /// A helper process for `request` on `pid`, without a console of its own so
    /// it can attach to the unit's.
    fn command(&self, pid: u32, request: Request) -> std::process::Command {
        use devlanes_sys::windows::process::DETACHED_PROCESS;
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new(&self.program);
        command
            .args(&self.args)
            .env(REQUEST_VAR, format!("{pid} {}", request.word()))
            .creation_flags(DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Sends Ctrl-C or Ctrl-Break to `pid`'s console through a short-lived
    /// helper. Blocking.
    pub(crate) fn send(&self, pid: u32, request: Request) -> Result<(), HelperError> {
        let mut child = self
            .command(pid, request)
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
            // helper's exit and end it. Being ended by the event proves it went
            // out (the ignoring handler is installed before attaching, so no
            // other event can do it), so this is success.
            Some(0 | ENDED_BY_EVENT) => Ok(()),
            Some(code) => Err(exit_error(code)),
            None => Err(HelperError::Exit(-1)),
        }
    }

    /// Starts an anchor on `pid`'s console and waits until it is attached.
    /// Blocking. The caller keeps `pid` from being reused meanwhile (it holds the
    /// unit leader's handle).
    pub(crate) fn anchor(&self, pid: u32) -> Result<Anchor, HelperError> {
        let mut child = self
            .command(pid, Request::Anchor)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(HelperError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::Output);
        };
        // A thread reads answers, so a stuck helper costs a timeout, not a hang.
        let (sender, answers) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("devlanes-console-anchor".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { return };
                    if let Some(answer) = answer(&line)
                        && sender.send(answer.to_owned()).is_err()
                    {
                        return;
                    }
                }
            });
        if let Err(error) = reader {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::Spawn(error));
        }
        let mut anchor = Anchor {
            child,
            stdin: Some(stdin),
            answers,
        };
        match anchor.next_answer() {
            Ok(answer) if answer == "attached" => Ok(anchor),
            Ok(_) => Err(HelperError::Output),
            // No answer: it exited, most likely failing to attach.
            Err(_) => {
                let code = anchor.wait_exit();
                Err(code.map_or(HelperError::Timeout, exit_error))
            }
        }
    }
}

/// The error a helper's exit code stands for.
fn exit_error(code: i32) -> HelperError {
    match devlanes_sys::windows::ctrl::CtrlError::from_exit_code(code) {
        Some(error) => HelperError::Ctrl(error),
        None => HelperError::Exit(code),
    }
}

/// A helper attached to a unit's console for as long as this lives. Dropping it
/// ends the helper; the console then lives on only while others are attached.
#[derive(Debug)]
pub(crate) struct Anchor {
    child: Child,
    stdin: Option<ChildStdin>,
    answers: mpsc::Receiver<String>,
}

impl Anchor {
    /// PIDs attached to the console now, the anchor excluded. Blocking.
    pub(crate) fn list(&mut self) -> Result<Vec<u32>, HelperError> {
        let answer = self.ask("list")?;
        parse_pids(&answer).ok_or(HelperError::Output)
    }

    /// Sends Ctrl-C or Ctrl-Break to the console. Blocking.
    pub(crate) fn send(&mut self, request: Request) -> Result<(), HelperError> {
        match self.ask(request.word())?.as_str() {
            "sent" => Ok(()),
            _ => Err(HelperError::Output),
        }
    }

    /// Writes `command` and waits for its answer.
    fn ask(&mut self, command: &str) -> Result<String, HelperError> {
        let stdin = self.stdin.as_mut().ok_or(HelperError::Output)?;
        writeln!(stdin, "{command}")
            .and_then(|()| stdin.flush())
            .map_err(HelperError::Spawn)?;
        self.next_answer()
    }

    /// The next answer line, within [`HELPER_TIMEOUT`].
    fn next_answer(&self) -> Result<String, HelperError> {
        self.answers
            .recv_timeout(HELPER_TIMEOUT)
            .map_err(|_| HelperError::Timeout)
    }

    /// The helper's exit code once it exited, ending it if it does not within
    /// [`HELPER_TIMEOUT`].
    fn wait_exit(&mut self) -> Option<i32> {
        self.stdin.take();
        let deadline = Instant::now() + HELPER_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return None;
                }
            }
        }
    }
}

impl Drop for Anchor {
    /// Closing its input ends the helper; one that does not exit is ended.
    fn drop(&mut self) {
        self.wait_exit();
    }
}

/// Why a helper request failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HelperError {
    #[error("starting or talking to the console helper failed: {0}")]
    Spawn(std::io::Error),
    #[error("the console helper did not answer in time")]
    Timeout,
    #[error("the console helper could not attach: {0}")]
    Ctrl(devlanes_sys::windows::ctrl::CtrlError),
    #[error("the console helper exited with {0}")]
    Exit(i32),
    #[error("the console helper answered something unexpected")]
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests round-trip through their words; anything else is refused.
    #[test]
    fn requests_parse() {
        for request in [Request::CtrlC, Request::CtrlBreak, Request::Anchor] {
            assert_eq!(
                Request::parse(&format!("42 {}", request.word())),
                Some((42, request))
            );
        }
        assert_eq!(Request::parse("42"), None);
        assert_eq!(Request::parse("x break"), None);
        assert_eq!(Request::parse("42 kill"), None);
    }

    /// Not a test of its own: this binary as the helper when started with a
    /// request. Without one it does nothing.
    #[test]
    fn helper_entry() {
        if std::env::var_os(REQUEST_VAR).is_some() {
            std::process::exit(i32::from(console_ctrl_main()));
        }
    }

    /// An anchor attaches to a process's console, lists it and sends to it; its
    /// errors are shown whole, since they differ between machines (CI runners).
    #[test]
    fn anchor_lists_and_signals_a_console() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut ping = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let helper = ConsoleHelper {
            program: std::env::current_exe()?,
            args: ["--exact", "ctrl::tests::helper_entry", "--nocapture"]
                .map(Into::into)
                .into(),
        };
        let result = (|| -> Result<(), HelperError> {
            let mut anchor = helper.anchor(ping.id())?;
            let pids = anchor.list()?;
            assert!(pids.contains(&ping.id()), "{pids:?} lacks {}", ping.id());
            anchor.send(Request::CtrlC)?;
            Ok(())
        })();
        let _ = ping.kill();
        let _ = ping.wait();
        result.map_err(|error| format!("{error:?}: {error}"))?;
        Ok(())
    }

    /// Answers are found inside other output, and PID lists parse strictly.
    #[test]
    fn answers_parse_from_noisy_output() {
        assert_eq!(
            answer("test x ... console-anchor: attached"),
            Some("attached")
        );
        assert_eq!(answer("running 1 test"), None);
        assert_eq!(parse_pids("pids 3 4 5"), Some(vec![3, 4, 5]));
        assert_eq!(parse_pids("pids"), Some(vec![]));
        assert_eq!(parse_pids("pids 1 x"), None);
        assert_eq!(parse_pids("sent"), None);
    }
}
