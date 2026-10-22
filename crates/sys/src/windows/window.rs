//! Asking windowed programs to close, the graceful stop for GUI units (Tauri,
//! Electron) that have no console to send Ctrl-C or Ctrl-Break to.

use std::collections::BTreeSet;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};
use windows_sys::core::BOOL;

/// What the enumeration callback reads and counts.
struct Search {
    pids: BTreeSet<u32>,
    closed: usize,
}

/// Posts `WM_CLOSE` to every visible top-level window owned by one of `pids`, as
/// clicking its close button would, and returns how many got one. Only windows in
/// the caller's session and desktop are reached, which is where the daemon and
/// its units run.
pub fn close_windows(pids: &[u32]) -> usize {
    let mut search = Search {
        pids: pids.iter().copied().collect(),
        closed: 0,
    };
    if search.pids.is_empty() {
        return 0;
    }
    // SAFETY: the callback only runs during this call, so the pointer to `search`
    // stays valid; a FALSE return means only that enumeration ended.
    unsafe { EnumWindows(Some(visit), (&raw mut search) as LPARAM) };
    search.closed
}

/// Posts `WM_CLOSE` to `window` when it is visible and owned by a listed process.
extern "system" fn visit(window: HWND, state: LPARAM) -> BOOL {
    // SAFETY: `state` is the `Search` that `close_windows` passed, alive and not
    // used elsewhere while the enumeration runs.
    let search = unsafe { &mut *(state as *mut Search) };
    let mut pid = 0;
    // SAFETY: `window` comes from the enumeration; `pid` is writable.
    unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
    // SAFETY: plain query on an enumerated window.
    if search.pids.contains(&pid) && unsafe { IsWindowVisible(window) } != 0 {
        // SAFETY: posting a message to another process's window is allowed;
        // a window that vanished in between only makes the call fail.
        if unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0 {
            search.closed += 1;
        }
    }
    1
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
    let mut found: Vec<TopWindow> = Vec::new();
    // SAFETY: the callback only runs during this call, so the pointer to `found`
    // stays valid; a FALSE return means only that enumeration ended.
    unsafe { EnumWindows(Some(collect), (&raw mut found) as LPARAM) };
    found
}

/// Adds `window` to the list when it is visible.
extern "system" fn collect(window: HWND, state: LPARAM) -> BOOL {
    // SAFETY: `state` is the vector `visible_windows` passed, alive and not used
    // elsewhere while the enumeration runs.
    let found = unsafe { &mut *(state as *mut Vec<TopWindow>) };
    // SAFETY: plain query on an enumerated window.
    if unsafe { IsWindowVisible(window) } != 0 {
        let mut pid = 0;
        // SAFETY: `window` comes from the enumeration; `pid` is writable.
        unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
        found.push(TopWindow {
            handle: window as isize,
            pid,
        });
    }
    1
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
