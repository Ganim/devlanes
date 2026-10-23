//! Windows only: no console windows from `start`, pipe units that outlive a
//! killed daemon, and batch files with arguments cmd.exe cannot take safely.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use devlanes_e2e::{
    Error, Lab, Result, alive, fixture_yaml, http_get, kill, lane, pid, port, unit,
};
use devlanes_sys::{DescendantTracker, ProcessInfo};
use devlanes_sys_windows::window::visible_windows;

use crate::{BUDGET, need, units_in};

/// Git runs for the worktree, `api`'s ready probe and `web` run through Git's
/// `sh.exe`, and both units run in pseudo consoles.
const QUIET: &str = "\
project: fixture-quiet
services:
  api:
    run: [fixture-server, --log-line, api-ready]
    ready: { cmd: 'exit 0' }
  web:
    run: fixture-server --log-line web-ready
    ready: { http: { path: /healthz } }
";

/// How often the desktop's windows are listed. A console window lives as long as
/// its program, tens of milliseconds for a `git` call, so this is well inside it.
const POLL: Duration = Duration::from_millis(5);

/// How often the process table is read to follow the tree. A window's owner is
/// looked up on its own, so this only needs to keep up with processes that start
/// others.
const SCAN: Duration = Duration::from_millis(50);

/// Most parents a window's owner is followed through to reach the tree.
const MAX_DEPTH: usize = 16;

/// A process as the watcher remembers it: a PID alone could be reused.
fn key(process: &ProcessInfo) -> (u32, u64) {
    (process.pid, process.start_time)
}

/// The owner of a window when it, or one of its living ancestors, is a member
/// of the tree. An ancestor started after its child is a reused PID, which ends
/// the walk.
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

