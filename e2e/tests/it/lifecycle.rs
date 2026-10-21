//! The daemon owns lanes: a client that disappears mid-start and a daemon that dies.

use std::time::Duration;

use devlanes_e2e::{Lab, Result, alive, fixture_yaml, http_get, kill, lane, pid, port, unit};

use crate::{BUDGET, need, units_in};

/// A service that takes three seconds before it listens, so a start can be interrupted.
const SLOW: &str = "\
project: fixture-slow
services:
  slow:
    run: [sh, -c, 'sleep 3; exec fixture-server --log-line slow-ready']
    ready: { http: { path: /healthz } }
";

/// Killing the CLI during `start` only detaches it: the daemon brings the lane up.
#[test]
fn killed_client_during_start_still_brings_the_lane_up() -> Result<()> {
    let lab = Lab::with_config(SLOW, BUDGET)?;
    let mut client = lab.spawn(lab.command(lab.repo(), &["start"])?)?;

    let state = lab.eventually("slow to be starting", Duration::from_secs(30), || {
        let lanes = lab.lanes(lab.repo())?;
        let state = lane(&lanes, "fixture-slow@0")
            .and_then(|l| unit(l, "slow"))
            .and_then(|u| u["state"].as_str().map(str::to_owned));
        Ok(state.filter(|s| s == "starting" || s == "queued"))
    })?;
    assert!(
        client.try_wait()?.is_none(),
        "start returned before the service was ready"
    );
    client.kill()?;
    client.wait()?;
    assert_ne!(state, "ready");

    let slow = lab.eventually("slow to be ready", Duration::from_secs(30), || {
        let lanes = lab.lanes(lab.repo())?;
        Ok(lane(&lanes, "fixture-slow@0")
            .and_then(|l| unit(l, "slow"))
            .filter(|u| u["state"] == "ready")
            .cloned())
    })?;
    let (status, _) = http_get(need(port(&slow), "slow port")?, "/healthz")?;
    assert_eq!(status, 200);
    Ok(())
}

/// After SIGKILL of the daemon the next command spawns a new one. A unit whose
/// process survived (identity verified) keeps running as `orphaned` (decision 16):
/// `start` refuses it and names the stop command, `stop` terminates it, and
/// `lane resume` then brings the lane back.
#[test]
fn daemon_crash_reconciles_and_resumes() -> Result<()> {
    // A pipe-runner service survives its daemon (no PTY hangup), so reconciliation has
    // a real orphan; the PTY services die with the old daemon's terminals.
    let yaml = format!(
        "{}  bg:\n    run: [fixture-server, --log-line, bg-ready]\n    pty: false\n    ready: {{ http: {{ path: /healthz }} }}\n",
        fixture_yaml("two-services")?
    );
    let lab = Lab::with_config(&yaml, BUDGET)?;
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let lanes = lab.lanes(lab.repo())?;
    let before = need(lane(&lanes, "fixture-two@0"), "lane 0")?;
    let orphan = need(pid(need(unit(before, "bg"), "bg")?), "bg pid")?;
    let daemon = need_pid(lab.daemon_pid()?)?;

    kill(daemon);
    lab.eventually("the daemon to exit", Duration::from_secs(10), || {
        Ok((!alive(daemon) && !lab.daemon_running()).then_some(()))
    })?;
    assert!(alive(orphan), "the pipe service should outlive the daemon");

    lab.lanes(lab.repo())?;
    let respawned = need_pid(lab.daemon_pid()?)?;
    assert_ne!(respawned, daemon, "a new daemon must have been spawned");
    // The PTY services may still be exiting at reconcile; the liveness check clears them.
    let crashed = lab.eventually("only bg to be orphaned", Duration::from_secs(15), || {
        let lanes = lab.lanes(lab.repo())?;
        Ok(lane(&lanes, "fixture-two@0")
            .filter(|lane| units_in(lane, "orphaned") == ["bg"])
            .cloned())
    })?;
    assert!(alive(orphan), "the new daemon leaves the orphan running");
    let bg = need(unit(&crashed, "bg"), "bg")?;
    assert_eq!(pid(bg), Some(orphan), "{bg}");
    assert!(bg["orphaned_since_ms"].is_u64(), "{bg}");
    assert_eq!(crashed["state"], "running", "{crashed}");
    assert!(units_in(&crashed, "ready").is_empty(), "{crashed}");

    let refused = lab.devlanes(lab.repo(), &["start", "bg"])?;
    refused.expect_code(1)?;
    assert!(
        refused.stdout.contains("unit_orphaned") || refused.stderr.contains("unit_orphaned"),
        "{refused}"
    );
    assert!(
        format!("{}{}", refused.stdout, refused.stderr)
            .contains("devlanes stop bg --lane fixture-two@0"),
        "{refused}"
    );
    assert!(alive(orphan), "a refused start leaves the orphan alone");

    let stopped = lab.json(lab.repo(), &["stop", "bg"])?;
    assert!(stopped.to_string().contains("\"bg\""), "{stopped}");
    lab.eventually(
        "the orphan to be terminated",
        Duration::from_secs(15),
        || Ok((!alive(orphan)).then_some(())),
    )?;

    let resumed = lab.devlanes(lab.repo(), &["lane", "resume"])?;
    resumed.expect_code(0)?;
    let lanes = lab.lanes(lab.repo())?;
    let back = need(lane(&lanes, "fixture-two@0"), "lane 0 after resume")?;
    assert_eq!(back["needs_env"], false, "{back}");
    assert_eq!(back["state"], "running", "{back}");
    assert_eq!(units_in(back, "ready"), ["api", "web", "bg"], "{back}");
    let api = need(unit(back, "api"), "api")?;
    let (status, _) = http_get(need(port(api), "api port")?, "/healthz")?;
    assert_eq!(status, 200);
    Ok(())
}

/// A running daemon must report its PID.
fn need_pid(pid: Option<i32>) -> Result<i32> {
    pid.ok_or_else(|| devlanes_e2e::Error::Unexpected("no daemon is running".into()))
}
