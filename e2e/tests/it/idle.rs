//! Idle lanes with the real runner: a lane freezes after `lanes.freeze_after`,
//! a direct connection to a service port thaws it, and a frozen lane still stops.

use std::time::Duration;

use devlanes_e2e::{Lab, Result, http_get, lane, port, unit};
use serde_json::Value;

use crate::{BUDGET, need};

/// The lane's power level word, `None` while it runs normally.
fn level(lab: &Lab) -> Result<Option<String>> {
    let lanes = lab.lanes(lab.repo())?;
    Ok(lane(&lanes, "fixture-two@0")
        .and_then(|lane| lane["power"]["level"].as_str())
        .map(str::to_owned))
}

/// A service's process state letters from `ps` (`T` while stopped).
#[cfg(unix)]
fn process_state(pid: i64) -> String {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Freezes on its own, thaws on a direct connection that bypasses the proxy,
/// and `debug freeze` followed by `stop` ends the lane cleanly.
#[test]
fn idle_lane_freezes_and_a_connection_thaws_it() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let config = lab.home().join("config.yaml");
    let mut user = std::fs::read_to_string(&config)?;
    user.push_str("lanes:\n  throttle_after: 0s\n  freeze_after: 3s\n");
    std::fs::write(&config, user)?;

    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let lanes = lab.lanes(lab.repo())?;
    let api: Value = need(
        lane(&lanes, "fixture-two@0").and_then(|l| unit(l, "api")),
        "api",
    )?
    .clone();
    let api_port = need(port(&api), "api port")?;

    lab.eventually("the lane to freeze", Duration::from_secs(30), || {
        Ok((level(&lab)?.as_deref() == Some("frozen")).then_some(()))
    })?;
    let lanes = lab.lanes(lab.repo())?;
    let frozen = need(
        lane(&lanes, "fixture-two@0").and_then(|l| unit(l, "api")),
        "api",
    )?;
    assert_eq!(frozen["state"], "ready", "{frozen}");
    assert_eq!(frozen["power"], "frozen", "{frozen}");
    #[cfg(unix)]
    {
        let pid = need(frozen["pid"].as_i64(), "api pid")?;
        assert!(
            process_state(pid).contains('T'),
            "api is stopped: {}",
            process_state(pid)
        );
    }

    // The proxy never sees this request; the connection alone wakes the lane.
    let (status, _) = http_get(api_port, "/healthz")?;
    assert_eq!(status, 200);
    lab.eventually("the lane to thaw", Duration::from_secs(10), || {
        Ok(level(&lab)?.is_none().then_some(()))
    })?;

    lab.json(lab.repo(), &["debug", "freeze"])?;
    assert_eq!(level(&lab)?.as_deref(), Some("frozen"));
    lab.json(lab.repo(), &["stop"])?;
    let lanes = lab.lanes(lab.repo())?;
    let after = need(lane(&lanes, "fixture-two@0"), "lane")?;
    assert!(after["power"].is_null(), "{after}");
    assert!(
        after["units"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|unit| unit["state"] != "ready"),
        "{after}"
    );
    Ok(())
}
