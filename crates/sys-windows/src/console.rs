//! Consoles: control events for another process's console, console modes
//! for prompts, `attach` and escape-sequence output, console text I/O, a
//! console for a process started without one, and this process's standard
//! output.
//!
//! Mode changes apply to the console buffer behind a handle, so every process
//! sharing the console sees them until they are restored; [`ModeGuard`] puts
//! the previous mode back, also when a console control event ends the
//! process without running destructors.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, IsTerminal};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, IntoRawHandle};
use std::ptr;
use std::sync::{Condvar, Mutex, MutexGuard, Once, PoisonError};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    SetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType};
use windows_sys::Win32::System::Console::{
    self as win, AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, FreeConsole,
    GenerateConsoleCtrlEvent, GetConsoleCP, GetConsoleMode, GetConsoleProcessList,
    GetConsoleWindow, GetStdHandle, ReadConsoleW, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode, SetStdHandle, WriteConsoleW,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::core::BOOL;

/// Input mode: typed characters are echoed (needs [`LINE_INPUT`]).
pub const ECHO_INPUT: u32 = win::ENABLE_ECHO_INPUT;
/// Input mode: reads return whole edited lines.
pub const LINE_INPUT: u32 = win::ENABLE_LINE_INPUT;
/// Input mode: Ctrl-C raises a console control event instead of being read.
pub const PROCESSED_INPUT: u32 = win::ENABLE_PROCESSED_INPUT;
/// Input mode: special keys arrive as VT escape sequences, as on a Unix terminal.
pub const VIRTUAL_TERMINAL_INPUT: u32 = win::ENABLE_VIRTUAL_TERMINAL_INPUT;
/// Output mode: VT escape sequences (colours, cursor movement) are interpreted.
pub const VIRTUAL_TERMINAL_PROCESSING: u32 = win::ENABLE_VIRTUAL_TERMINAL_PROCESSING;

/// A console control event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlEvent {
    /// `CTRL_C_EVENT`, what Ctrl-C in a console sends.
    CtrlC,
    /// `CTRL_BREAK_EVENT`, which ends a process with the default handler;
    /// Node reports it as `SIGBREAK`.
    CtrlBreak,
}

/// Why attaching to a process's console failed.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// Windows refused the attach (`ERROR_ACCESS_DENIED`): the console is
    /// going away, or the target may not be attached to.
    #[error("attaching to the console was refused")]
    Denied,
    /// The target has no console (a GUI program, or `DETACHED_PROCESS`).
    #[error("the process has no console")]
    NoConsole,
    /// No process has that ID.
    #[error("no such process")]
    NoProcess,
    /// Another console call failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// This process attached to another process's console, with every control
/// event ignored. For a short-lived helper that sends a control event to a
/// unit's console: a process can only be attached to one console, so the
/// daemon cannot do it itself.
///
/// The process stays attached until it exits, also after this is dropped.
/// Events are delivered on threads Windows starts later; detaching before
/// that drops the ignoring handler, and the default handling would end the
/// helper with `STATUS_CONTROL_C_EXIT`.
#[derive(Debug)]
pub struct Attached(());