/// `start` makes no visible window appear for any process it starts.
///
/// Only this test's process tree counts: the CLI it runs, the daemon the CLI
/// spawns and everything the daemon starts (Git, the shell, probes, units). The
/// tree is rooted at the test process because the daemon only exists once `start`
/// has spawned it, and it is followed while `start` runs, so helpers that exit
/// within milliseconds are still seen. Windows that existed before, and windows of
/// other programs, do not count. A console window shown by Windows Terminal (as
/// the default terminal) belongs to the terminal, outside the tree; GitHub's
/// runners and a default install use the console host, whose windows count.
#[test]
fn start_opens_no_console_windows() -> Result<()> {
    let lab = Lab::with_config(QUIET, BUDGET)?;
    let root = devlanes_sys::identity(std::process::id())
        .map_err(|error| Error::Unexpected(error.to_string()))?
        .ok_or_else(|| Error::Unexpected("no identity for the test process".into()))?;
    let before: HashSet<isize> = visible_windows().iter().map(|w| w.handle).collect();
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let mut seen: Vec<String> = Vec::new();
            let mut tree = DescendantTracker::new(root);
            // Every process the tree ever had, exited ones included: a window may
            // outlive the helper it belonged to.
            let mut members: HashSet<(u32, u64)> = HashSet::from([(root.pid, root.start_time)]);
            let mut scanned: Option<Instant> = None;
            while !done.load(Ordering::Relaxed) {
                if scanned.is_none_or(|at| at.elapsed() >= SCAN) {
                    scanned = Some(Instant::now());
                    tree.update(&devlanes_sys::processes().unwrap_or_default());
                    members.extend(tree.processes().map(key));
                }
                for window in visible_windows() {
                    if before.contains(&window.handle) {
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
    // Helpers still running when start answers get a moment.
    thread::sleep(Duration::from_millis(500));
    done.store(true, Ordering::Relaxed);
    let seen = watcher
        .join()
        .map_err(|_| Error::Unexpected("the window watcher panicked".into()))?;
    let started = started?;
    assert_eq!(started["ready"], true, "{started}");
    assert!(seen.is_empty(), "start opened windows: {seen:?}");
    Ok(())
}

/// After the daemon is killed, every pipe unit survives as an `orphaned` unit
/// of the next daemon, with its PID and identity checked; `start` refuses it,
/// `stop` terminates it, and `start` then brings the lane back.
#[test]
fn daemon_crash_leaves_pipe_units_as_orphans() -> Result<()> {
    // `pty: true` units end with the old daemon's pseudo consoles.
    let yaml = fixture_yaml("two-services")?.replace(
        "    run: [fixture-server, --log-line",
        "    pty: false\n    run: [fixture-server, --log-line",
    );
    let lab = Lab::with_config(&yaml, BUDGET)?;
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let lanes = lab.lanes(lab.repo())?;
    let before = need(lane(&lanes, "fixture-two@0"), "lane 0")?;
    let api = need(pid(need(unit(before, "api"), "api")?), "api pid")?;
    let web = need(pid(need(unit(before, "web"), "web")?), "web pid")?;
    let daemon = need(lab.daemon_pid()?, "daemon pid")?;

    kill(daemon);
    lab.eventually("the daemon to exit", Duration::from_secs(10), || {
        Ok((!alive(daemon) && !lab.daemon_running()).then_some(()))
    })?;
    assert!(alive(api) && alive(web), "pipe units outlive the daemon");

    let crashed = lab.eventually("both units to be orphaned", Duration::from_secs(15), || {
        let lanes = lab.lanes(lab.repo())?;
        Ok(lane(&lanes, "fixture-two@0")
            .filter(|lane| units_in(lane, "orphaned") == ["api", "web"])
            .cloned())
    })?;
    assert_ne!(need(lab.daemon_pid()?, "new daemon pid")?, daemon);
    assert_eq!(
        pid(need(unit(&crashed, "api"), "api")?),
        Some(api),
        "{crashed}"
    );

    let refused = lab.devlanes(lab.repo(), &["start", "api"])?;
    refused.expect_code(1)?;
    assert!(
        format!("{}{}", refused.stdout, refused.stderr)
            .contains("devlanes stop api --lane fixture-two@0"),
        "{refused}"
    );
    assert!(alive(api), "a refused start leaves the orphan alone");

    lab.json(lab.repo(), &["stop"])?;
    lab.eventually(
        "the orphans to be terminated",
        Duration::from_secs(15),
        || Ok((!alive(api) && !alive(web)).then_some(())),
    )?;

    let again = lab.json(lab.repo(), &["start"])?;
    assert_eq!(again["ready"], true, "{again}");
    let lanes = lab.lanes(lab.repo())?;
    let back = need(lane(&lanes, "fixture-two@0"), "lane 0 after the restart")?;
    assert_eq!(units_in(back, "ready"), ["api", "web"], "{back}");
    let (status, _) = http_get(
        need(port(need(unit(back, "api"), "api")?), "api port")?,
        "/healthz",
    )?;
    assert_eq!(status, 200);
    Ok(())
}

/// A batch file gets its arguments through cmd.exe, which cannot take a line
/// break safely: the start fails with the typed reason instead of running a cut
/// command line.
#[test]
fn batch_file_with_an_unsafe_argument_is_refused() -> Result<()> {
    let yaml = format!(
        "{}  shim:\n    run: [./shim.cmd, \"first\\nsecond\"]\n",
        fixture_yaml("two-services")?
    );
    let lab = Lab::with_config(&yaml, BUDGET)?;
    lab.write("shim.cmd", "@echo off\r\necho shim-ran %*\r\n")?;
    let output = lab.devlanes(lab.repo(), &["start", "shim", "--json"])?;
    output.expect_code(1)?;
    let result = output.json()?;
    let failure = &result["failures"][0];
    assert_eq!(failure["reason"]["code"], "spawn_failed", "{result}");
    assert_eq!(
        failure["reason"]["data"]["reason"], "unsafe_batch_argument",
        "{result}"
    );
    let logs = lab.json(lab.repo(), &["logs", "shim", "--json"])?;
    assert!(!logs.to_string().contains("shim-ran"), "{logs}");
    Ok(())
}
