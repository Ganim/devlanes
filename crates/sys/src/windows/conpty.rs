//! Pseudo consoles (ConPTY): a terminal for units started with `pty: true`.
//!
//! A redistributed `conpty.dll` next to the executable, with its `OpenConsole.exe`,
//! is used when present: it fixes rendering and resize bugs of older inbox
//! versions. Otherwise the functions in `kernel32.dll` are used.
//!
//! A pseudo console lives as long as its handle. Its children receive
//! `CTRL_CLOSE_EVENT` when it closes and are ended about five seconds later, so
//! units in a pseudo console do not outlive the daemon (pipe units do).

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::Path;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::core::HRESULT;

use super::security::wide;

/// `CreatePseudoConsole`.
type CreateFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
/// `ResizePseudoConsole`.
type ResizeFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
/// `ClosePseudoConsole`.
type CloseFn = unsafe extern "system" fn(HPCON);

/// The three entry points, from the redistributed DLL or from kernel32.
#[derive(Clone, Copy)]
struct Api {
    create: CreateFn,
    resize: ResizeFn,
    close: CloseFn,
    bundled: bool,
}

/// Loaded once per process; the DLL stays loaded for the process's lifetime.
static API: OnceLock<Api> = OnceLock::new();

/// The API in use, loading the redistributed DLL on first use when present.
fn api() -> Api {
    *API.get_or_init(|| {
        bundled().unwrap_or(Api {
            create: windows_sys::Win32::System::Console::CreatePseudoConsole,
            resize: windows_sys::Win32::System::Console::ResizePseudoConsole,
            close: windows_sys::Win32::System::Console::ClosePseudoConsole,
            bundled: false,
        })
    })
}

/// The redistributed `conpty.dll` beside the executable. It starts
/// `OpenConsole.exe` from its own directory, so both must be there.
fn bundled() -> Option<Api> {
    let directory = std::env::current_exe().ok()?.parent()?.to_owned();
    let dll = directory.join("conpty.dll");
    if !dll.is_file() || !directory.join("OpenConsole.exe").is_file() {
        return None;
    }
    load(&dll)
}

/// Loads `dll` by full path and resolves the three functions.
fn load(dll: &Path) -> Option<Api> {
    let path = wide(dll.as_os_str());
    // SAFETY: `path` is NUL-terminated; the flags restrict the DLL's own
    // dependencies to its directory and System32, so nothing is found on PATH.
    let module = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    // A DLL that does not load leaves the system's ConPTY in use, which
    // `bundled_in_use` lets `doctor` report.
    if module.is_null() {
        return None;
    }
    let symbol = |name: &[u8]| -> Option<unsafe extern "system" fn() -> isize> {
        // SAFETY: `module` is loaded and `name` is NUL-terminated.
        unsafe { GetProcAddress(module, name.as_ptr()) }
    };
    let (create, resize, close) = (
        symbol(b"CreatePseudoConsole\0")?,
        symbol(b"ResizePseudoConsole\0")?,
        symbol(b"ClosePseudoConsole\0")?,
    );
    // SAFETY: the redistributed DLL exports these with the documented
    // kernel32 signatures; only the pointer type changes.
    unsafe {
        Some(Api {
            create: std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateFn>(create),
            resize: std::mem::transmute::<unsafe extern "system" fn() -> isize, ResizeFn>(resize),
            close: std::mem::transmute::<unsafe extern "system" fn() -> isize, CloseFn>(close),
            bundled: true,
        })
    }
}

/// Whether the redistributed `conpty.dll` is in use, for `doctor`.
pub fn bundled_in_use() -> bool {
    api().bundled
}

/// A pseudo console. Closing it ends the processes attached to it.
#[derive(Debug)]
pub struct PseudoConsole {
    handle: HPCON,
    close: CloseFn,
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
        let api = api();
        let mut handle: HPCON = 0;
        // SAFETY: both handles are open for the call; `handle` is writable.
        let result = unsafe {
            (api.create)(
                size(columns, rows)?,
                input.as_raw_handle(),
                output.as_raw_handle(),
                0,
                &raw mut handle,
            )
        };
        hresult(result)?;
        Ok(Self {
            handle,
            close: api.close,
        })
    }

    /// Changes the console's size; the attached programs see a resize.
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        // SAFETY: the handle stays valid until `self` is dropped.
        hresult(unsafe { (api().resize)(self.handle, size(columns, rows)?) })
    }

    /// The value `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes.
    pub(crate) fn raw(&self) -> HPCON {
        self.handle
    }
}

impl Drop for PseudoConsole {
    /// Closes the console on a separate thread. Older Windows versions block in
    /// `ClosePseudoConsole` until its output pipe is drained, and the caller may
    /// be the one that would drain it.
    fn drop(&mut self) {
        let (handle, close) = (self.handle, self.close);
        let closing = std::thread::Builder::new()
            .name("devlanes-conpty-close".into())
            // SAFETY: the handle is closed exactly once, here.
            .spawn(move || unsafe { close(handle) });
        if closing.is_err() {
            // No thread: close inline rather than leak the console and its children.
            // SAFETY: as above; the spawn failed, so the closure never ran.
            unsafe { close(handle) };
        }
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

/// The pseudo-console attribute takes the handle value itself, not a pointer to it.
pub(crate) fn attribute_value(console: &PseudoConsole) -> *const c_void {
    console.raw() as *const c_void
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
        let child = spawn(&Spawn {
            program: Path::new(r"C:\Windows\System32\cmd.exe"),
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

    /// Sizes outside what a console can hold are refused before any call.
    #[test]
    fn sizes_are_checked() {
        assert!(size(0, 10).is_err());
        assert!(size(10, 0).is_err());
        assert!(size(40_000, 10).is_err());
        assert!(size(120, 40).is_ok());
    }
}
