//! Console modes and console text I/O for the CLI: raw key input for `attach`,
//! escape-sequence output, and prompts that hide what the user types.
//!
//! Mode changes apply to the console buffer behind a handle, not to the handle,
//! so they are visible to every process sharing the console until restored;
//! [`ModeGuard`] puts the previous mode back.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, GetLastError, SetLastError};
use windows_sys::Win32::System::Console::{
    self as win, GetConsoleMode, ReadConsoleW, SetConsoleMode, WriteConsoleW,
};

/// Input mode: typed characters are echoed (needs [`LINE_INPUT`]).
pub const ECHO_INPUT: u32 = win::ENABLE_ECHO_INPUT;
/// Input mode: reads return whole edited lines.
pub const LINE_INPUT: u32 = win::ENABLE_LINE_INPUT;
/// Input mode: Ctrl-C raises a console control event instead of being read.
pub const PROCESSED_INPUT: u32 = win::ENABLE_PROCESSED_INPUT;
/// Input mode: special keys arrive as VT escape sequences, as on a Unix terminal.
pub const VIRTUAL_TERMINAL_INPUT: u32 = win::ENABLE_VIRTUAL_TERMINAL_INPUT;
/// Output mode: VT escape sequences (colors, cursor movement) are interpreted.
pub const VIRTUAL_TERMINAL_PROCESSING: u32 = win::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
/// Output mode: a line feed does not imply a carriage return, and wrapping at the
/// last column is deferred, as on a VT terminal.
pub const DISABLE_NEWLINE_AUTO_RETURN: u32 = win::DISABLE_NEWLINE_AUTO_RETURN;

/// Opens the console input buffer (`CONIN$`) of this process's console, also
/// when stdin is redirected. Fails when the process has no console.
pub fn open_input() -> io::Result<File> {
    File::options().read(true).write(true).open("CONIN$")
}

/// Opens the active console screen buffer (`CONOUT$`), also when stdout and
/// stderr are redirected. Fails when the process has no console.
pub fn open_output() -> io::Result<File> {
    File::options().read(true).write(true).open("CONOUT$")
}

/// The console mode of `handle`. Fails for handles that are not a console
/// buffer (files, pipes, MSYS pseudo terminals), so it doubles as a console check.
pub fn mode(handle: impl AsHandle) -> io::Result<u32> {
    let mut mode = 0;
    // SAFETY: the handle is borrowed for the call and `mode` is a writable u32.
    if unsafe { GetConsoleMode(handle.as_handle().as_raw_handle(), &raw mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}

/// Sets the console mode of `handle`. Fails for handles that are not a console
/// buffer, and for output flags the console host does not know (VT processing on
/// consoles older than Windows 10 1511).
pub fn set_mode(handle: impl AsHandle, mode: u32) -> io::Result<()> {
    // SAFETY: the handle is borrowed for the call; the mode is passed by value.
    if unsafe { SetConsoleMode(handle.as_handle().as_raw_handle(), mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads UTF-16 units from a console input handle into `buf`, returning how
/// many were read.
///
/// Unlike `std::io::stdin`, Ctrl-Z is returned as a character rather than as end
/// of input, which matters in raw mode where it is a key for the program behind
/// the console. A read that Ctrl-C or Ctrl-Break cancels (processed input mode)
/// fails with [`io::ErrorKind::Interrupted`]. A surrogate pair may be split
/// across two reads.
pub fn read_input(handle: impl AsHandle, buf: &mut [u16]) -> io::Result<usize> {
    let capacity = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let mut read = 0;
    // SAFETY: SetLastError only writes this thread's error slot. ReadConsoleW gets
    // a borrowed handle, a writable buffer of at least `capacity` units, a writable
    // count and no input control, which is allowed.
    let ok = unsafe {
        SetLastError(0);
        ReadConsoleW(
            handle.as_handle().as_raw_handle(),
            buf.as_mut_ptr().cast(),
            capacity,
            &raw mut read,
            ptr::null(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // A Ctrl-C during a cooked read ends it with success, no characters and this code.
    // SAFETY: reads this thread's last-error value.
    if read == 0 && unsafe { GetLastError() } == ERROR_OPERATION_ABORTED {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "console read cancelled by Ctrl-C",
        ));
    }
    Ok(read as usize)
}

/// Writes `text` to a console output handle as UTF-16, so it shows correctly
/// whatever the console code page is.
pub fn write_output(handle: impl AsHandle, text: &str) -> io::Result<()> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut rest = units.as_slice();
    while !rest.is_empty() {
        let chunk = u32::try_from(rest.len()).unwrap_or(u32::MAX);
        let mut written = 0;
        // SAFETY: the handle is borrowed for the call; `rest` holds at least `chunk`
        // readable units and `written` is a writable u32; the reserved pointer is null.
        let ok = unsafe {
            WriteConsoleW(
                handle.as_handle().as_raw_handle(),
                rest.as_ptr(),
                chunk,
                &raw mut written,
                ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        rest = rest.get(written as usize..).unwrap_or_default();
    }
    Ok(())
}

/// A console mode change that is undone on drop, also on error paths, so the
/// user's shell gets its console back as it was.
pub struct ModeGuard<H: AsHandle> {
    handle: H,
    previous: u32,
}

impl<H: AsHandle> ModeGuard<H> {
    /// Sets the `set` flags and clears the `clear` flags on the console buffer
    /// behind `handle`. Fails, changing nothing, when `handle` is not a console or
    /// the console host rejects the new mode.
    pub fn change(handle: H, set: u32, clear: u32) -> io::Result<Self> {
        let previous = mode(&handle)?;
        set_mode(&handle, (previous | set) & !clear)?;
        Ok(Self { handle, previous })
    }
}

impl<H: AsHandle> Drop for ModeGuard<H> {
    /// Restores the previous mode; there is nothing useful to do if that fails.
    fn drop(&mut self) {
        let _ = set_mode(&self.handle, self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_console_handles_are_refused() {
        let file = tempfile::tempfile().expect("temp file");
        assert!(mode(&file).is_err());
        assert!(ModeGuard::change(&file, VIRTUAL_TERMINAL_PROCESSING, 0).is_err());
    }
}
