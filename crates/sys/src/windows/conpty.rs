//! Pseudo consoles (ConPTY): a terminal for units started with `pty: true`.
//!
//! A pseudo console lives as long as its handle. Its children receive
//! `CTRL_CLOSE_EVENT` when it closes and are ended about five seconds later, so
//! units in a pseudo console do not outlive the daemon (pipe units do).
//!
//! Its output is the console host's rendering of the screen, not what the
//! programs wrote: long lines are split with absolute cursor moves for the
//! console's size, and a resize repaints the whole window (still so on Windows
//! 11 build 26300).

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};

use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::core::HRESULT;

/// A pseudo console. Closing it ends the processes attached to it.
#[derive(Debug)]
pub struct PseudoConsole {
    handle: HPCON,
}

// SAFETY: an HPCON is a plain handle value; the API may be called from any thread.
unsafe impl Send for PseudoConsole {}
// SAFETY: as above; resizing concurrently is allowed by the API.
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    /// Creates a console of `columns` x `rows` that reads its input from `input`
    /// (a pipe's read end) and writes its output to `output` (a pipe's write end).
    /// The console duplicates both, so the caller should close its copies of
    /// these ends once the child is started, or end of file never comes.
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
        hresult(result)?;
        Ok(Self { handle })
    }

    /// Changes the console's size; the attached programs see a resize.
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        // SAFETY: the handle stays valid until `self` is dropped.
        hresult(unsafe { ResizePseudoConsole(self.handle, size(columns, rows)?) })
    }

    /// The value `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes: the handle value
    /// itself, not a pointer to it.
    pub(crate) fn attribute_value(&self) -> *const c_void {
        self.handle as *const c_void
    }
}

impl Drop for PseudoConsole {
    /// Closes the console. From Windows 11 24H2 (build 26100), the oldest
    /// devlanes supports, this returns at once even while the output pipe is
    /// not drained.
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

/// Maps a failed `HRESULT` to an I/O error.
fn hresult(result: HRESULT) -> io::Result<()> {
    if result < 0 {
        // HRESULT_FROM_WIN32 keeps the Win32 code in the low 16 bits.
        let code = if (result as u32) & 0xFFFF_0000 == 0x8007_0000 {
            (result as u32 & 0xFFFF).cast_signed()
        } else {
            result
        };
        return Err(io::Error::from_raw_os_error(code));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::windows::io::AsHandle;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::windows::spawn::{Spawn, Stdio, spawn};

    /// A child in a pseudo console writes to it, reads typed input from it, and
    /// output ends once the console is closed.
    #[test]
    fn child_runs_in_a_pseudo_console() -> io::Result<()> {
        let (input_read, mut input_write) = io::pipe()?;
        let (mut output_read, output_write) = io::pipe()?;
        let console =
            PseudoConsole::create(100, 30, input_read.as_handle(), output_write.as_handle())?;
        // The console holds its own duplicates of these ends.
        drop((input_read, output_write));
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let cmd = Path::new(&root).join(r"System32\cmd.exe");
        let child = spawn(&Spawn {
            program: &cmd,
            // Delayed expansion (`!NAME!`), since `%NAME%` expands before `set /p` runs.
            command_line: std::ffi::OsStr::new(
                r#"cmd.exe /d /q /v:on /c "set /p NAME=& echo hello !NAME!""#,
            ),
            environment: None,
            cwd: &std::env::temp_dir(),
            stdout: Stdio::Null,
            stderr: Stdio::Null,
            job: None,
            console: Some(&console),
        })?;
        console.resize(120, 40)?;
        input_write.write_all(b"acme\r")?;
        // Output only ends once the console closes, so read on a thread.
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let _ = output_read.read_to_end(&mut output);
            String::from_utf8_lossy(&output).into_owned()
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.exit_code()?.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(child.exit_code()?, Some(0));
        drop(console);
        let output = reader
            .join()
            .map_err(|_| io::Error::other("reader panicked"))?;
        assert!(output.contains("hello acme"), "{output:?}");
        Ok(())
    }

    /// The console host passes a clear of the screen and its scrollback (`cls`
    /// here; `Clear-Host` and Git's `clear` too) on as `ESC[3J`, which the
    /// daemon's screen model takes as the end of a rendered unit's history.
    /// Its own `ESC[2J` at startup comes without one.
    #[test]
    fn a_clear_screen_reaches_the_terminal_as_a_scrollback_clear() -> io::Result<()> {
        let (input_read, _input_write) = io::pipe()?;
        let (mut output_read, output_write) = io::pipe()?;
        let console =
            PseudoConsole::create(100, 30, input_read.as_handle(), output_write.as_handle())?;
        drop((input_read, output_write));
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let cmd = Path::new(&root).join(r"System32\cmd.exe");
        let child = spawn(&Spawn {
            program: &cmd,
            command_line: std::ffi::OsStr::new(r#"cmd.exe /d /q /c "echo one & cls & echo two""#),
            environment: None,
            cwd: &std::env::temp_dir(),
            stdout: Stdio::Null,
            stderr: Stdio::Null,
            job: None,
            console: Some(&console),
        })?;
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let _ = output_read.read_to_end(&mut output);
            String::from_utf8_lossy(&output).into_owned()
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.exit_code()?.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(console);
        let output = reader
            .join()
            .map_err(|_| io::Error::other("reader panicked"))?;
        let clear = output.find("\x1b[3J");
        let (one, two) = (output.find("one"), output.find("two"));
        assert!(one < clear && clear < two && one.is_some(), "{output:?}");
        Ok(())
    }

    /// Closing returns at once while the output pipe is full and nobody reads
    /// it, which before 24H2 blocked until the pipe was drained.
    #[test]
    fn closing_does_not_wait_for_undrained_output() -> io::Result<()> {
        let (input_read, _input_write) = io::pipe()?;
        let (output_read, output_write) = io::pipe()?;
        let console =
            PseudoConsole::create(100, 30, input_read.as_handle(), output_write.as_handle())?;
        drop((input_read, output_write));
        let job = crate::windows::job::Job::create()?;
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let cmd = Path::new(&root).join(r"System32\cmd.exe");
        let _child = spawn(&Spawn {
            program: &cmd,
            command_line: std::ffi::OsStr::new(
                r#"cmd.exe /d /q /c "for /l %i in (1,1,1000000) do @echo line %i""#,
            ),
            environment: None,
            cwd: &std::env::temp_dir(),
            stdout: Stdio::Null,
            stderr: Stdio::Null,
            job: Some(&job),
            console: Some(&console),
        })?;
        // Long enough to fill the pipe, which nothing reads.
        std::thread::sleep(Duration::from_millis(500));
        let (closed, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(console);
            let _ = closed.send(());
        });
        let returned = done.recv_timeout(Duration::from_secs(5)).is_ok();
        let _ = job.terminate(1);
        drop(output_read);
        assert!(
            returned,
            "ClosePseudoConsole waited for the output to drain"
        );
        Ok(())
    }

    /// Sizes outside what a console can hold are refused before any call.
    #[test]
    fn sizes_are_checked() {
        assert!(size(0, 10).is_err());
        assert!(size(10, 0).is_err());
        assert!(size(40_000, 10).is_err());
        assert!(size(120, 40).is_ok());
    }
}
