//! Console control events for another process's console: what a graceful stop
//! sends to a unit. Windows has no signals; Ctrl-C and Ctrl-Break are events of a
//! console, delivered to every process attached to it.
//!
//! The daemon has no console, and a process can be attached to only one at a
//! time, so these run in a short-lived helper process started without a console
//! (`DETACHED_PROCESS`). The helper attaches to the unit's console, sends the
//! event to process group 0 (everything attached, which is the unit's processes;
//! `CTRL_C_EVENT` cannot target any other group), and exits.

use std::io;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
};
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
    SetConsoleCtrlHandler,
};
use windows_sys::core::BOOL;

/// A console control event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlEvent {
    /// `CTRL_C_EVENT`: what Ctrl-C in a console sends.
    CtrlC,
    /// `CTRL_BREAK_EVENT`: ends a process with the default handler, and is the
    /// event tools treat as a request to stop (Node reports it as `SIGBREAK`).
    CtrlBreak,
}

/// Why attaching to a process's console failed.
#[derive(Debug, thiserror::Error)]
pub enum CtrlError {
    /// Windows refused the attach (`ERROR_ACCESS_DENIED`): the target is a
    /// process this one may not attach to, or the console is going away.
    #[error("attaching to the console was refused")]
    AttachDenied,
    /// The target process has no console (a GUI program or `DETACHED_PROCESS`).
    #[error("the process has no console")]
    NoConsole,
    /// No process has that ID.
    #[error("no such process")]
    NoProcess,
    #[error("console call failed: {0}")]
    Io(#[from] io::Error),
}

impl CtrlError {
    /// The helper's exit code for this error, so the daemon can tell the cases
    /// apart without parsing text.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::AttachDenied => 3,
            Self::NoConsole => 4,
            Self::NoProcess => 5,
            Self::Io(_) => 1,
        }
    }
}

/// This process attached to another's console, with control events ignored,
/// until dropped.
#[derive(Debug)]
pub struct Attached(());

impl Attached {
    /// Attaches to `pid`'s console, ignoring the events sent to it from here on.
    /// Only call it from a process started without a console: a process can
    /// attach to another's only while it has none of its own.
    pub fn to(pid: u32) -> Result<Self, CtrlError> {
        // The ignore flag survives the attach, so Ctrl-C is covered from the
        // first moment; other events only once the handler below is in place.
        super::process::ignore_ctrl_c()?;
        // SAFETY: harmless without a console.
        unsafe { FreeConsole() };
        // SAFETY: plain call; failures are mapped below.
        if unsafe { AttachConsole(pid) } == 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_ACCESS_DENIED) => CtrlError::AttachDenied,
                Some(ERROR_INVALID_HANDLE) => CtrlError::NoConsole,
                Some(ERROR_INVALID_PARAMETER) => CtrlError::NoProcess,
                _ => CtrlError::Io(error),
            });
        }
        // After the attach, not before: on Windows 11 build 26300 a handler
        // registered before AttachConsole no longer ran after it, and the next
        // event (even one this process sent) ended it with
        // STATUS_CONTROL_C_EXIT. `helpers_signal_a_console` in
        // devlanes-runner-windows covers this.
        // SAFETY: `ignore` is a valid handler for the life of the process.
        if unsafe { SetConsoleCtrlHandler(Some(ignore), 1) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: detaches from the console attached above.
            unsafe { FreeConsole() };
            return Err(error.into());
        }
        Ok(Self(()))
    }

    /// Sends `event` to every process attached to the console.
    pub fn send(&self, event: CtrlEvent) -> Result<(), CtrlError> {
        let event = match event {
            CtrlEvent::CtrlC => CTRL_C_EVENT,
            CtrlEvent::CtrlBreak => CTRL_BREAK_EVENT,
        };
        // SAFETY: plain call; group 0 is every process attached to this console.
        if unsafe { GenerateConsoleCtrlEvent(event, 0) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        // The handler stays installed: events still in flight would otherwise end
        // the helper before it exits on its own.
        // SAFETY: detaches from the console attached in `to`.
        unsafe { FreeConsole() };
    }
}

/// Handles every control event by ignoring it. Needed besides the ignore flag,
/// which only covers Ctrl-C; once attached, an event someone else sends to the
/// console would otherwise end this process with the default handler.
extern "system" fn ignore(_event: u32) -> BOOL {
    1
}
