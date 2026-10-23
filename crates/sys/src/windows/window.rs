//! Asking windowed programs to close, the graceful stop for GUI units (Tauri,
//! Electron) that have no console to send Ctrl-C or Ctrl-Break to.

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};
use windows_sys::core::BOOL;

/// Calls `visit` with every top-level window on the caller's desktop and the
/// PID that owns it, until `visit` returns `false`.
pub(crate) fn for_each_window(mut visit: impl FnMut(HWND, u32) -> bool) {
    let mut visit: &mut dyn FnMut(HWND, u32) -> bool = &mut visit;
    // SAFETY: the callback only runs during this call, so the pointer to `visit`
    // stays valid; a FALSE return means only that enumeration ended.
    unsafe { EnumWindows(Some(enumerated), (&raw mut visit) as LPARAM) };
}

/// Passes one enumerated window to the closure [`for_each_window`] was given.
extern "system" fn enumerated(window: HWND, state: LPARAM) -> BOOL {
    // SAFETY: `state` points to the closure `for_each_window` passed, alive and
    // not used elsewhere while the enumeration runs.
    let visit = unsafe { &mut *(state as *mut &mut dyn FnMut(HWND, u32) -> bool) };
    let mut pid = 0;
    // SAFETY: `window` comes from the enumeration; `pid` is writable.
    unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
    BOOL::from(visit(window, pid))
}

/// Whether an enumerated window is visible.
fn visible(window: HWND) -> bool {
    // SAFETY: plain query on an enumerated window.
    unsafe { IsWindowVisible(window) != 0 }
}

/// Posts `WM_CLOSE` to every visible top-level window owned by one of `pids`, as
/// clicking its close button would, and returns how many got one. Only windows in
/// the caller's session and desktop are reached, which is where the daemon and
/// its units run.
pub fn close_windows(pids: &[u32]) -> usize {
    let mut closed = 0;
    if pids.is_empty() {
        return closed;
    }
    for_each_window(|window, pid| {
        if pids.contains(&pid) && visible(window) {
            // SAFETY: posting a message to another process's window is allowed;
            // a window that vanished in between only makes the call fail.
            let posted = unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0;
            closed += usize::from(posted);
        }
        true
    });
    closed
}

/// A visible top-level window and the process that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TopWindow {
    /// The window handle as a number; only compared, never used to reach the window.
    pub handle: isize,
    pub pid: u32,
}

/// Every visible top-level window on the caller's desktop. Tests use it to check
/// that devlanes opens no console windows.
pub fn visible_windows() -> Vec<TopWindow> {
    let mut found = Vec::new();
    for_each_window(|window, pid| {
        if visible(window) {
            found.push(TopWindow {
                handle: window as isize,
                pid,
            });
        }
        true
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is closed without PIDs, and this test process has no window.
    #[test]
    fn closes_only_listed_processes() {
        assert_eq!(close_windows(&[]), 0);
        assert_eq!(close_windows(&[std::process::id()]), 0);
    }

    /// This test process owns no visible window, whatever else the desktop shows.
    #[test]
    fn lists_visible_windows() {
        let own = std::process::id();
        assert!(visible_windows().iter().all(|window| window.pid != own));
    }
}
