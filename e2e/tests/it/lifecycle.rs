//! The daemon owns lanes: a client that disappears mid-start, a daemon that dies,
//! waits that time out and config edits picked up by the next start.

use std::time::{Duration, Instant};

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

    // Plain output words the refusal and names the stop command; only JSON may carry
    // the raw code.
    let mut command = lab.command(lab.repo(), &["start", "bg"])?;
    command.env("DEVLANES_OUTPUT", "plain");
    let refused = lab.run(command, devlanes_e2e::COMMAND_TIMEOUT)?;
    refused.expect_code(1)?;
    let all = format!("{}{}", refused.stdout, refused.stderr);
    assert!(
        all.contains(&format!(
            "is still running from a previous daemon (pid {orphan})"
        )),
        "{refused}"
    );
    assert!(
        all.contains("devlanes stop bg --lane fixture-two@0"),
        "{refused}"
    );
    assert!(!all.contains("unit_orphaned"), "raw code leaked: {refused}");
    let refused = lab.devlanes(lab.repo(), &["start", "bg"])?;
    refused.expect_code(1)?;
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

/// A service that listens but whose ready check never passes within the test.
const NEVER_READY: &str = "\
project: fixture-wait
services:
  stuck:
    run: [fixture-server, --log-line, stuck-booting]
    ready: { log: never-printed, timeout: 120s }
";

/// `wait --timeout 1s` on a unit that stays `starting` exits 124 soon after the
/// second, and leaves the unit starting.
#[test]
fn wait_timeout_on_a_never_ready_unit_exits_124() -> Result<()> {
    let lab = Lab::with_config(NEVER_READY, BUDGET)?;
    let mut client = lab.spawn(lab.command(lab.repo(), &["start"])?)?;
    let waited = (|| {
        lab.eventually("stuck to be starting", Duration::from_secs(30), || {
            let lanes = lab.lanes(lab.repo())?;
            Ok(lane(&lanes, "fixture-wait@0")
                .and_then(|l| unit(l, "stuck"))
                .filter(|u| u["state"] == "starting")
                .cloned())
        })?;
        let begun = Instant::now();
        let mut command = lab.command(lab.repo(), &["wait", "stuck", "--timeout", "1s"])?;
        command.env("DEVLANES_OUTPUT", "plain");
        let output = lab.run(command, Duration::from_secs(30))?;
        Ok::<_, devlanes_e2e::Error>((output, begun.elapsed()))
    })();
    let _ = client.kill();
    let _ = client.wait();
    let (output, elapsed) = waited?;
    output.expect_code(124)?;
    assert!(
        elapsed < Duration::from_secs(10),
        "wait took {elapsed:?}: {output}"
    );
    let lanes = lab.lanes(lab.repo())?;
    let stuck = need(
        lane(&lanes, "fixture-wait@0").and_then(|l| unit(l, "stuck")),
        "stuck",
    )?;
    assert_eq!(
        stuck["state"], "starting",
        "a wait timeout must not fail the unit: {stuck}"
    );
    Ok(())
}

/// `devlanes.yaml` before and after an edit: the edit adds `extra` and later changes
/// what `api` prints.
const EDIT_BEFORE: &str = "\
project: fixture-edit
services:
  api:
    run: [fixture-server, --log-line, api-v1]
    ready: { http: { path: /healthz } }
";

/// A `start` right after `devlanes.yaml` changes uses the new file, without waiting for
/// the daemon's config watcher: a new service starts while the lane runs, and a changed
/// command applies after a stop.
#[test]
fn start_right_after_an_edit_uses_the_new_config() -> Result<()> {
    let lab = Lab::with_config(EDIT_BEFORE, BUDGET)?;
    let first = lab.json(lab.repo(), &["start"])?;
    assert_eq!(first["ready"], true, "{first}");

    let added = format!(
        "{EDIT_BEFORE}  extra:\n    run: [fixture-server, --log-line, extra-ready]\n    ready: {{ http: {{ path: /healthz }} }}\n"
    );
    lab.write("devlanes.yaml", &added)?;
    let second = lab.json(lab.repo(), &["start"])?;
    assert_eq!(second["ready"], true, "{second}");
    assert_eq!(
        second["started"],
        serde_json::json!(["fixture-edit@0:extra"]),
        "{second}"
    );
    let lanes = lab.lanes(lab.repo())?;
    let zero = need(lane(&lanes, "fixture-edit@0"), "lane 0")?;
    assert_eq!(units_in(zero, "ready"), ["api", "extra"], "{zero}");

    lab.devlanes(lab.repo(), &["stop"])?.expect_code(0)?;
    lab.write("devlanes.yaml", &added.replace("api-v1", "api-v2"))?;
    let third = lab.json(lab.repo(), &["start"])?;
    assert_eq!(third["ready"], true, "{third}");
    let logs = lab.json(lab.repo(), &["logs", "api", "--tail", "20", "--json"])?;
    let texts: Vec<&str> = logs["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| line["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"api-v2"),
        "api runs the edited command: {logs}"
    );
    Ok(())
}

/// A running daemon must report its PID.
fn need_pid(pid: Option<i32>) -> Result<i32> {
    pid.ok_or_else(|| devlanes_e2e::Error::Unexpected("no daemon is running".into()))
}
