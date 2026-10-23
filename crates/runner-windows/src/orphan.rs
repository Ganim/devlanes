//! Stopping a unit a previous daemon started (an orphan). Its job cannot be
//! reopened once that daemon has exited, so the stop works from the identities
//! recorded while the unit ran, and from the process table.

use std::time::{Duration, Instant};

use devlanes_core::ProcessIdentity;
use devlanes_sys_windows::{
    console::CtrlEvent,
    proc::{self, Held, ProcessEntry, Terminated},
    window::close_windows,
};

use crate::{ConsoleHelper, spawn::is_console_host};

/// Exit code of the orphaned processes this ends.
const TERMINATED: u32 = 1;

/// What [`stop_orphan`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanStop {
    /// The PID the console event was sent through, `None` when none was sent.
    pub asked_through: Option<u32>,
    /// How many visible windows were asked to close.
    pub windows: usize,
    /// How many of the unit's processes ran when the stop began, its console
    /// host left out.
    pub running: usize,
    /// How many processes were still running after the grace period and were
    /// terminated.
    pub terminated: usize,
}

/// Stops an orphaned unit: the recorded `leader` and `members` and the live
/// descendants of those that still verify. Like a stop of a running unit, it
/// asks first: `WM_CLOSE` to the processes' visible windows and one
/// Ctrl-Break to the unit's console through `helper`, attached through the
/// leader while it verifies, else through the first live recorded member
/// that is not the console host. Never one event per process: everything on
/// the console gets the one event. After `grace`, or once every process has
/// exited, whatever still runs of that tree is terminated with exit code 1,
/// each checked by start time on the handle that ends it. Blocking.
pub fn stop_orphan(
    helper: Option<&ConsoleHelper>,
    leader: ProcessIdentity,
    members: &[ProcessIdentity],
    grace: Duration,
) -> OrphanStop {
    let deadline = Instant::now() + grace;
    let mut recorded: Vec<(u32, u64)> = Vec::with_capacity(members.len() + 1);
    for identity in std::iter::once(&leader).chain(members) {
        let key = (identity.pid, identity.start_time);
        if !recorded.contains(&key) {
            recorded.push(key);
        }
    }
    let tree = process_tree(&recorded);
    // Held so no PID is reused while windows and the console are reached by PID.
    let held: Vec<(&ProcessEntry, Held)> = tree
        .iter()
        .filter(|entry| !is_console_host(&entry.name))
        .filter_map(|entry| Some((entry, Held::open(entry.pid, entry.start_time).ok()??)))
        .collect();
    let mut stop = OrphanStop {
        running: held.len(),
        ..OrphanStop::default()
    };
    let targets: Vec<(u32, u64)> = held
        .iter()
        .map(|(entry, _)| (entry.pid, entry.start_time))
        .collect();
    match close_windows(&targets) {
        Ok(closed) => stop.windows = closed,
        Err(error) => tracing::debug!(%error, "closing an orphan's windows failed"),
    }
    // Recorded order puts the leader first.
    let through = recorded.iter().find_map(|key| {
        held.iter()
            .find(|(entry, _)| (entry.pid, entry.start_time) == *key)
            .map(|(_, handle)| handle)
    });
    if let (Some(helper), Some(target)) = (helper, through) {
        match helper.send(target, CtrlEvent::CtrlBreak, deadline) {
            Ok(()) => stop.asked_through = Some(target.pid()),
            Err(error) => {
                tracing::warn!(%error, pid = target.pid(), "console event to an orphan not sent");
            }
        }
    }
    for (_, handle) in &held {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _ = handle.wait(remaining);
    }
    // Read again: descendants started during the grace period count too.
    let roots: Vec<(u32, u64)> = tree.iter().map(|p| (p.pid, p.start_time)).collect();
    for entry in process_tree(&roots) {
        match proc::terminate_verified(entry.pid, entry.start_time, TERMINATED) {
            Ok(Terminated::Sent) => stop.terminated += usize::from(!is_console_host(&entry.name)),
            Ok(Terminated::Gone | Terminated::Mismatch) => {}
            Err(error) => {
                tracing::warn!(%error, pid = entry.pid, "terminating an orphaned process failed");
            }
        }
    }
    stop
}

/// The live processes among `roots` (`(pid, start_time)`) and their live
/// descendants, parents first. A child counts when its parent PID names a
/// process already in the tree and it started no earlier than that process,
/// which rules out a PID reused after the parent exited. Without a process
/// table, only the roots are looked up.
fn process_tree(roots: &[(u32, u64)]) -> Vec<ProcessEntry> {
    let snapshot = match proc::processes() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(%error, "reading the process table failed; only recorded processes are stopped");
            roots
                .iter()
                .filter_map(|&(pid, _)| proc::process(pid).ok().flatten())
                .collect()
        }
    };
    descendants(&snapshot, roots)
}

/// [`process_tree`] over a given process table.
fn descendants(snapshot: &[ProcessEntry], roots: &[(u32, u64)]) -> Vec<ProcessEntry> {
    let mut found: Vec<ProcessEntry> = snapshot
        .iter()
        .filter(|p| !p.exited && roots.contains(&(p.pid, p.start_time)))
        .cloned()
        .collect();
    let mut next = 0;
    while let Some(parent) = found.get(next).cloned() {
        next += 1;
        for child in snapshot {
            let fits = !child.exited
                && child.ppid == parent.pid
                && child.pid != parent.pid
                && child.start_time >= parent.start_time;
            if fits
                && !found
                    .iter()
                    .any(|p| p.pid == child.pid && p.start_time == child.start_time)
            {
                found.push(child.clone());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, ppid: u32, start_time: u64) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid,
            start_time,
            name: "node.exe".into(),
            exited: false,
        }
    }

    /// Children and grandchildren of live roots are found, parents first; a
    /// process whose parent PID was reused before it, an exited one and a
    /// root with another start time are not.
    #[test]
    fn descendants_follow_parents_in_creation_order() {
        let snapshot = [
            entry(10, 1, 100),
            entry(11, 10, 150),
            entry(12, 11, 160),
            // Its parent 10 was started later, so 10 is a reused PID.
            entry(13, 10, 50),
            ProcessEntry {
                exited: true,
                ..entry(14, 10, 170)
            },
            entry(20, 1, 300),
            entry(21, 20, 310),
        ];
        let pids = |found: Vec<ProcessEntry>| found.iter().map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(descendants(&snapshot, &[(10, 100)])), [10, 11, 12]);
        assert!(descendants(&snapshot, &[(20, 299)]).is_empty());
        assert_eq!(
            pids(descendants(&snapshot, &[(20, 300), (10, 100)])),
            [10, 20, 11, 21, 12]
        );
    }
}
