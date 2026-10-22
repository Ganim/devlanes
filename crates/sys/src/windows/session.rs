//! Logoff and shutdown notices for a process without a console.
//!
//! Windows tells console processes that the session ends through console control
//! events (`CTRL_LOGOFF_EVENT`, `CTRL_SHUTDOWN_EVENT`), which never reach a
//! detached daemon, and tells GUI processes through `WM_QUERYENDSESSION` and
//! `WM_ENDSESSION`, which it sends to top-level windows only. A message-only
//! window (`HWND_MESSAGE` parent) receives no broadcasts, so [`watch`] creates a
//! hidden top-level window on a thread of its own and runs its message loop.
//! It also works for a process attached to a console: once a process has
//! loaded `user32.dll` (creating the window does), Windows treats it as a GUI
//! program, so its console handlers no longer get `CTRL_LOGOFF_EVENT` or
//! `CTRL_SHUTDOWN_EVENT` and the window gets the session messages instead (see
//! `SetConsoleCtrlHandler`). `CTRL_CLOSE_EVENT` still reaches console handlers.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io;
use std::ptr;
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, ENDSESSION_CLOSEAPP, ENDSESSION_LOGOFF,
    EnumWindows, GetClassNameW, GetMessageW, GetWindowThreadProcessId, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, TranslateMessage,
    WM_CLOSE, WM_DESTROY, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};
use windows_sys::core::BOOL;

use super::security::wide;

/// Why the session ends, from the `WM_ENDSESSION` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The user logs off.
    Logoff,
    /// The machine shuts down or restarts.
    Shutdown,
    /// The Restart Manager closes the program, typically so an installer can
    /// replace its files.
    CloseApp,
}

impl SessionEnd {
    /// Reads the reason from `WM_ENDSESSION`'s `lParam`.
    fn from_flags(flags: LPARAM) -> Self {
        // The flags are a 32-bit mask in the low half of the parameter.
        let flags = flags as u32;
        if flags & ENDSESSION_CLOSEAPP != 0 {
            Self::CloseApp
        } else if flags & ENDSESSION_LOGOFF != 0 {
            Self::Logoff
        } else {
            Self::Shutdown
        }
    }
}

/// What the window procedure calls when the session ends.
type Handler = Box<dyn Fn(SessionEnd)>;

thread_local! {
    /// The handler of the window on this thread; each watcher has its own thread.
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

/// The class every watcher window uses; registered once per process.
const CLASS: &str = "devlanes-session-end";

/// A running watcher. Dropping it closes the window and joins its thread.
#[derive(Debug)]
pub struct SessionWatch {
    /// The window handle as a number, so the watch can move between threads;
    /// only used to post messages to it.
    window: isize,
    thread: Option<JoinHandle<()>>,
}

/// Starts a hidden top-level window that calls `on_end` on its own thread when
/// `WM_ENDSESSION` says the session ends.
///
/// `WM_QUERYENDSESSION` is always allowed: another program may still cancel the
/// shutdown then, so nothing should be stopped yet. Windows ends the process soon
/// after `WM_ENDSESSION` returns, so `on_end` should block until the cleanup it
/// starts is done, bounded by a few seconds.
pub fn watch(on_end: impl Fn(SessionEnd) + Send + 'static) -> io::Result<SessionWatch> {
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("session-end".into())
        .spawn(move || {
            let window = match create_window() {
                Ok(window) => window,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            HANDLER.with(|slot| *slot.borrow_mut() = Some(Box::new(on_end)));
            let _ = sender.send(Ok(window as isize));
            pump();
            HANDLER.with(|slot| slot.borrow_mut().take());
        })?;
    match receiver.recv() {
        Ok(Ok(window)) => Ok(SessionWatch {
            window,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(io::Error::other("the session watcher thread ended early"))
        }
    }
}

impl SessionWatch {
    /// Sends the window the `WM_QUERYENDSESSION` and `WM_ENDSESSION` pair Windows
    /// would send for `end` and returns once both are answered, or fails after
    /// `timeout` per message. For tests of what `on_end` does.
    pub fn simulate(&self, end: SessionEnd, timeout: std::time::Duration) -> io::Result<()> {
        send_end(self.window as HWND, end, timeout)
    }
}

/// [`SessionWatch::simulate`] for the watcher window of another process `pid`,
/// so end-to-end tests can log a running daemon off. `NotFound` when `pid` has
/// no such window.
pub fn simulate_for(pid: u32, end: SessionEnd, timeout: std::time::Duration) -> io::Result<()> {
    let mut search = Search {
        pid,
        class: wide(OsStr::new(CLASS)),
        found: None,
    };
    // SAFETY: the callback only runs during this call, so the pointer to
    // `search` stays valid; a FALSE return only means enumeration stopped.
    unsafe { EnumWindows(Some(visit), (&raw mut search) as LPARAM) };
    match search.found {
        Some(window) => send_end(window as HWND, end, timeout),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} has no session-end window"),
        )),
    }
}

/// What [`simulate_for`]'s enumeration looks for and finds.
struct Search {
    pid: u32,
    /// [`CLASS`] as a NUL-terminated wide string.
    class: Vec<u16>,
    found: Option<isize>,
}

/// Stops at the first window of `search.pid` with the watcher class.
extern "system" fn visit(window: HWND, state: LPARAM) -> BOOL {
    // SAFETY: `state` is the `Search` that `simulate_for` passed, alive and not
    // used elsewhere while the enumeration runs.
    let search = unsafe { &mut *(state as *mut Search) };
    let mut pid = 0;
    // SAFETY: `window` comes from the enumeration; `pid` is writable.
    unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
    if pid != search.pid {
        return 1;
    }
    let mut name = [0_u16; 64];
    // SAFETY: `name` holds the 64 characters the call may write.
    let length = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
    let length = usize::try_from(length).unwrap_or(0);
    // `class` ends with its NUL, which the copied name does not include.
    if name[..length] == search.class[..search.class.len() - 1] {
        search.found = Some(window as isize);
        return 0;
    }
    1
}

