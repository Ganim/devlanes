//! Pseudo consoles (ConPTY), the terminal for units started with `pty: true`.
//!
//! A pseudo console lives as long as its handle. Its processes receive
//! `CTRL_CLOSE_EVENT` when it closes and end about five seconds later. Start
//! processes in it through [`crate::spawn`] with
//! [`crate::spawn::Stdio::PseudoConsole`].

use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};

use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};

use crate::util::hresult_error;

/// A pseudo console. Dropping it closes the console, which ends the processes
/// attached to it.
#[derive(Debug)]
pub struct PseudoConsole {
    handle: HPCON,
}

// SAFETY: an HPCON is a plain handle value that the API accepts from any thread.
unsafe impl Send for PseudoConsole {}
// SAFETY: as above; `resize` from several threads is allowed by the API.
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    /// Creates a console of `columns` x `rows` that reads its input from
    /// `input` (a pipe's read end) and writes its output to `output` (a
    /// pipe's write end). The console keeps its own duplicates, so the caller
    /// closes its copies of these ends once the child started, or end of file
    /// never comes.
    pub fn create(
        columns: u16,
        rows: u16,
        input: BorrowedHandle<'_>,
        output: BorrowedHandle<'_>,
    ) -> io::Result<Self> {
        let mut handle: HPCON = 0;
        // SAFETY: both handles are open for the call; `handle` is writable.
        let result = unsafe {
            CreatePseudoConsole(
                size(columns, rows)?,
                input.as_raw_handle(),
                output.as_raw_handle(),
                0,
                &raw mut handle,
            )
        };
        if result < 0 {
            return Err(hresult_error(result));
        }
        Ok(Self { handle })
    }

    /// Changes the console's size; the attached programs see a resize.
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        // SAFETY: the handle stays valid until `self` is dropped.
        let result = unsafe { ResizePseudoConsole(self.handle, size(columns, rows)?) };
        if result < 0 {
            return Err(hresult_error(result));
        }
        Ok(())
    }

    /// The value `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes: the handle
    /// value itself, not a pointer to it.
    pub(crate) fn raw(&self) -> HPCON {
        self.handle
    }
}

impl Drop for PseudoConsole {
    /// Closes the console. On 24H2 this returns at once, also while the output
    /// pipe is not drained.
    fn drop(&mut self) {
        // SAFETY: the handle is closed exactly once, here.
        unsafe { ClosePseudoConsole(self.handle) };
    }
}

/// A console size; both dimensions must be non-zero and fit in an `i16`.
fn size(columns: u16, rows: u16) -> io::Result<COORD> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid console size");
    let x = i16::try_from(columns).map_err(|_| invalid())?;
    let y = i16::try_from(rows).map_err(|_| invalid())?;
    if x == 0 || y == 0 {
        return Err(invalid());
    }
    Ok(COORD { X: x, Y: y })
}