impl Attached {
    /// Leaves this process's console, if any, and attaches to `pid`'s.
    ///
    /// The ignore-Ctrl-C flag is set before attaching, so a Ctrl-C sent to the
    /// console is ignored from the first moment; it survives the attach. The
    /// handler that ignores every event is installed after attaching, because
    /// attaching drops handlers registered before it. Then the flag is cleared
    /// again, so Ctrl-C reaches that handler too and [`Attached::generate`]
    /// can tell when an event arrived.
    pub fn to(pid: u32) -> Result<Self, AttachError> {
        // SAFETY: harmless without a console.
        unsafe { FreeConsole() };
        set_ctrl_c_ignored(true)?;
        // SAFETY: plain call; failures are mapped below.
        if unsafe { AttachConsole(pid) } == 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error().map(i32::cast_unsigned) {
                Some(ERROR_ACCESS_DENIED) => AttachError::Denied,
                Some(ERROR_INVALID_HANDLE) => AttachError::NoConsole,
                Some(ERROR_INVALID_PARAMETER) => AttachError::NoProcess,
                _ => AttachError::Io(error),
            });
        }
        // SAFETY: `ignore` is a valid handler for the life of the process.
        if unsafe { SetConsoleCtrlHandler(Some(ignore), 1) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: detaches from the console attached above.
            unsafe { FreeConsole() };
            return Err(error.into());
        }
        set_ctrl_c_ignored(false)?;
        Ok(Self(()))
    }

    /// Sends `event` to process group 0, every process attached to the
    /// console (`CTRL_C_EVENT` cannot target any other group), and waits up to
    /// `timeout` for it to reach this process too. Returns whether it did.
    ///
    /// The console host handles the request after this call returns, and
    /// drops it when the requesting process has left by then, so a helper
    /// exits only after this returned `true` or gave up.
    pub fn generate(&self, event: CtrlEvent, timeout: Duration) -> io::Result<bool> {
        let event = match event {
            CtrlEvent::CtrlC => CTRL_C_EVENT,
            CtrlEvent::CtrlBreak => CTRL_BREAK_EVENT,
        };
        let (count, arrived) = &RECEIVED;
        let received = count.lock().unwrap_or_else(PoisonError::into_inner);
        let before = *received;
        // SAFETY: plain call; group 0 is every process attached to this console.
        if unsafe { GenerateConsoleCtrlEvent(event, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let (received, _) = arrived
            .wait_timeout_while(received, timeout, |received| *received == before)
            .unwrap_or_else(PoisonError::into_inner);
        Ok(*received != before)
    }

    /// PIDs of the processes attached to the console, this one included.
    pub fn processes(&self) -> io::Result<Vec<u32>> {
        console_processes()
    }
}

/// How many control events [`ignore`] has seen, for [`Attached::generate`].
static RECEIVED: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

/// Ignores every control event, counting it. Windows runs it on a thread of
/// its own for each event.
unsafe extern "system" fn ignore(_event: u32) -> BOOL {
    let (count, arrived) = &RECEIVED;
    *count.lock().unwrap_or_else(PoisonError::into_inner) += 1;
    arrived.notify_all();
    1
}

/// PIDs of the processes attached to this process's console. Fails without a
/// console.
pub fn console_processes() -> io::Result<Vec<u32>> {
    let mut list = vec![0_u32; 64];
    loop {
        let capacity = u32::try_from(list.len()).unwrap_or(u32::MAX);
        // SAFETY: `list` holds `capacity` writable u32s.
        let count = unsafe { GetConsoleProcessList(list.as_mut_ptr(), capacity) };
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        // A larger count than the buffer holds is the size needed.
        if count > capacity {
            list.resize(count as usize + 16, 0);
            continue;
        }
        list.truncate(count as usize);
        return Ok(list);
    }
}

/// Sets or clears this process's ignore-Ctrl-C flag, which processes it
/// starts inherit. `CREATE_NEW_PROCESS_GROUP` sets it in the new process, so
/// a process started that way clears it to let its children receive Ctrl-C.
/// Works without a console.
pub fn set_ctrl_c_ignored(ignored: bool) -> io::Result<()> {
    // SAFETY: a null handler only sets or clears the process's ignore flag.
    if unsafe { SetConsoleCtrlHandler(None, BOOL::from(ignored)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How [`alloc_console`] creates a console for a process without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocMode {
    /// What Windows does when it starts a console program without a parent
    /// console: a new console with a visible window.
    Default,
    /// A console without a window. Console programs this process starts
    /// share it, so none of them opens a window either.
    NoWindow,
}

/// What [`alloc_console`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allocated {
    /// The process had no console and now has a new one.
    New,
    /// The process already had a console and keeps it.
    Existing,
}

/// `ALLOC_CONSOLE_OPTIONS` from `consoleapi.h` (Windows 11 24H2 SDK).
#[repr(C)]
struct AllocConsoleOptions {
    mode: i32,
    use_show_window: BOOL,
    show_window: u16,
}

/// `AllocConsoleWithOptions`, which `windows-sys` 0.61 does not declare.
type AllocConsoleWithOptions =
    unsafe extern "system" fn(options: *const AllocConsoleOptions, result: *mut i32) -> i32;

/// Gives this process a console when it has none, the way `mode` says; a
/// process that has one keeps it. A program whose manifest sets
/// `consoleAllocationPolicy` to `detached` starts without a console when
/// nothing passes one down (the Run key, Explorer), and console programs it
/// starts would then each open a window of their own, so it calls this
/// before starting any. Needs Windows 11 24H2 (`AllocConsoleWithOptions`);
/// fails with [`io::ErrorKind::Unsupported`] on older builds.
pub fn alloc_console(mode: AllocMode) -> io::Result<Allocated> {
    let name = crate::util::wide(OsStr::new("kernel32.dll"))?;
    // SAFETY: kernel32 is loaded in every process; the name is NUL-terminated.
    let module = unsafe { GetModuleHandleW(name.as_ptr()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `module` is kernel32's handle and the symbol name is NUL-terminated.
    let Some(function) =
        (unsafe { GetProcAddress(module, c"AllocConsoleWithOptions".as_ptr().cast()) })
    else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AllocConsoleWithOptions needs Windows 11 24H2 or later",
        ));
    };
    // SAFETY: the export has this signature (consoleapi.h); both are
    // `extern "system"` function pointers of the same size.
    let function: AllocConsoleWithOptions = unsafe { std::mem::transmute(function) };
    let options = AllocConsoleOptions {
        mode: match mode {
            AllocMode::Default => 0,
            AllocMode::NoWindow => 2,
        },
        use_show_window: 0,
        show_window: 0,
    };
    let mut result = 0;
    // SAFETY: both pointers are valid for the call.
    let status = unsafe { function(&raw const options, &raw mut result) };
    if status < 0 {
        // An HRESULT, not a Win32 error code.
        return Err(io::Error::other(format!(
            "AllocConsoleWithOptions failed with 0x{status:08x}"
        )));
    }
    match result {
        1 => Ok(Allocated::New),
        2 => Ok(Allocated::Existing),
        other => Err(io::Error::other(format!(
            "AllocConsoleWithOptions allocated no console (result {other})"
        ))),
    }
}

/// Gives this process a console without a window ([`AllocMode::NoWindow`])
/// when it has none, keeping the standard handles its parent passed. A
/// program started without a console but with redirected streams (pipes or
/// `NUL` from a detached parent) keeps using them: `AllocConsole` is
/// documented to point every standard handle at the new console, so the
/// ones that were valid before are put back (24H2 was seen keeping pipes
/// anyway). Console programs it starts share that console, so none of them
/// opens a window. A process with a console keeps it and its handles.
pub fn alloc_hidden_console() -> io::Result<Allocated> {
    if has_console() {
        return Ok(Allocated::Existing);
    }
    let slots = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    // SAFETY: reads the standard handles without taking ownership.
    let passed = slots.map(|slot| unsafe { GetStdHandle(slot) });
    let allocated = alloc_console(AllocMode::NoWindow)?;
    for (slot, handle) in slots.into_iter().zip(passed) {
        // SAFETY: the slot gets back the value it held before the allocation;
        // the standard handle table keeps owning it.
        if usable(handle) && unsafe { SetStdHandle(slot, handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(allocated)
}

/// Whether `handle` names an open file, pipe or character device (`NUL`),
/// so it is worth keeping as a standard handle; null, `INVALID_HANDLE_VALUE`
/// and values that name nothing are not.
fn usable(handle: HANDLE) -> bool {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    // SAFETY: only queries the handle; a value that names nothing gives
    // FILE_TYPE_UNKNOWN.
    (unsafe { GetFileType(handle) }) != FILE_TYPE_UNKNOWN
}

/// Whether this process is attached to a console.
pub fn has_console() -> bool {
    // SAFETY: plain query; it returns 0 without a console.
    unsafe { GetConsoleCP() != 0 }
}

/// Turns on VT processing for standard output and standard error where they
/// are consoles. `false` when a console refused it (a legacy console host);
/// streams that are not consoles (pipes, files) do not count.
pub fn enable_vt_output() -> bool {
    let mut enabled = true;
    for stream in [io::stdout().as_handle(), io::stderr().as_handle()] {
        if let Ok(current) = mode(stream) {
            enabled &= current & VIRTUAL_TERMINAL_PROCESSING != 0
                || set_mode(stream, current | VIRTUAL_TERMINAL_PROCESSING).is_ok();
        }
    }
    enabled
}

/// Opens this process's console input (`CONIN$`), also when stdin is
/// redirected. Fails without a console.
pub fn open_input() -> io::Result<File> {
    File::options().read(true).write(true).open("CONIN$")
}

/// Opens this process's active console screen buffer (`CONOUT$`), also when
/// stdout and stderr are redirected. Fails without a console.
pub fn open_output() -> io::Result<File> {
    File::options().read(true).write(true).open("CONOUT$")
}

/// The console mode of `handle`. Fails for handles that are not a console
/// buffer (files, pipes, MSYS pseudo terminals), so it doubles as a check.
pub fn mode(handle: impl AsHandle) -> io::Result<u32> {
    let mut mode = 0;
    // SAFETY: the handle is borrowed for the call and `mode` is a writable u32.
    if unsafe { GetConsoleMode(handle.as_handle().as_raw_handle(), &raw mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}

/// Sets the console mode of `handle`. Fails for handles that are not a
/// console buffer and for flags the console host does not know.
pub fn set_mode(handle: impl AsHandle, mode: u32) -> io::Result<()> {
    // SAFETY: the handle is borrowed for the call; the mode is passed by value.
    if unsafe { SetConsoleMode(handle.as_handle().as_raw_handle(), mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Raw input for `attach` on a console input handle: no line editing, echo
/// or Ctrl-C processing, and special keys as VT sequences. Restored on drop.
pub fn raw_vt_input<H: AsHandle>(handle: H) -> io::Result<ModeGuard<H>> {
    ModeGuard::change(
        handle,
        VIRTUAL_TERMINAL_INPUT,
        LINE_INPUT | ECHO_INPUT | PROCESSED_INPUT,
    )
}

/// Line input without echo on a console input handle, for secret prompts.
/// Restored on drop.
pub fn hidden_input<H: AsHandle>(handle: H) -> io::Result<ModeGuard<H>> {
    ModeGuard::change(handle, LINE_INPUT | PROCESSED_INPUT, ECHO_INPUT)
}

/// Reads UTF-16 units from a console input handle into `buf`, returning how
/// many were read.
///
/// Ctrl-Z is returned as a character rather than as end of input. A read
/// that Ctrl-C or Ctrl-Break cancels (with processed input) fails with
/// [`io::ErrorKind::Interrupted`]. A surrogate pair may be split across two
/// reads.
pub fn read_console_utf16(handle: impl AsHandle, buf: &mut [u16]) -> io::Result<usize> {
    let capacity = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let mut read = 0;
    // SAFETY: SetLastError only writes this thread's error slot. ReadConsoleW
    // gets a borrowed handle, a writable buffer of at least `capacity` units,
    // a writable count and no input control, which is allowed.
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
    // A Ctrl-C during a cooked read ends it with success, nothing read and
    // this code.
    // SAFETY: reads this thread's last-error value.
    if read == 0 && unsafe { GetLastError() } == ERROR_OPERATION_ABORTED {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "console read cancelled by Ctrl-C",
        ));
    }
    Ok(read as usize)
}

/// Cancels the reads and writes in progress on `handle` from any thread, so
/// a [`read_console_utf16`] waiting for input nobody types returns with an
/// error. Returns whether one was in progress; a read that starts after this
/// call is not affected, so a caller racing the reader repeats it until the
/// reader is done.
pub fn cancel_io(handle: impl AsHandle) -> io::Result<bool> {
    // SAFETY: the handle is borrowed for the call; a null OVERLAPPED cancels
    // every request on it, synchronous ones of other threads included.
    if unsafe { CancelIoEx(handle.as_handle().as_raw_handle(), ptr::null()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND.cast_signed()) {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(true)
}

/// Writes `text` to a console output handle as UTF-16, so it shows correctly
/// whatever the console code page is.
pub fn write_console(handle: impl AsHandle, text: &str) -> io::Result<()> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut rest = units.as_slice();
    while !rest.is_empty() {
        let chunk = u32::try_from(rest.len()).unwrap_or(u32::MAX);
        let mut written = 0;
        // SAFETY: the handle is borrowed for the call; `rest` holds at least
        // `chunk` readable units and `written` is writable; the reserved
        // pointer is null.
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

/// Where the user's keys go, checked before prompting on the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyboard {
    /// A console with a window or a pseudo console (conhost, Windows
    /// Terminal, VS Code, an SSH session) that passes keys on.
    Console,
    /// No console, or one without a window, as `CREATE_NO_WINDOW` gives the
    /// commands agents and editors run: a read would wait forever.
    Unreachable,
    /// A standard stream is a Cygwin or MSYS2 pseudo terminal: mintty (Git
    /// Bash's own window) keeps the keys, and Windows programs started from
    /// it get a hidden console nobody types into.
    Mintty,
}

/// Where the user's keys go.
pub fn keyboard() -> Keyboard {
    let mintty = |handle: BorrowedHandle<'_>| handle.is_terminal() && mode(handle).is_err();
    if mintty(io::stdin().as_handle())
        || mintty(io::stdout().as_handle())
        || mintty(io::stderr().as_handle())
    {
        return Keyboard::Mintty;
    }
    if has_window() {
        Keyboard::Console
    } else {
        Keyboard::Unreachable
    }
}

/// Whether this process's console has a window, its own or a pseudo
/// console's (Windows Terminal, VS Code): someone may type Ctrl-C into it.
/// False without a console and for one created without a window
/// (`CREATE_NO_WINDOW`, or [`AllocMode::NoWindow`]).
pub fn has_window() -> bool {
    // SAFETY: no arguments; returns null without a console or for one created
    // without a window.
    !unsafe { GetConsoleWindow() }.is_null()
}

/// Points standard output at `NUL` and closes the original handle, so the
/// reader of an inherited pipe sees end of file. std looks the standard
/// handle up on each write, so later writes go to `NUL`.
///
/// A parent may pass one handle as several standard streams; every slot
/// holding the original is switched too, or closing it would leave a slot
/// naming a handle value the next open could reuse.
pub fn release_stdout() -> io::Result<()> {
    let null = File::options()
        .read(true)
        .write(true)
        .open("NUL")?
        .into_raw_handle();
    // SAFETY: reads the current standard handle without taking ownership.
    let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    // SAFETY: `null` is an open handle whose ownership moves to the standard
    // handle table.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, null) } == 0 {
        let error = io::Error::last_os_error();
        // SAFETY: the switch failed, so `null` is still ours to close.
        unsafe { CloseHandle(null) };
        return Err(error);
    }
    if original.is_null() || original == INVALID_HANDLE_VALUE {
        return Ok(());
    }
    for which in [STD_INPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: reads a standard handle without taking ownership.
        if unsafe { GetStdHandle(which) } == original
            // SAFETY: `null` stays open in the standard handle table.
            && unsafe { SetStdHandle(which, null) } == 0
        {
            // Closing would leave this slot dangling; keeping the pipe open
            // only delays the reader's end of file until this process exits.
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: no standard slot refers to the original handle any more.
    unsafe { CloseHandle(original) };
    Ok(())
}

/// A console mode change that is undone on drop, so the user's shell gets
/// its console back as it was.
///
/// Ctrl-Break, closing the console window, logoff and shutdown end the
/// process from the console's control handler thread without running
/// destructors, so every live guard is also listed where a control handler
/// restores it before the default handling ends the process.
pub struct ModeGuard<H: AsHandle> {
    handle: H,
    previous: u32,
    id: u64,
}

impl<H: AsHandle> ModeGuard<H> {
    /// Sets the `set` flags and clears the `clear` flags on the console
    /// buffer behind `handle`. Fails, changing nothing, when `handle` is not
    /// a console or the console host rejects the new mode.
    pub fn change(handle: H, set: u32, clear: u32) -> io::Result<Self> {
        install_handler();
        let previous = mode(&handle)?;
        // The lock is held across the change, so the control handler sees
        // either no change or a change with its entry.
        let mut registry = active();
        set_mode(&handle, (previous | set) & !clear)?;
        let id = registry.add(handle.as_handle().as_raw_handle() as usize, previous);
        Ok(Self {
            handle,
            previous,
            id,
        })
    }

    /// The console handle the guard changed.
    pub fn handle(&self) -> &H {
        &self.handle
    }
}

impl<H: AsHandle> Drop for ModeGuard<H> {
    /// Restores the previous mode; there is nothing useful to do if that fails.
    fn drop(&mut self) {
        let mut registry = active();
        let _ = set_mode(&self.handle, self.previous);
        registry.remove(self.id);
    }
}

/// Modes of live [`ModeGuard`]s, for the control handler.
static ACTIVE: Mutex<Registry> = Mutex::new(Registry {
    next: 0,
    entries: Vec::new(),
});

/// Locks [`ACTIVE`]. A panic while it was held cannot leave an entry half
/// written, so a poisoned lock is still used.
fn active() -> MutexGuard<'static, Registry> {
    ACTIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adds the restoring control handler on first use. If that fails, guards
/// still restore on drop.
fn install_handler() {
    static HANDLER: Once = Once::new();
    HANDLER.call_once(|| {
        // SAFETY: `restore_on_exit` is a valid handler for the life of the
        // process and is never removed.
        let _ = unsafe { SetConsoleCtrlHandler(Some(restore_on_exit), 1) };
    });
}

/// Puts back every registered mode when a control event is about to end the
/// process, then returns FALSE so the next handler, or the default one that
/// exits, runs.
unsafe extern "system" fn restore_on_exit(_event: u32) -> BOOL {
    active().restore(|handle, previous| {
        // SAFETY: entries are removed under the lock held here before their
        // guard releases the handle, so `handle` is still open.
        let _ = unsafe { SetConsoleMode(handle as HANDLE, previous) };
    });
    0
}

/// The previous modes of live guards, in the order they were taken.
#[derive(Debug)]
struct Registry {
    next: u64,
    entries: Vec<(u64, usize, u32)>,
}

impl Registry {
    /// Records a guard's handle and previous mode; returns its id.
    fn add(&mut self, handle: usize, previous: u32) -> u64 {
        let id = self.next;
        self.next += 1;
        self.entries.push((id, handle, previous));
        id
    }

    /// Forgets the guard `id` once it restored the mode itself.
    fn remove(&mut self, id: u64) {
        self.entries.retain(|entry| entry.0 != id);
    }

    /// Calls `set` for every entry, newest first, so a buffer changed by
    /// nested guards ends at the mode it had before the oldest one.
    fn restore(&self, mut set: impl FnMut(usize, u32)) {
        for &(_, handle, previous) in self.entries.iter().rev() {
            set(handle, previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nested guards on one handle restore to the oldest previous mode, and
    /// removed guards are not restored again.
    #[test]
    fn restore_ends_at_the_oldest_previous_mode() {
        let mut registry = Registry {
            next: 0,
            entries: Vec::new(),
        };
        let outer = registry.add(1, 0o7);
        let inner = registry.add(1, 0o3);
        let other = registry.add(2, 0o5);
        let replay = |registry: &Registry| {
            let mut modes: Vec<(usize, u32)> = Vec::new();
            registry.restore(|handle, previous| {
                match modes.iter_mut().find(|(known, _)| *known == handle) {
                    Some(slot) => slot.1 = previous,
                    None => modes.push((handle, previous)),
                }
            });
            modes.sort_unstable();
            modes
        };
        assert_eq!(replay(&registry), [(1, 0o7), (2, 0o5)]);
        registry.remove(other);
        registry.remove(inner);
        assert_eq!(replay(&registry), [(1, 0o7)]);
        registry.remove(outer);
        assert!(replay(&registry).is_empty());
    }
}
