//! Console modes and console text I/O for the CLI: raw key input for `attach`,
//! escape-sequence output, and prompts that hide what the user types, with
//! whether the user's keys reach the console at all; and for the TUI, whether
//! a classic console window shows the console.
//!
//! Mode changes apply to the console buffer behind a handle, not to the handle,
//! so they are visible to every process sharing the console until restored;
//! [`ModeGuard`] puts the previous mode back, also when a console control event
//! ends the process without running destructors.

use std::fs::File;
use std::io::{self, IsTerminal};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::ptr;
use std::sync::{Mutex, MutexGuard, Once, PoisonError};

use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, GetLastError, HANDLE, SetLastError};
use windows_sys::Win32::System::Console::{
    self as win, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT, GetConsoleMode, GetConsoleWindow, ReadConsoleW, SetConsoleCtrlHandler,
    SetConsoleMode, WriteConsoleW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, IsWindowVisible};
use windows_sys::core::BOOL;

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
/// buffer, and for output flags the console host does not know (VT processing in
/// the legacy console).
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

/// Whether this process's console shows in a classic console window (conhost)
/// rather than in a terminal behind a pseudoconsole (Windows Terminal, also
/// when it took over a console launched elsewhere, VS Code, WezTerm, Alacritty,
/// an SSH session) or a hidden console another program draws (ConEmu,
/// winpty). False without a console.
pub fn classic_window() -> bool {
    // SAFETY: no arguments; returns null without a console.
    let window = unsafe { GetConsoleWindow() };
    if window.is_null() {
        return false;
    }
    let mut class = [0u16; 64];
    // SAFETY: `window` is a window handle and `class` holds the number of
    // writable units passed; a stale handle makes the call fail, not crash.
    let len = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    // SAFETY: as above, a plain query on the handle.
    let visible = unsafe { IsWindowVisible(window) } != 0;
    let class = class
        .get(..usize::try_from(len).unwrap_or(0))
        .unwrap_or_default();
    is_classic(class, visible)
}

/// Whether a console window of `class` is conhost's own window on screen.
/// Pseudoconsoles use `PseudoConsoleWindow`, which is never shown.
fn is_classic(class: &[u16], visible: bool) -> bool {
    visible
        && class
            .iter()
            .copied()
            .eq("ConsoleWindowClass".encode_utf16())
}

/// Whether what the user types reaches this process's console, so a prompt
/// read from `CONIN$` can be answered. See [`keyboard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyboard {
    /// The console has a window or a pseudoconsole (conhost, Windows Terminal,
    /// VS Code, an SSH session, winpty) that passes keys on.
    Console,
    /// No console, or one without a window, as `CREATE_NO_WINDOW` gives the
    /// commands agents and editors run: a read would wait forever.
    Unreachable,
    /// A standard stream is a Cygwin or MSYS2 pseudo terminal: mintty, Git
    /// Bash's own window, keeps the keys, and Windows programs started from it
    /// get a hidden console nobody can type into.
    Mintty,
}

/// Where the user's keys go, checked before prompting on the console.
pub fn keyboard() -> Keyboard {
    if mintty_stream(io::stdin().as_handle())
        || mintty_stream(io::stdout().as_handle())
        || mintty_stream(io::stderr().as_handle())
    {
        return Keyboard::Mintty;
    }
    // SAFETY: no arguments; returns null without a console or for a console
    // created without a window.
    if unsafe { GetConsoleWindow() }.is_null() {
        return Keyboard::Unreachable;
    }
    Keyboard::Console
}

/// Whether `handle` is a Cygwin or MSYS2 pseudo terminal: std's terminal check
/// accepts those pipes by name, and only they are terminals without a console mode.
fn mintty_stream(handle: BorrowedHandle<'_>) -> bool {
    handle.is_terminal() && mode(handle).is_err()
}

/// A console mode change that is undone on drop, also on error paths, so the
/// user's shell gets its console back as it was.
///
/// Ctrl-Break, closing the console window, logoff and shutdown end the process
/// from the console's control handler thread without running destructors, so
/// every live guard is also listed in a registry that a control handler restores
/// before the default handling ends the process. The handler also covers Ctrl-C,
/// which ends a process without a handler of its own while a prompt has echo off.
pub struct ModeGuard<H: AsHandle> {
    handle: H,
    previous: u32,
    id: u64,
}

impl<H: AsHandle> ModeGuard<H> {
    /// Sets the `set` flags and clears the `clear` flags on the console buffer
    /// behind `handle`. Fails, changing nothing, when `handle` is not a console or
    /// the console host rejects the new mode.
    pub fn change(handle: H, set: u32, clear: u32) -> io::Result<Self> {
        install_handler();
        let previous = mode(&handle)?;
        // The lock is held across the change, so the control handler sees either
        // no change or a change with its registry entry.
        let mut registry = active();
        set_mode(&handle, (previous | set) & !clear)?;
        let id = registry.add(handle.as_handle().as_raw_handle() as usize, previous);
        Ok(Self {
            handle,
            previous,
            id,
        })
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
static ACTIVE: Mutex<Registry> = Mutex::new(Registry::new());

/// Installs [`restore_on_exit`] once per process.
static HANDLER: Once = Once::new();

/// Locks [`ACTIVE`]. A panic while it was held cannot leave an entry half
/// written, so a poisoned lock is still used.
fn active() -> MutexGuard<'static, Registry> {
    ACTIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adds the control handler on first use. If that fails, guards still restore
/// on drop; only the control events are left to the default handling.
fn install_handler() {
    HANDLER.call_once(|| {
        // SAFETY: `restore_on_exit` is a valid handler for the life of the
        // process and is never removed.
        let _ = unsafe { SetConsoleCtrlHandler(Some(restore_on_exit), 1) };
    });
}

/// Puts back every registered mode when `event` is about to end the process,
/// then returns FALSE so the next handler, or the default one that exits, runs.
/// When another handler keeps the process alive, the guards restore again on drop.
extern "system" fn restore_on_exit(event: u32) -> BOOL {
    if ends_process(event) {
        active().restore(|handle, previous| {
            // SAFETY: entries are removed under the lock held here before their
            // guard releases the handle, so `handle` is still open; the mode is
            // passed by value.
            let _ = unsafe { SetConsoleMode(handle as HANDLE, previous) };
        });
    }
    0
}

/// Whether the default handling of console control `event` ends the process.
fn ends_process(event: u32) -> bool {
    matches!(
        event,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    )
}

/// The previous modes of live guards, in the order they were taken.
#[derive(Debug)]
struct Registry {
    next: u64,
    entries: Vec<Entry>,
}

/// One live guard: the console handle as an integer and the mode to restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    id: u64,
    handle: usize,
    previous: u32,
}

impl Registry {
    /// An empty registry, usable in a static.
    const fn new() -> Self {
        Self {
            next: 0,
            entries: Vec::new(),
        }
    }

