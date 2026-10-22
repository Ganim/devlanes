//! Worktrees while the daemon runs: a worktree becomes a lane once devlanes runs
//! in it; `git worktree move` and `remove` reach `status` through the daemon's
//! watcher, without a command that re-lists worktrees.

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

/// Add, move and remove worktrees while lane 0 runs: a new branch worktree and a
/// detached one are no lanes until `status` runs in them, then idle lanes with
/// ports and hostnames; a move keeps the lane; removing a running worktree stops
/// its lane, and once idle it is cleaned at the next re-list while its logs stay
/// on disk; lane 0 is never touched.
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
    // The watcher sees it, yet existing does not make it a lane.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        watched(&lab)?.len(),
        1,
        "a new worktree is no lane by itself"
    );
    lab.lanes(&first)?;
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
    lab.lanes(&detached)?;
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
    lab.git(&["worktree", "remove", "--force", &moved.to_string_lossy()])?;

    let stopped = lab.eventually("lane 1 to stop", Duration::from_secs(20), || {
        let lanes = watched(&lab)?;
        let done = one_pids.iter().all(|p| !alive(*p))
            && one_ports.iter().all(|p| !listening(*p))
            && lane(&lanes, "fixture-two@1").is_some_and(|l| units_in(l, "ready").is_empty());
        Ok(done.then_some(lanes))
    })?;
    let gone = need(lane(&stopped, "fixture-two@1"), "lane 1 after removal")?;
    assert_ne!(gone["state"], "running", "{gone}");
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

    // The next re-list cleans the idle lane whose folder is gone; its logs stay.
    lab.json(lab.repo(), &["lane", "ls"])?;
    // The daemon waits 5 s before it cleans, in case the folder comes back.
    lab.eventually("lane 1 to be cleaned", Duration::from_secs(20), || {
        Ok(lane(&watched(&lab)?, "fixture-two@1")
            .is_none()
            .then_some(()))
    })?;
    assert!(
        lab.home().join("logs/fixture-two/1/api").is_dir(),
        "a cleaned lane keeps its logs"
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
