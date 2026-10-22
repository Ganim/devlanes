//! Windows only: the daemon runs without a console, so every console program it
//! starts for itself (git, the shell for ready probes) would get a console window
//! of its own unless started with `CREATE_NO_WINDOW`.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use devlanes_e2e::{Error, Lab, Result};
use devlanes_sys::ProcessInfo;
use devlanes_sys::windows::window::visible_windows;

use crate::BUDGET;

/// Git runs for the worktree, `api`'s ready probe runs through the shell (Git's
/// `sh.exe` or `cmd.exe`), `web` runs through it as a `run:` string, and the
/// units themselves run over pipes and under a pseudo console.
const CONFIG: &str = "\
project: fixture-quiet
services:
  api:
    run: [fixture-server, --log-line, api-ready]
    pty: false
    ready: { cmd: 'exit 0' }
  web:
    run: fixture-server --log-line web-ready
    ready: { http: { path: /healthz } }
";

/// How often the desktop is checked. A console window lives as long as its
/// program, tens of milliseconds for a `git` call, so this is well inside it.
const POLL: Duration = Duration::from_millis(5);

/// A process as the baseline remembers it: a PID alone could be reused.
fn key(process: &ProcessInfo) -> (u32, u64) {
    (process.pid, process.start_time)
}

/// `start` makes no visible window appear for a process started during the
/// test, and no console tab in Windows Terminal: when it is the default
/// terminal, a console a helper gets opens there as a new tab (an `OpenConsole`
/// whose parent is `WindowsTerminal`) instead of a window of its own.
///
/// The check watches the whole desktop and only counts windows and tabs of
/// processes that did not exist when the test began, so windows that other
/// programs already had, or move, do not count.
#[test]
fn start_opens_no_console_windows() -> Result<()> {
    let lab = Lab::with_config(CONFIG, BUDGET)?;
    let before: HashSet<(u32, u64)> = devlanes_sys::processes()
        .map_err(|error| Error::Unexpected(error.to_string()))?
        .iter()
        .map(key)
        .collect();
    let windows_before: HashSet<isize> = visible_windows().iter().map(|w| w.handle).collect();
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let mut seen: Vec<String> = Vec::new();
            while !done.load(Ordering::Relaxed) {
                for window in visible_windows() {
                    if windows_before.contains(&window.handle) {
                        continue;
                    }
                    if let Ok(Some(owner)) = devlanes_sys::process(window.pid)
                        && !before.contains(&key(&owner))
                    {
                        seen.push(format!("window of {} (pid {})", owner.name, owner.pid));
                    }
                }
                let table = devlanes_sys::processes().unwrap_or_default();
                for tab in table.iter().filter(|p| {
                    p.name.eq_ignore_ascii_case("OpenConsole.exe") && !before.contains(&key(p))
                }) {
                    let host = table.iter().find(|p| p.pid == tab.ppid);
                    if host.is_some_and(|p| p.name.eq_ignore_ascii_case("WindowsTerminal.exe")) {
                        seen.push(format!(
                            "Windows Terminal tab (OpenConsole pid {})",
                            tab.pid
                        ));
                    }
                }
                thread::sleep(POLL);
            }
            seen.sort();
            seen.dedup();
            seen
        })
    };
    let started = lab.json(lab.repo(), &["start"]);
    // Helpers still running when start answers (a slow probe) get a moment.
    thread::sleep(Duration::from_millis(500));
    done.store(true, Ordering::Relaxed);
    let seen = watcher
        .join()
        .map_err(|_| Error::Unexpected("the window watcher panicked".into()))?;
    let started = started?;
    assert_eq!(started["ready"], true, "{started}");
    assert!(seen.is_empty(), "start opened console windows: {seen:?}");
    Ok(())
}
