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
        // Once the leader is gone its session ID could name an unrelated session, so
        // new members are adopted only while the session is provably still ours: the
        // leader is listed or unreaped, or a process we already track is in it.
        let known: Vec<ProcessIdentity> =
            self.tracked.values().map(ProcessInfo::identity).collect();
        if root_present || self.root_held || session_anchored(snapshot, &self.root, &known) {
            for process in snapshot {
                // A member older than the leader, or a different process holding the
                // leader's PID, belongs to a session that merely reuses the ID.
                if self.root.sid != 0
                    && process.sid == Some(self.root.sid)
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
        // Snapshots can omit an exited leader whose PID is still held (Windows
        // drops exited processes, macOS may drop zombies). Its PID cannot be
        // reused meanwhile, so children it started just before exiting are still
        // its own.
        if !root_present && self.root_held {
            pending.push((self.root.pid, self.root.start_time));
        }
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

    /// Tracks `process` as a member known by other means, such as Windows job
    /// membership, which catches children of a parent that exited between two
    /// snapshots. Its own descendants are then found by the next [`Self::update`].
    /// A process older than the root cannot be one of its descendants.
    pub fn adopt(&mut self, process: &ProcessInfo) {
        if process.start_time >= self.root.start_time {
            self.tracked
                .entry(process.pid)
                .or_insert_with(|| process.clone());
        }
    }
}

/// Selects the leader and live members of its session from a snapshot.
///
/// For start-up reconciliation of a crashed daemon's units. Members count only
/// while the snapshot still holds the leader with its recorded start time: once
/// the leader is gone its PID, and so its session ID, can belong to an unrelated
/// session, which must never be swept. Members must also have started no earlier
/// than the leader.
pub fn session_members(snapshot: &[ProcessInfo], leader: &ProcessIdentity) -> Vec<ProcessInfo> {
    session_members_known(snapshot, leader, &[])
}

/// Like [`session_members`], but the session is also trusted after its leader is
/// gone if one of `known` (identities the caller recorded as members of this
/// session, such as a supervisor's tracked set) is still in it.
///
/// That is sound because neither Linux nor macOS hands out a PID while it is still
/// in use as a session ID, so a session that still has one of our members cannot
/// have been replaced by a stranger's session with the same ID.
pub fn session_members_known(
    snapshot: &[ProcessInfo],
    leader: &ProcessIdentity,
    known: &[ProcessIdentity],
) -> Vec<ProcessInfo> {
    let leader_present = snapshot
        .iter()
        .any(|p| p.pid == leader.pid && p.start_time == leader.start_time);
    if !leader_present && !session_anchored(snapshot, leader, known) {
        return Vec::new();
    }
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

/// True when a `known` member, matched by PID and start time, is still listed in
/// the leader's session. Such a member keeps the session ID from being reused.
fn session_anchored(
    snapshot: &[ProcessInfo],
    leader: &ProcessIdentity,
    known: &[ProcessIdentity],
) -> bool {
    leader.sid != 0
        && snapshot.iter().any(|p| {
            p.sid == Some(leader.sid)
                && p.start_time >= leader.start_time
                && known
                    .iter()
                    .any(|k| k.pid == p.pid && k.start_time == p.start_time)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, sid: u32, start_time: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            pgid: Some(pid),
            sid: Some(sid),
            uid: Some(501),
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

    /// An adopted member is tracked with its descendants from the next update, even
    /// though its parent was never seen; a process older than the root is not.
    #[test]
    fn adopted_members_bring_their_descendants() {
        let mut tracker = DescendantTracker::new(root());
        // 300's parent 150 exited before any snapshot saw it; 300 has no session.
        let orphan = proc(300, 150, 300, 30);
        tracker.update(&[proc(100, 1, 100, 10), orphan.clone()]);
        assert_eq!(pids(&tracker), [100]);
        tracker.adopt(&orphan);
        tracker.adopt(&proc(50, 1, 50, 5));
        tracker.update(&[proc(100, 1, 100, 10), orphan, proc(301, 300, 301, 31)]);
        assert_eq!(pids(&tracker), [100, 300, 301]);
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

    /// After the leader is gone, session members are still adopted while a tracked
    /// member keeps the session alive (its ID cannot be reused then), including
    /// orphans whose parent was never seen. Once none remains, a process carrying
    /// the old session ID belongs to someone else.
    #[test]
    fn session_adoption_after_root_needs_a_tracked_member() {
        let mut tracker = DescendantTracker::new(root());
        tracker.update(&[proc(100, 1, 100, 10), proc(101, 100, 100, 11)]);
        // 200 was forked and orphaned between snapshots.
        tracker.update(&[proc(101, 1, 100, 11), proc(200, 1, 100, 20)]);
        assert_eq!(pids(&tracker), [101, 200]);
        // Every member is gone; the ID now names a stranger's session.
        tracker.update(&[proc(300, 1, 100, 30)]);
        assert!(pids(&tracker).is_empty());
        tracker.update(&[proc(300, 1, 100, 30), proc(301, 1, 100, 31)]);
        assert!(pids(&tracker).is_empty());
        // An unreaped root keeps its session trustworthy even when not listed.
        tracker.hold_root(true);
        tracker.update(&[proc(301, 1, 100, 31)]);
        assert_eq!(pids(&tracker), [301]);
    }

    /// A held root missing from the snapshot still brings in children it started
    /// before exiting; once released, its PID may be a stranger's.
    #[test]
    fn held_root_absent_from_the_snapshot_keeps_its_children() {
        let mut tracker = DescendantTracker::new(root());
        tracker.update(&[proc(100, 1, 100, 10)]);
        tracker.hold_root(true);
        tracker.update(&[proc(500, 100, 500, 12)]);
        assert_eq!(pids(&tracker), [500]);
        let mut released = DescendantTracker::new(root());
        released.update(&[proc(100, 1, 100, 10)]);
        released.update(&[proc(500, 100, 500, 12)]);
        assert!(pids(&released).is_empty());
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
        assert!(session_members(&snapshot, &stale).is_empty());
    }

    /// A dead leader's session ID reused by an unrelated, newer session selects
    /// nothing, although its processes started after the recorded leader.
    #[test]
    fn reused_session_of_a_dead_leader_is_not_swept() {
        let snapshot = [
            proc(1, 0, 1, 0),
            proc(100, 1, 100, 50),
            proc(101, 100, 100, 51),
        ];
        assert!(session_members(&snapshot, &root()).is_empty());
        let without_leader = [proc(1, 0, 1, 0), proc(101, 1, 100, 51)];
        assert!(session_members(&without_leader, &root()).is_empty());
        // Known identities from an earlier session holder do not anchor a reuse.
        let earlier = ProcessIdentity {
            pid: 101,
            sid: 100,
            start_time: 11,
        };
        assert!(session_members_known(&without_leader, &root(), &[earlier]).is_empty());
    }

    /// A supervisor that recorded a member can still sweep the session after the
    /// leader is reaped, including members it never saw.
    #[test]
    fn known_member_anchors_a_leaderless_session() {
        let member = ProcessIdentity {
            pid: 101,
            sid: 100,
            start_time: 11,
        };
        let snapshot = [
            proc(1, 0, 1, 0),
            proc(101, 1, 100, 11),
            proc(102, 1, 100, 12),
            proc(103, 1, 100, 9),
        ];
        let found: Vec<u32> = session_members_known(&snapshot, &root(), &[member])
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, [101, 102]);
        assert!(session_members(&snapshot, &root()).is_empty());
    }
}
