//! Daemon shutdown with a running lane: SIGTERM (a service manager, logout)
//! stops the units without `lane.stop` tasks, and `devlanes daemon stop` stops
//! them after running the tasks.

use std::time::Duration;

use devlanes_e2e::{Lab, Result, alive, lane, listening, pid, port, unit};
use serde_json::json;

use crate::{BUDGET, need, units_in};

/// A pipe service, which would outlive a daemon that did not stop it, and a
/// stop task that leaves a marker in `$HOME` (the lab's home).
const CONFIG: &str = r#"project: fixture-shutdown
services:
  api:
    run: [fixture-server, --log-line, api-ready]
    pty: false
    ready: { http: { path: /healthz } }
tasks:
  mark:
    run: 'echo stopped >> "$HOME/lane-stop-marker"'
    when: lane.stop
"#;

/// Starts the lane and returns the service's PID and port.
fn start(lab: &Lab) -> Result<(i32, u16)> {
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let lanes = lab.lanes(lab.repo())?;
    let api = need(
        lane(&lanes, "fixture-shutdown@0").and_then(|l| unit(l, "api")),
        "api",
    )?;
    let port = need(port(api), "api port")?;
    assert!(listening(port), "api listens on {port}");
    Ok((need(pid(api), "api pid")?, port))
}

/// Waits until the daemon process `daemon` is gone and released its lock.
fn wait_for_exit(lab: &Lab, daemon: i32) -> Result<()> {
    lab.eventually("the daemon to exit", Duration::from_secs(30), || {
        Ok((!alive(daemon) && !lab.daemon_running()).then_some(()))
    })
}

/// SIGTERM to the daemon stops the service (process gone, port closed) and
/// closes its run, so the next daemon finds the lane idle with no orphan; the
/// stop task does not run.
#[cfg(unix)]
#[test]
fn sigterm_stops_units_without_lane_stop_tasks() -> Result<()> {
    signal_stops_units_without_lane_stop_tasks(|daemon| {
        let target = need(rustix::process::Pid::from_raw(daemon), "daemon pid")?;
        rustix::process::kill_process(target, rustix::process::Signal::TERM)
            .map_err(|error| devlanes_e2e::Error::Io(error.into()))
    })
}

/// Logoff (the session-end messages Windows sends the daemon's hidden window)
/// is the Windows signal: the same as SIGTERM.
#[cfg(windows)]
#[test]
fn logoff_stops_units_without_lane_stop_tasks() -> Result<()> {
    use devlanes_sys::windows::session::{SessionEnd, simulate_for};
    signal_stops_units_without_lane_stop_tasks(|daemon| {
        let daemon = need(u32::try_from(daemon).ok(), "daemon pid")?;
        simulate_for(daemon, SessionEnd::Logoff, Duration::from_secs(20))?;
        Ok(())
    })
}

/// Sends the daemon a signal with `send` and checks the shutdown that follows.
fn signal_stops_units_without_lane_stop_tasks(send: impl FnOnce(i32) -> Result<()>) -> Result<()> {
    let lab = Lab::with_config(CONFIG, BUDGET)?;
    let (api, port) = start(&lab)?;
    let daemon = need(lab.daemon_pid()?, "daemon pid")?;
    send(daemon)?;
    wait_for_exit(&lab, daemon)?;
    assert!(!alive(api), "api (pid {api}) still runs");
    assert!(!listening(port), "port {port} still open");
    assert!(
        !lab.home().join("lane-stop-marker").exists(),
        "a signal shutdown ran the lane.stop task"
    );

    // `status` spawns a new daemon, which reads the closed run from state.db.
    let lanes = lab.lanes(lab.repo())?;
    let zero = need(lane(&lanes, "fixture-shutdown@0"), "lane 0")?;
    assert_eq!(zero["state"], "idle", "{zero}");
    assert!(units_in(zero, "orphaned").is_empty(), "{zero}");
    let respawned = need(lab.daemon_pid()?, "new daemon pid")?;
    assert_ne!(respawned, daemon, "a new daemon answered");
    Ok(())
}

/// `devlanes daemon restart` starts what ran again on the new daemon, with the
/// lane's profiles; `daemon stop` then leaves it stopped for the next daemon.
#[test]
fn daemon_restart_brings_back_what_ran() -> Result<()> {
    let lab = Lab::with_config(CONFIG, BUDGET)?;
    let (api, _) = start(&lab)?;
    let daemon = need(lab.daemon_pid()?, "daemon pid")?;
    let restarted = lab.json(lab.repo(), &["daemon", "restart", "--yes"])?;
    let resumed = restarted["resumed"].as_array().cloned().unwrap_or_default();
    assert_eq!(resumed.len(), 1, "{restarted}");
    assert_eq!(resumed[0]["lane"], "fixture-shutdown@0", "{restarted}");
    assert_eq!(
        resumed[0]["started"],
        json!(["fixture-shutdown@0:api"]),
        "{restarted}"
    );
    assert!(!alive(api), "the old run (pid {api}) stopped");
    let lanes = lab.lanes(lab.repo())?;
    let zero = need(lane(&lanes, "fixture-shutdown@0"), "lane 0")?;
    assert_eq!(units_in(zero, "ready"), ["api"], "{zero}");
    assert_ne!(need(lab.daemon_pid()?, "new daemon pid")?, daemon);

    let new_daemon = need(lab.daemon_pid()?, "daemon pid")?;
    lab.json(lab.repo(), &["daemon", "stop", "--yes"])?;
    wait_for_exit(&lab, new_daemon)?;
    let lanes = lab.lanes(lab.repo())?;
    let zero = need(lane(&lanes, "fixture-shutdown@0"), "lane 0")?;
    assert!(
        units_in(zero, "ready").is_empty(),
        "stop keeps it stopped: {zero}"
    );
    Ok(())
}

/// `devlanes daemon stop` runs the stop task, then stops the service, and
/// reports both.
#[test]
fn daemon_stop_runs_lane_stop_tasks() -> Result<()> {
    let lab = Lab::with_config(CONFIG, BUDGET)?;
    let (api, port) = start(&lab)?;
    let daemon = need(lab.daemon_pid()?, "daemon pid")?;
    let stopped = lab.json(lab.repo(), &["daemon", "stop", "--yes"])?;
    assert_eq!(stopped["reason"], "explicit", "{stopped}");
    assert_eq!(
        stopped["stopped"],
        json!([{"lane": "fixture-shutdown@0", "unit": "api"}]),
        "{stopped}"
    );
    assert_eq!(
        stopped["tasks"],
        json!([{"lane": "fixture-shutdown@0", "task": "mark", "exit_code": 0}]),
        "{stopped}"
    );
    wait_for_exit(&lab, daemon)?;
    assert!(!alive(api), "api (pid {api}) still runs");
    assert!(!listening(port), "port {port} still open");
    let marker = std::fs::read_to_string(lab.home().join("lane-stop-marker"))?;
    assert_eq!(marker, "stopped\n");
    Ok(())
}