    /// Records a guard and returns its id for [`Self::remove`].
    fn add(&mut self, handle: usize, previous: u32) -> u64 {
        let id = self.next;
        self.next += 1;
        self.entries.push(Entry {
            id,
            handle,
            previous,
        });
        id
    }

    /// Forgets the guard `id`, when it has restored the mode itself.
    fn remove(&mut self, id: u64) {
        self.entries.retain(|entry| entry.id != id);
    }

    /// Calls `set` for every entry, newest first, so a buffer changed by nested
    /// guards ends at the mode it had before the oldest one.
    fn restore(&self, mut set: impl FnMut(usize, u32)) {
        for entry in self.entries.iter().rev() {
            set(entry.handle, entry.previous);
        }
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
        let raw = file.as_raw_handle() as usize;
        assert!(active().entries.iter().all(|entry| entry.handle != raw));
    }

    /// Replays `registry` onto a fake console: handle -> current mode.
    fn replay(registry: &Registry) -> Vec<(usize, u32)> {
        let mut modes: Vec<(usize, u32)> = Vec::new();
        registry.restore(|handle, previous| {
            match modes.iter_mut().find(|(known, _)| *known == handle) {
                Some(slot) => slot.1 = previous,
                None => modes.push((handle, previous)),
            }
        });
        modes.sort_unstable();
        modes
    }

    #[test]
    fn restore_ends_at_the_oldest_previous_mode_per_handle() {
        let mut registry = Registry::new();
        let outer = registry.add(1, 0o7);
        let inner = registry.add(1, 0o3);
        let other = registry.add(2, 0o5);
        assert_eq!(replay(&registry), [(1, 0o7), (2, 0o5)]);
        // A guard that restored itself is not restored again.
        registry.remove(other);
        registry.remove(inner);
        assert_eq!(replay(&registry), [(1, 0o7)]);
        registry.remove(outer);
        assert!(replay(&registry).is_empty());
        // Ids are not reused, so a stale id removes nothing.
        let next = registry.add(3, 1);
        assert_ne!(next, outer);
        registry.remove(outer);
        assert_eq!(replay(&registry), [(3, 1)]);
    }

    #[test]
    fn only_a_visible_conhost_window_is_classic() {
        let class = |name: &str| name.encode_utf16().collect::<Vec<_>>();
        assert!(is_classic(&class("ConsoleWindowClass"), true));
        // ConEmu and winpty keep the real console hidden.
        assert!(!is_classic(&class("ConsoleWindowClass"), false));
        assert!(!is_classic(&class("PseudoConsoleWindow"), true));
        assert!(!is_classic(&class("PseudoConsoleWindow"), false));
        assert!(!is_classic(&[], true));
        // Whatever console nextest runs under, the query must not fail.
        let _ = classic_window();
    }

    /// Only pipes named like a Cygwin or MSYS2 pseudo terminal count as mintty.
    #[test]
    fn mintty_streams_are_recognised_by_their_pipe_name() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("runtime");
        let _context = runtime.enter();
        let unique = format!("{:016x}", std::process::id());
        let is_mintty = |name: String| {
            let _server = tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .expect("pipe server");
            let client = File::options()
                .read(true)
                .write(true)
                .open(&name)
                .expect("pipe client");
            mintty_stream(client.as_handle())
        };
        assert!(is_mintty(format!(
            r"\\.\pipe\msys-{unique}-pty0-from-master"
        )));
        assert!(is_mintty(format!(
            r"\\.\pipe\cygwin-{unique}-pty1-to-master"
        )));
        assert!(!is_mintty(format!(
            r"\\.\pipe\acme-{unique}-pty0-from-master"
        )));
        assert!(!is_mintty(format!(r"\\.\pipe\msys-{unique}-pipe")));
        let file = tempfile::tempfile().expect("temp file");
        assert!(!mintty_stream(file.as_handle()));
        // Whatever runs the tests, the check must not fail.
        let _ = keyboard();
    }

    #[test]
    fn only_events_that_end_the_process_restore() {
        for event in [
            CTRL_C_EVENT,
            CTRL_BREAK_EVENT,
            CTRL_CLOSE_EVENT,
            CTRL_LOGOFF_EVENT,
            CTRL_SHUTDOWN_EVENT,
        ] {
            assert!(ends_process(event), "{event}");
        }
        assert!(!ends_process(7));
    }
}