/// Sends `window` the `WM_QUERYENDSESSION` and `WM_ENDSESSION` pair for `end`,
/// waiting at most `timeout` for each answer.
fn send_end(window: HWND, end: SessionEnd, timeout: std::time::Duration) -> io::Result<()> {
    let flags = match end {
        SessionEnd::Logoff => ENDSESSION_LOGOFF,
        SessionEnd::Shutdown => 0,
        SessionEnd::CloseApp => ENDSESSION_CLOSEAPP,
    } as LPARAM;
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    for (message, wparam) in [(WM_QUERYENDSESSION, 0), (WM_ENDSESSION, 1)] {
        let mut result = 0;
        // SAFETY: a window that is gone only makes the call fail; it waits at most
        // `millis` for the answer, which `result` receives.
        let ok = unsafe {
            SendMessageTimeoutW(
                window,
                message,
                wparam,
                flags,
                SMTO_ABORTIFHUNG,
                millis,
                &raw mut result,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl Drop for SessionWatch {
    /// Closes the window, which ends its message loop, and waits for the thread.
    fn drop(&mut self) {
        // SAFETY: posting to a window handle that may already be gone only fails.
        let posted = unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) } != 0;
        if posted && let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Registers the class (once) and creates the hidden window on this thread.
fn create_window() -> io::Result<HWND> {
    let class = wide(OsStr::new(CLASS));
    let title = wide(OsStr::new("devlanes daemon"));
    // SAFETY: a null name asks for this executable's module handle.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let definition = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(procedure),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    // SAFETY: `definition` and the class name it points to outlive the call.
    if unsafe { RegisterClassW(&raw const definition) } == 0 {
        // SAFETY: reads this thread's last error right after the failed call.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    // Without WS_VISIBLE the window never shows; a top-level window (no parent)
    // still receives the session-end messages.
    // SAFETY: the class is registered and both strings outlive the call.
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(window)
}

/// Runs this thread's message loop until the window is destroyed.
fn pump() {
    // SAFETY: an all-zero bit pattern is a valid value for this plain C structure.
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is writable; a null window takes this thread's messages.
        let got = unsafe { GetMessageW(&raw mut message, ptr::null_mut(), 0, 0) };
        // 0 is WM_QUIT, -1 an error; either way the loop is over.
        if got == 0 || got == -1 {
            return;
        }
        // SAFETY: `message` was just filled by GetMessageW.
        unsafe {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// The watcher window's procedure.
extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            // FALSE means the shutdown was cancelled after all.
            if wparam != 0 {
                let end = SessionEnd::from_flags(lparam);
                HANDLER.with(|slot| {
                    if let Ok(handler) = slot.try_borrow()
                        && let Some(handler) = handler.as_ref()
                    {
                        handler(end);
                    }
                });
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: default handling for a message of this thread's own window;
        // for WM_CLOSE it destroys the window.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Sends `message` to the watcher's window and returns its answer.
    fn send(watch: &SessionWatch, message: u32, wparam: WPARAM, lparam: LPARAM) -> usize {
        let mut result = 0;
        // SAFETY: the window lives on the watcher's thread; the call waits at most
        // five seconds for the answer, which `result` receives.
        let ok = unsafe {
            SendMessageTimeoutW(
                watch.window as HWND,
                message,
                wparam,
                lparam,
                SMTO_ABORTIFHUNG,
                5_000,
                &raw mut result,
            )
        };
        assert_ne!(ok, 0, "the watcher window did not answer");
        result
    }

    /// The query is allowed and calls nothing; the end calls the handler with its
    /// reason, unless the shutdown was cancelled; dropping ends the thread.
    #[test]
    fn reports_session_end() -> io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        let watch = watch(move |end| {
            let _ = sender.send(end);
        })?;
        assert_eq!(
            send(&watch, WM_QUERYENDSESSION, 0, ENDSESSION_LOGOFF as LPARAM),
            1
        );
        assert!(receiver.try_recv().is_err());
        send(&watch, WM_ENDSESSION, 0, 0);
        assert!(receiver.try_recv().is_err());
        send(&watch, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as LPARAM);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(SessionEnd::Logoff)
        );
        watch.simulate(SessionEnd::Shutdown, Duration::from_secs(5))?;
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::Shutdown));
        watch.simulate(SessionEnd::CloseApp, Duration::from_secs(5))?;
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::CloseApp));
        drop(watch);
        // The handler (and its sender) went with the thread.
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_err());
        Ok(())
    }

    /// Several watchers can run at once; the class is registered once.
    #[test]
    fn watchers_coexist() -> io::Result<()> {
        let first = watch(|_| {})?;
        let second = watch(|_| {})?;
        assert_ne!(first.window, second.window);
        Ok(())
    }

    /// A process's watcher window is found by its PID; a process without one
    /// is `NotFound`.
    #[test]
    fn simulates_for_a_process() -> io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        let _watch = watch(move |end| {
            let _ = sender.send(end);
        })?;
        simulate_for(
            std::process::id(),
            SessionEnd::Logoff,
            Duration::from_secs(5),
        )?;
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(SessionEnd::Logoff)
        );
        let missing = simulate_for(u32::MAX - 1, SessionEnd::Logoff, Duration::from_secs(1));
        assert_eq!(
            missing.map_err(|error| error.kind()),
            Err(io::ErrorKind::NotFound)
        );
        Ok(())
    }
}
