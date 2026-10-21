//! Pure descendant tracking over process-table snapshots.

use std::collections::{BTreeMap, HashMap};

use crate::{ProcessIdentity, ProcessInfo};

/// Accumulates a unit's process tree across periodic snapshots.
///
/// A process joins when it shares the root's session or when its parent is already
/// tracked. Membership survives reparenting, so a daemonized grandchild that called
/// `setsid` stays tracked once any snapshot saw it under a tracked parent. A process
/// forked and orphaned between two snapshots outside the session cannot be observed.
#[derive(Debug, Clone)]
pub struct DescendantTracker {
    root: ProcessIdentity,
    tracked: BTreeMap<u32, ProcessInfo>,
    root_held: bool,
}

impl DescendantTracker {
    /// Starts tracking from a session leader; `root.sid == 0` disables session matching.
    pub fn new(root: ProcessIdentity) -> Self {
        Self {
            root,
            tracked: BTreeMap::new(),
            root_held: false,
        }
    }

    /// Declares whether the caller is the root's parent and has not reaped it yet.
    /// While held, the root's PID (and so its session ID) cannot be recycled, so
    /// session members keep being adopted even when a platform omits the zombie root
    /// from its process table (macOS does).
    pub fn hold_root(&mut self, held: bool) {
        self.root_held = held;
    }

    /// Returns the identity the tracker was created for.
    pub fn root(&self) -> ProcessIdentity {
        self.root
    }

    /// Applies one full snapshot: forgets exited or recycled PIDs, then adds new members.
    pub fn update(&mut self, snapshot: &[ProcessInfo]) {
        let by_pid: HashMap<u32, &ProcessInfo> = snapshot.iter().map(|p| (p.pid, p)).collect();
        self.tracked.retain(|pid, known| {
            by_pid
                .get(pid)
                .is_some_and(|current| current.start_time == known.start_time)
        });
        for (pid, known) in &mut self.tracked {
            if let Some(current) = by_pid.get(pid) {
                // Parent and session change on reparenting or setsid; keep them current.
                known.clone_from(current);
            }
        }

        let root_present = match by_pid.get(&self.root.pid) {
            Some(root) if root.start_time == self.root.start_time => {
                self.tracked.insert(root.pid, (*root).clone());
                true
            }
            _ => false,
        };
        // Once the leader is gone its session ID can be handed to an unrelated new
        // session, so only members seen while the leader existed are trusted; later
        // ones still join through a tracked parent.
        if (root_present || self.root_held) && self.root.sid != 0 {
            for process in snapshot {
                // A member older than the leader, or a different process holding the
                // leader's PID, belongs to a session that merely reuses the ID.
                if process.sid == Some(self.root.sid)
                    && process.pid != self.root.pid
                    && process.start_time >= self.root.start_time
                {
                    self.tracked
                        .entry(process.pid)
                        .or_insert_with(|| process.clone());
                }
            }
        }

        let mut children: HashMap<u32, Vec<&ProcessInfo>> = HashMap::new();
        for process in snapshot {
            children.entry(process.ppid).or_default().push(process);
        }
        let mut pending: Vec<(u32, u64)> = self
            .tracked
            .values()
            .map(|p| (p.pid, p.start_time))
            .collect();
        while let Some((parent, parent_start)) = pending.pop() {
            for child in children.get(&parent).into_iter().flatten() {
                // A child older than its supposed parent means the parent PID was recycled.
                if child.start_time >= parent_start && !self.tracked.contains_key(&child.pid) {
                    self.tracked.insert(child.pid, (*child).clone());
                    pending.push((child.pid, child.start_time));
                }
            }
        }
    }

    /// Iterates tracked processes, including the root and unreaped zombies.
    pub fn processes(&self) -> impl Iterator<Item = &ProcessInfo> {
        self.tracked.values()
    }

    /// Iterates tracked processes that can still receive signals.
    pub fn live(&self) -> impl Iterator<Item = &ProcessInfo> {
        self.tracked.values().filter(|p| !p.zombie)
    }
}

