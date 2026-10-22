//! Worktree detection while the daemon runs (plan M5): `git worktree add`, `move` and
//! `remove` reach `status` through the daemon's watcher, without a command that
//! re-lists worktrees.

use std::{path::Path, time::Duration};

use devlanes_e2e::{Lab, Result, alive, http_get, lane, listening, pid, port, unit};
use serde_json::Value;

use crate::{BUDGET, need, units_in};

/// How long a worktree change may take to show. The plan asks for a second; the
/// margin covers loaded CI machines.
const DETECT: Duration = Duration::from_secs(5);

/// The lanes of `status --json`. Unlike `lane ls`, `status` does not re-list
/// worktrees, so what it shows came from the watcher.
fn watched(lab: &Lab) -> Result<Vec<Value>> {
    lab.lanes(lab.repo())
}

/// Polls `status` until `probe` accepts the lane `reference`.
fn lane_where(
    lab: &Lab,
    what: &str,
    reference: &str,
    probe: impl Fn(&Value) -> bool,
) -> Result<Value> {
    lab.eventually(what, DETECT, || {
        Ok(lane(&watched(lab)?, reference)
            .filter(|lane| probe(lane))
            .cloned())
    })
}

/// The lane's path as `status` reports it, canonicalized like `expected`.
fn same_path(lane: &Value, expected: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    lane["path"]
        .as_str()
        .is_some_and(|path| canonical(Path::new(path)) == canonical(expected))
}

/// `git worktree remove --force` while the worktree's units run.
#[cfg(unix)]
fn remove_running_worktree(lab: &Lab, path: &Path) -> Result<()> {
    lab.git(&["worktree", "remove", "--force", &path.to_string_lossy()])
}

/// `git worktree remove --force` while the worktree's units run. Windows refuses
/// to delete a directory a running process uses as its cwd, so Git deletes the
/// `.git` link, the files and its own record, then fails on the directories.
/// The daemon must still see the worktree as removed.
#[cfg(windows)]
fn remove_running_worktree(lab: &Lab, path: &Path) -> Result<()> {
    // The failure is expected; the assertion below checks what it left.
    let _refused = lab.git(&["worktree", "remove", "--force", &path.to_string_lossy()]);
    assert!(
        !path.join(".git").exists(),
        "git worktree remove left the .git link in {}",
        path.display()
    );
    Ok(())
}

/// Add, move and remove worktrees while lane 0 runs: a new branch worktree and a
/// detached one appear as idle lanes with ports and hostnames; a move keeps the lane;
/// removing a running worktree stops its lane and keeps its record and logs; lane 0
/// is never touched.
#[test]
fn worktree_add_move_and_remove_reach_the_running_daemon() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let pids_zero = |lanes: &[Value]| -> Vec<Option<i32>> {
        lane(lanes, "fixture-two@0")
            .and_then(|l| l["units"].as_array())
            .into_iter()
            .flatten()
            .map(pid)
            .collect()
    };
    let before = pids_zero(&watched(&lab)?);
    assert!(before.iter().all(Option::is_some), "{before:?}");

    // Add: a branch with a slash, then a detached HEAD.
    let first = lab.add_worktree("feat/watch")?;
    let one = lane_where(&lab, "the new worktree as lane 1", "fixture-two@1", |l| {
        same_path(l, &first)
    })?;
    assert_eq!(one["branch"], "feat/watch", "{one}");
    assert_eq!(one["state"], "idle", "{one}");
    assert!(one["port_block"].is_array(), "lane 1 has ports: {one}");
    assert_eq!(one["domain"], "fixture-two-1.localhost", "{one}");
    let api = need(unit(&one, "api"), "lane 1 api")?;
    assert!(port(api).is_some(), "{api}");
    assert!(
        api["urls"]
            .as_array()
            .is_some_and(|urls| urls.iter().any(|u| u
                .as_str()
                .is_some_and(|u| u.starts_with("https://api.fixture-two-1.localhost")))),
        "lane 1 api has its hostname: {api}"
    );

    let detached = lab.worktrees_dir().join("detached");
    lab.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        &detached.to_string_lossy(),
    ])?;
    let two = lane_where(
        &lab,
        "the detached worktree as lane 2",
        "fixture-two@2",
        |l| same_path(l, &detached),
    )?;
    assert_eq!(two["state"], "idle", "{two}");
    assert_ne!(two["port_block"], one["port_block"], "{two}");

    // Move: the same lane, now at the new path.
    let moved = lab.worktrees_dir().join("moved");
    lab.git(&[
        "worktree",
        "move",
        &first.to_string_lossy(),
        &moved.to_string_lossy(),
    ])?;
    let one_moved = lane_where(&lab, "lane 1 at its new path", "fixture-two@1", |l| {
        same_path(l, &moved)
    })?;
    assert_eq!(one_moved["branch"], "feat/watch", "{one_moved}");
    assert_eq!(one_moved["port_block"], one["port_block"], "{one_moved}");
    assert_eq!(
        watched(&lab)?.len(),
        3,
        "a move must not add a lane: {one_moved}"
    );

    // Start the moved lane, then remove its worktree under it.
    let started = lab.json(&moved, &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let running = need(lane(&watched(&lab)?, "fixture-two@1"), "lane 1")?.clone();
    assert_eq!(units_in(&running, "ready"), ["api", "web"], "{running}");
    let one_pids: Vec<i32> = running["units"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(pid)
        .collect();
    let one_ports: Vec<u16> = running["units"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(port)
        .collect();
    assert_eq!(one_pids.len(), 2, "{running}");
    remove_running_worktree(&lab, &moved)?;

    let stopped = lab.eventually("lane 1 to stop", Duration::from_secs(20), || {
        let lanes = watched(&lab)?;
        let done = one_pids.iter().all(|p| !alive(*p))
            && one_ports.iter().all(|p| !listening(*p))
            && lane(&lanes, "fixture-two@1").is_some_and(|l| units_in(l, "ready").is_empty());
        Ok(done.then_some(lanes))
    })?;
    let gone = need(lane(&stopped, "fixture-two@1"), "lane 1 after removal")?;
    assert_ne!(gone["state"], "running", "{gone}");
    // What Git could not delete on Windows goes once the units stopped.
    #[cfg(windows)]
    lab.eventually("the leftover worktree directory to delete", DETECT, || {
        Ok(std::fs::remove_dir_all(&moved).ok())
    })?;
    let logs = lab.json(
        lab.repo(),
        &["logs", "fixture-two@1:api", "--tail", "20", "--json"],
    )?;
    assert!(
        logs["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|l| l["text"] == "api-ready")),
        "a removed lane keeps its logs: {logs}"
    );

    // The detached lane and lane 0 are untouched.
    let after = watched(&lab)?;
    let two_after = need(lane(&after, "fixture-two@2"), "lane 2")?;
    assert!(same_path(two_after, &detached), "{two_after}");
    assert_eq!(pids_zero(&after), before, "lane 0 was restarted");
    let zero = need(lane(&after, "fixture-two@0"), "lane 0")?;
    for name in ["api", "web"] {
        let (status, _) = http_get(
            need(port(need(unit(zero, name), name)?), "port")?,
            "/healthz",
        )?;
        assert_eq!(status, 200, "lane 0 {name}");
    }
    Ok(())
}
