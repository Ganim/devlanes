//! Top-level windows: asking windowed programs to close, and logoff and
//! shutdown notices for a process without a console.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ENDSESSION_CLOSEAPP,
    ENDSESSION_LOGOFF, EnumWindows, GetMessageW, GetWindowThreadProcessId, IsWindowVisible, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};
use windows_sys::core::BOOL;

use crate::proc::Held;
use crate::util::{wide, win32_error};

/// Posts `WM_CLOSE` to every visible top-level window of the processes in
/// `targets` (`(pid, start_time)`), as clicking its close button would, and
/// returns how many got one. Each target is opened and verified first and
/// held during the walk, so a window of a process that reused a PID is never
/// reached. Only windows on the caller's desktop are seen.
pub fn close_windows(targets: &[(u32, u64)]) -> io::Result<usize> {
    let mut held = Vec::new();
    for &(pid, start_time) in targets {
        if let Some(process) = Held::open(pid, start_time)? {
            held.push(process);
        }
    }
    let mut closed = 0;
    if held.is_empty() {
        return Ok(closed);
    }
    for_each_window(|window, pid| {
        // SAFETY: plain query on an enumerated window.
        let visible = unsafe { IsWindowVisible(window) } != 0;
        if visible && held.iter().any(|process| process.pid() == pid) {
            // SAFETY: posting to another process's window is allowed; a window
            // that vanished in between only makes the call fail.
            let posted = unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0;
            closed += usize::from(posted);
        }
        true
    });
    Ok(closed)
}

/// A visible top-level window and the process that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VisibleWindow {
    /// The window handle as a number; only good for telling windows apart.
    pub handle: isize,
    /// The owning process. The console host reports a console window as owned
    /// by one of the console's client processes.
    pub pid: u32,
}

/// Lists the visible top-level windows on the caller's desktop. A caller
/// without a desktop (a service session) gets none.
pub fn visible_windows() -> Vec<VisibleWindow> {
    let mut windows = Vec::new();
    for_each_window(|window, pid| {
        // SAFETY: plain query on an enumerated window.
        if unsafe { IsWindowVisible(window) } != 0 {
            windows.push(VisibleWindow {
                handle: window as isize,
                pid,
            });
        }
        true
    });
    windows
}

/// Calls `visit` with every top-level window on the caller's desktop and the
/// PID that owns it, until `visit` returns `false`.
fn for_each_window(mut visit: impl FnMut(HWND, u32) -> bool) {
    let mut visit: &mut dyn FnMut(HWND, u32) -> bool = &mut visit;
    // SAFETY: the callback only runs during this call, so the pointer to
    // `visit` stays valid; a FALSE result only means the walk was stopped.
    unsafe { EnumWindows(Some(enumerated), (&raw mut visit) as LPARAM) };
}

/// Passes one enumerated window to the closure [`for_each_window`] was given.
unsafe extern "system" fn enumerated(window: HWND, state: LPARAM) -> BOOL {
    // SAFETY: `state` points to the closure `for_each_window` passed, alive
    // and not used elsewhere while the walk runs.
    let visit = unsafe { &mut *(state as *mut &mut dyn FnMut(HWND, u32) -> bool) };
    let mut pid = 0;
    // SAFETY: `window` comes from the walk; `pid` is writable.
    unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
    BOOL::from(visit(window, pid))
}

/// Why the watch asks the process to stop: the `WM_ENDSESSION` flags, or a
/// `WM_CLOSE` from another program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The user logs off.
    Logoff,
    /// The machine shuts down or restarts.
    Shutdown,
    /// The Restart Manager closes the program, typically for an installer.
    CloseApp,
    /// Another program sent the hidden window `WM_CLOSE`, as `taskkill`
    /// without `/f` does: a polite request to exit, like `SIGTERM`. The
    /// session goes on, so nothing waits for [`Done`].
    Close,
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

/// Handed to the session-end callback: Windows gets its answer to
/// `WM_ENDSESSION`, after which it may end the process, once this is
/// finished or dropped, or once the watch's timeout passes.
#[derive(Debug)]
pub struct Done(mpsc::Sender<()>);