/// Selects the leader and live members of its session from a snapshot.
///
/// For start-up reconciliation of a crashed daemon's units: members must have started
/// no earlier than the leader, which rejects most sessions on a recycled PID.
pub fn session_members(snapshot: &[ProcessInfo], leader: &ProcessIdentity) -> Vec<ProcessInfo> {
    snapshot
        .iter()
        .filter(|p| !p.zombie)
        .filter(|p| {
            if p.pid == leader.pid {
                p.start_time == leader.start_time
            } else {
                leader.sid != 0 && p.sid == Some(leader.sid) && p.start_time >= leader.start_time
            }
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, sid: u32, start_time: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            pgid: pid,
            sid: Some(sid),
            uid: 501,
            start_time,
            zombie: false,
            name: format!("p{pid}"),
        }
    }

    fn root() -> ProcessIdentity {
        ProcessIdentity {
            pid: 100,
            sid: 100,
            start_time: 10,
        }
    }

    fn pids(tracker: &DescendantTracker) -> Vec<u32> {
        tracker.processes().map(|p| p.pid).collect()
    }

    /// A setsid grandchild stays tracked after its parent exits and it is reparented to init.
    #[test]
    fn reparented_escapee_stays_tracked() {
        let mut tracker = DescendantTracker::new(root());
        tracker.update(&[
            proc(1, 0, 1, 0),
            proc(100, 1, 100, 10),
            proc(101, 100, 100, 11),
            proc(102, 101, 102, 12),
        ]);
        assert_eq!(pids(&tracker), [100, 101, 102]);
        tracker.update(&[proc(1, 0, 1, 0), proc(102, 1, 102, 12)]);
        assert_eq!(pids(&tracker), [102]);
        assert_eq!(tracker.processes().next().map(|p| p.ppid), Some(1));
    }

    /// Session members join without a tracked parent; unrelated and recycled PIDs do not.
    #[test]
    fn session_membership_and_pid_reuse() {
        let mut tracker = DescendantTracker::new(root());
        tracker.update(&[
            proc(100, 1, 100, 10),
            proc(200, 1, 100, 20),
            proc(300, 1, 300, 30),
        ]);
        assert_eq!(pids(&tracker), [100, 200]);
        // PID 200 was recycled by an unrelated process with a new start time.
        tracker.update(&[proc(100, 1, 100, 10), proc(200, 1, 300, 40)]);
        assert_eq!(pids(&tracker), [100]);
        // A child older than its parent belongs to an earlier holder of the parent PID.
        tracker.update(&[proc(100, 1, 100, 10), proc(400, 100, 400, 5)]);
        assert_eq!(pids(&tracker), [100]);
    }

    /// After the leader is gone, a process carrying the old session ID is not adopted
    /// unless a tracked parent forked it.
    #[test]
    fn no_session_adoption_without_root() {
        let mut tracker = DescendantTracker::new(root());
        tracker.update(&[proc(100, 1, 100, 10), proc(101, 100, 100, 11)]);
        tracker.update(&[proc(101, 1, 100, 11), proc(200, 1, 100, 20)]);
        assert_eq!(pids(&tracker), [101]);
        tracker.update(&[proc(101, 1, 100, 11), proc(102, 101, 100, 21)]);
        assert_eq!(pids(&tracker), [101, 102]);
        // An unreaped root keeps its session trustworthy even when not listed.
        tracker.hold_root(true);
        tracker.update(&[proc(101, 1, 100, 11), proc(300, 1, 100, 30)]);
        assert_eq!(pids(&tracker), [101, 300]);
    }

    /// A recycled root PID is never adopted, and zombies are excluded from live members.
    #[test]
    fn root_identity_and_zombies() {
        let mut tracker = DescendantTracker::new(root());
        tracker.update(&[proc(100, 1, 100, 99)]);
        assert!(pids(&tracker).is_empty());
        let mut zombie = proc(100, 1, 100, 10);
        zombie.zombie = true;
        tracker.update(&[zombie]);
        assert_eq!(pids(&tracker), [100]);
        assert_eq!(tracker.live().count(), 0);
    }

    /// Reconciliation selects the verified leader and members no older than it.
    #[test]
    fn reconciliation_members() {
        let snapshot = [
            proc(100, 1, 100, 10),
            proc(101, 1, 100, 11),
            proc(102, 1, 100, 9),
            proc(103, 1, 103, 12),
        ];
        let found: Vec<u32> = session_members(&snapshot, &root())
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, [100, 101]);
        let stale = ProcessIdentity {
            start_time: 1,
            ..root()
        };
        let found: Vec<u32> = session_members(&snapshot, &stale)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, [101, 102]);
    }
}
