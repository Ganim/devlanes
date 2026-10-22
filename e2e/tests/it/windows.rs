//! Windows only: the daemon runs without a console, so every console program it
//! starts for itself (git, the shell for ready probes) would get a console window
//! of its own unless started with `CREATE_NO_WINDOW`.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use devlanes_e2e::{Error, Lab, Result};
use devlanes_sys::windows::handles::held_processes;
use devlanes_sys::windows::window::visible_windows;
use devlanes_sys::{DescendantTracker, ProcessInfo};

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

/// How often the desktop's windows are listed. A console window lives as long
/// as its program, tens of milliseconds for a `git` call, so this is well
/// inside it.
const POLL: Duration = Duration::from_millis(5);

/// How often the process table is read, to follow the tree and find new
/// `OpenConsole` hosts. A window's owner is looked up on its own, so this only
/// needs to keep up with the processes that start others.
const SCAN: Duration = Duration::from_millis(50);

/// Most parents a window's owner is followed through to reach the tree.
const MAX_DEPTH: usize = 16;

/// A process as the watcher remembers it: a PID alone could be reused.
fn key(process: &ProcessInfo) -> (u32, u64) {
    (process.pid, process.start_time)
}

/// Whether `pid`, or one of its living ancestors, is a member of the tree.
/// An ancestor started after its child is a reused PID, which ends the walk.
fn in_tree(pid: u32, members: &HashSet<(u32, u64)>) -> Option<ProcessInfo> {
    let owner = devlanes_sys::process(pid).ok()??;
    let mut current = owner.clone();
    for _ in 0..MAX_DEPTH {
        if members.contains(&key(&current)) {
            return Some(owner);
        }
        let parent = devlanes_sys::process(current.ppid).ok()??;
        if parent.start_time > current.start_time {
            return None;
        }
        current = parent;
    }
    None
}

/// `start` makes no visible window appear for the processes it starts, and
/// no console tab in Windows Terminal: when it is the default terminal, a
/// console a helper gets opens there as a new tab (an `OpenConsole`) instead
/// of a window of its own.
///
/// Only this test's own process tree counts: the CLI it runs, the daemon the
/// CLI spawns and everything the daemon starts. The tree is rooted at the test
/// process because the daemon only exists once `start` has spawned it. Windows
/// count when their owner (a console window's owner is its client) or one of
/// its ancestors is in the tree; an `OpenConsole` counts when it holds a
/// handle to a process of the tree, which a console host does for each client
/// it serves. Other programs' windows and tabs, opened during the test or
/// before, do not count.
#[test]
fn start_opens_no_console_windows() -> Result<()> {
    let lab = Lab::with_config(CONFIG, BUDGET)?;
    let root = devlanes_sys::identity(std::process::id())
        .map_err(|error| Error::Unexpected(error.to_string()))?
        .ok_or_else(|| Error::Unexpected("no identity for the test process".into()))?;
    let hosts_before: HashSet<(u32, u64)> = devlanes_sys::processes()
        .map_err(|error| Error::Unexpected(error.to_string()))?
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case("OpenConsole.exe"))
        .map(key)
        .collect();
    let windows_before: HashSet<isize> = visible_windows().iter().map(|w| w.handle).collect();
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let mut seen: Vec<String> = Vec::new();
            let mut tree = DescendantTracker::new(root);
            // Every process the tree ever had, exited ones included: a window
            // or a host may outlive the helper it belonged to.
            let mut members: HashSet<(u32, u64)> = HashSet::from([(root.pid, root.start_time)]);
            let mut scanned: Option<Instant> = None;
            while !done.load(Ordering::Relaxed) {
                if scanned.is_none_or(|at| at.elapsed() >= SCAN) {
                    scanned = Some(Instant::now());
                    let table = devlanes_sys::processes().unwrap_or_default();
                    tree.update(&table);
                    members.extend(tree.processes().map(key));
                    for host in table.iter().filter(|p| {
                        p.name.eq_ignore_ascii_case("OpenConsole.exe")
                            && !hosts_before.contains(&key(p))
                    }) {
                        let serves_tree = held_processes(host.pid)
                            .ok()
                            .flatten()
                            .unwrap_or_default()
                            .iter()
                            .any(|held| members.contains(&(held.pid, held.start_time)));
                        if serves_tree {
                            seen.push(format!(
                                "Windows Terminal tab (OpenConsole pid {})",
                                host.pid
                            ));
                        }
                    }
                }
                for window in visible_windows() {
                    if windows_before.contains(&window.handle) {
                        continue;
                    }
                    if let Some(owner) = in_tree(window.pid, &members) {
                        seen.push(format!("window of {} (pid {})", owner.name, owner.pid));
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