impl Done {
    /// Releases the `WM_ENDSESSION` answer; dropping does the same.
    pub fn finish(self) {
        let _ = self.0.send(());
    }
}

/// The callback and the cap on how long its answer is held.
struct Watcher {
    on_end: Box<dyn Fn(SessionEnd, Done)>,
    timeout: Duration,
    /// Set by [`SessionWatch`]'s drop just before it posts [`STOP`].
    stopping: Arc<AtomicBool>,
}

thread_local! {
    /// The watcher of the window on this thread; each watch has its own thread.
    static WATCHER: RefCell<Option<Watcher>> = const { RefCell::new(None) };
}

/// The class every watch window uses; registered once per process.
const CLASS: &str = "devlanes-session-end";

/// The private message that ends a watch once [`Watcher::stopping`] is set.
/// `WM_CLOSE` cannot do it, since other programs send that one to ask the
/// process to exit, and the flag keeps a stray `STOP` from another program
/// from ending the watch either.
const STOP: u32 = WM_APP + 1;

/// A running session-end watch. Dropping it closes the window and joins its
/// thread.
#[derive(Debug)]
pub struct SessionWatch {
    /// The window handle as a number, so the watch can move between threads;
    /// only used to post messages to it.
    window: isize,
    thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

/// Starts a hidden top-level window on its own thread that calls `on_end`
/// there when `WM_ENDSESSION` says the session ends, then holds its answer
/// until `on_end`'s [`Done`] is finished or dropped, at most `timeout`.
/// Windows ends the process soon after the answer. A `WM_CLOSE` from another
/// program calls `on_end` with [`SessionEnd::Close`] and leaves the window
/// in place.
///
/// Windows tells GUI processes, and every process that loaded `user32.dll`
/// (creating this window does), through these messages to top-level windows
/// rather than through console control events, so a message-only window
/// would not do. `WM_QUERYENDSESSION` is always allowed: another program may
/// still cancel the shutdown then, so nothing should stop yet.
pub fn watch_session_end(
    timeout: Duration,
    on_end: impl Fn(SessionEnd, Done) + Send + 'static,
) -> io::Result<SessionWatch> {
    let (sender, receiver) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stopping);
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
            WATCHER.with(|slot| {
                *slot.borrow_mut() = Some(Watcher {
                    on_end: Box::new(on_end),
                    timeout,
                    stopping: flag,
                });
            });
            let _ = sender.send(Ok(window as isize));
            pump();
            WATCHER.with(|slot| slot.borrow_mut().take());
        })?;
    match receiver.recv() {
        Ok(Ok(window)) => Ok(SessionWatch {
            window,
            thread: Some(thread),
            stopping,
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(io::Error::other("the session watch thread ended early"))
        }
    }
}

impl Drop for SessionWatch {
    /// Destroys the window, which ends its message loop, and waits for the thread.
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // SAFETY: posting to a window handle that may already be gone only fails.
        let posted = unsafe { PostMessageW(self.window as HWND, STOP, 0, 0) } != 0;
        if posted && let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Registers the class (once) and creates the hidden window on this thread.
fn create_window() -> io::Result<HWND> {
    let class = wide(OsStr::new(CLASS))?;
    let title = wide(OsStr::new("devlanes"))?;
    // SAFETY: a null name asks for this executable's module handle.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let definition = WNDCLASSW {
        lpfnWndProc: Some(procedure),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: `definition` and the class name it points to outlive the call.
    if unsafe { RegisterClassW(&raw const definition) } == 0 {
        // SAFETY: reads this thread's last error right after the failed call.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(win32_error(error));
        }
    }
    // Without WS_VISIBLE the window never shows; a top-level window (no
    // parent) still receives the session-end messages.
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
    let mut message = MSG::default();
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

/// The watch window's procedure.
unsafe extern "system" fn procedure(
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
                WATCHER.with(|slot| {
                    if let Ok(watcher) = slot.try_borrow()
                        && let Some(watcher) = watcher.as_ref()
                    {
                        let (sender, receiver) = mpsc::channel();
                        (watcher.on_end)(end, Done(sender));
                        // Finished, dropped or timed out: answer either way.
                        let _ = receiver.recv_timeout(watcher.timeout);
                    }
                });
            }
            0
        }
        // The default handling would destroy the window, and logoff would
        // then end the process without a shutdown.
        WM_CLOSE => {
            WATCHER.with(|slot| {
                if let Ok(watcher) = slot.try_borrow()
                    && let Some(watcher) = watcher.as_ref()
                {
                    let (sender, _) = mpsc::channel();
                    (watcher.on_end)(SessionEnd::Close, Done(sender));
                }
            });
            0
        }
        STOP => {
            let stopping = WATCHER.with(|slot| {
                slot.try_borrow().is_ok_and(|watcher| {
                    watcher
                        .as_ref()
                        .is_some_and(|watcher| watcher.stopping.load(Ordering::SeqCst))
                })
            });
            if stopping {
                // SAFETY: the window belongs to this thread; destroying it
                // sends WM_DESTROY, which ends the loop.
                unsafe { DestroyWindow(window) };
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: default handling for a message of this thread's own window.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW};

    use super::*;

    /// Sends `message` to the watch's window and returns its answer, waiting
    /// at most 30 s.
    fn send(watch: &SessionWatch, message: u32, wparam: WPARAM, lparam: LPARAM) -> usize {
        let mut result = 0;
        // SAFETY: the window lives on the watch's thread; the call waits a
        // bounded time for the answer, which `result` receives.
        let ok = unsafe {
            SendMessageTimeoutW(
                watch.window as HWND,
                message,
                wparam,
                lparam,
                SMTO_ABORTIFHUNG,
                30_000,
                &raw mut result,
            )
        };
        assert_ne!(ok, 0, "the watch window did not answer");
        result
    }

    /// The query is allowed and calls nothing; a cancelled end calls nothing;
    /// a real end calls the callback and is answered only after it finished.
    #[test]
    fn holds_the_answer_until_done() {
        let finished = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let flag = Arc::clone(&finished);
        let watch = watch_session_end(Duration::from_secs(30), move |end, done| {
            let _ = sender.send(end);
            let flag = Arc::clone(&flag);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                flag.store(true, Ordering::SeqCst);
                done.finish();
            });
        })
        .expect("watch");
        assert_eq!(
            send(&watch, WM_QUERYENDSESSION, 0, ENDSESSION_LOGOFF as LPARAM),
            1
        );
        send(&watch, WM_ENDSESSION, 0, 0);
        assert!(receiver.try_recv().is_err());
        send(&watch, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as LPARAM);
        assert!(finished.load(Ordering::SeqCst), "answered before done");
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::Logoff));
        drop(watch);
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_err());
    }

    /// The hidden watch window is never listed as visible.
    #[test]
    fn hidden_windows_are_not_listed() {
        let watch = watch_session_end(Duration::from_secs(1), |_, _| {}).expect("watch");
        assert!(
            visible_windows()
                .iter()
                .all(|window| window.handle != watch.window)
        );
    }

    /// `WM_CLOSE` and a `STOP` from someone else are requests that leave the
    /// window running: the first calls the callback, the second nothing.
    #[test]
    fn close_requests_keep_the_window() {
        let (sender, receiver) = mpsc::channel();
        let watch = watch_session_end(Duration::from_secs(30), move |end, _done| {
            let _ = sender.send(end);
        })
        .expect("watch");
        send(&watch, WM_CLOSE, 0, 0);
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::Close));
        send(&watch, STOP, 0, 0);
        send(&watch, WM_ENDSESSION, 1, ENDSESSION_LOGOFF as LPARAM);
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::Logoff));
        drop(watch);
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_err());
    }

    /// A callback that drops its `Done` releases the answer, and the reason
    /// follows the flags.
    #[test]
    fn dropping_done_answers() {
        let (sender, receiver) = mpsc::channel();
        let watch = watch_session_end(Duration::from_secs(30), move |end, _done| {
            let _ = sender.send(end);
        })
        .expect("watch");
        send(&watch, WM_ENDSESSION, 1, 0);
        send(&watch, WM_ENDSESSION, 1, ENDSESSION_CLOSEAPP as LPARAM);
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::Shutdown));
        assert_eq!(receiver.try_recv(), Ok(SessionEnd::CloseApp));
    }
}
