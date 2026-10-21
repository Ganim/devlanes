//! Two worktrees of `fixtures/two-services`: concurrency, references, idempotence,
//! stopping one lane and cleaning one lane.

use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use devlanes_e2e::{Lab, Result, http_get, lane, listening, pid, port, unit};
use serde_json::Value;

use crate::{BUDGET, need, units_in};

/// Starts lane 0 in the primary worktree and lane 1 in a `feat/x` worktree.
fn two_lanes() -> Result<(Lab, PathBuf)> {
    let lab = Lab::new("two-services", BUDGET)?;
    let feature = lab.add_worktree("feat/x")?;
    for cwd in [lab.repo().to_owned(), feature.clone()] {
        let started = lab.json(&cwd, &["start"])?;
        assert_eq!(
            started["ready"],
            true,
            "start in {}: {started}",
            cwd.display()
        );
    }
    Ok((lab, feature))
}

/// `(lane reference, unit name, port)` of every unit in `status --json`.
fn unit_ports(lanes: &[Value]) -> Vec<(String, String, u16)> {
    lanes
        .iter()
        .flat_map(|lane| {
            lane["units"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|u| {
                    Some((
                        lane["reference"].as_str()?.to_owned(),
                        u["name"].as_str()?.to_owned(),
                        port(u)?,
                    ))
                })
        })
        .collect()
}

/// Both lanes run at once with distinct ports; lane refs by number and by branch resolve
/// to lane 1; logs return the lines of the referenced lane's process.
#[test]
fn two_worktrees_run_side_by_side() -> Result<()> {
    let (lab, feature) = two_lanes()?;
    let lanes = lab.lanes(lab.repo())?;
    let zero = need(lane(&lanes, "fixture-two@0"), "lane fixture-two@0")?;
    let one = need(lane(&lanes, "fixture-two@1"), "lane fixture-two@1")?;
    assert_eq!(zero["branch"], "main");
    assert_eq!(one["branch"], "feat/x");
    for lane in [zero, one] {
        assert_eq!(lane["state"], "running", "{lane}");
        assert_eq!(units_in(lane, "ready"), ["api", "web"], "{lane}");
    }

    let ports = unit_ports(&lanes);
    assert_eq!(ports.len(), 4, "{ports:?}");
    let distinct: BTreeSet<u16> = ports.iter().map(|(_, _, p)| *p).collect();
    assert_eq!(
        distinct.len(),
        4,
        "ports must differ across lanes: {ports:?}"
    );
    for (lane, unit, port) in &ports {
        let (status, _) = http_get(*port, "/healthz")?;
        assert_eq!(status, 200, "{lane}:{unit} on {port}");
    }

    for reference in ["1", "feat/x", "fixture-two@1", "fixture-two@feat/x"] {
        let status = lab.json(lab.repo(), &["status", reference, "--json"])?;
        let items = status["items"].as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 1, "status {reference}: {status}");
        assert_eq!(items[0]["reference"], "fixture-two@1", "status {reference}");
    }

    let api_one = need(
        port(need(unit(one, "api"), "lane 1 api")?),
        "lane 1 api port",
    )?;
    let api_zero = need(
        port(need(unit(zero, "api"), "lane 0 api")?),
        "lane 0 api port",
    )?;
    let cases = [
        (lab.repo().to_owned(), "api", "api-ready", api_zero),
        (lab.repo().to_owned(), "1:api", "api-ready", api_one),
        (lab.repo().to_owned(), "feat/x:api", "api-ready", api_one),
        (feature.clone(), "api", "api-ready", api_one),
    ];
    for (cwd, reference, first, listening_port) in cases {
        let logs = lab.json(&cwd, &["logs", reference, "--tail", "20", "--json"])?;
        let texts: Vec<&str> = logs["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|line| line["text"].as_str())
            .collect();
        assert!(texts.contains(&first), "logs {reference}: {logs}");
        let expected = format!("LISTENING 127.0.0.1:{listening_port}");
        assert!(
            texts.iter().any(|t| *t == expected),
            "logs {reference} should come from the process on {listening_port}: {logs}"
        );
    }
    Ok(())
}

/// A second `start` leaves running units alone and reports them as already running.
#[test]
fn start_is_idempotent() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let first = lab.json(lab.repo(), &["start"])?;
    assert_eq!(first["ready"], true, "{first}");
    let before = lab.lanes(lab.repo())?;
    let pids = |lanes: &[Value]| -> Vec<Option<i32>> {
        lane(lanes, "fixture-two@0")
            .and_then(|l| l["units"].as_array())
            .into_iter()
            .flatten()
            .map(pid)
            .collect()
    };

    let again = lab.json(lab.repo(), &["start"])?;
    assert_eq!(again["ready"], true, "{again}");
    assert_eq!(again["started"], serde_json::json!([]), "{again}");
    assert_eq!(
        again["already_running"],
        serde_json::json!(["fixture-two@0:api", "fixture-two@0:web"]),
        "{again}"
    );
    assert_eq!(again["whole_lane"], false, "{again}");
    let after = lab.lanes(lab.repo())?;
    assert_eq!(
        pids(&before),
        pids(&after),
        "processes must not be replaced"
    );
    assert!(pids(&after).iter().all(Option::is_some));
    Ok(())
}

/// Stopping lane 1 from its worktree leaves lane 0's processes running and answering.
#[test]
fn stop_one_lane_leaves_the_other_running() -> Result<()> {
    let (lab, feature) = two_lanes()?;
    let before = lab.lanes(lab.repo())?;
    let one_ports: Vec<u16> = unit_ports(&before)
        .into_iter()
        .filter(|(lane, _, _)| lane == "fixture-two@1")
        .map(|(_, _, p)| p)
        .collect();

    lab.devlanes(&feature, &["stop"])?.expect_code(0)?;

    let after = lab.lanes(lab.repo())?;
    let zero_before = need(lane(&before, "fixture-two@0"), "lane 0 before")?;
    let zero = need(lane(&after, "fixture-two@0"), "lane 0")?;
    let one = need(lane(&after, "fixture-two@1"), "lane 1")?;
    assert_eq!(zero["state"], "running", "{zero}");
    assert_eq!(units_in(zero, "ready"), ["api", "web"]);
    assert_ne!(one["state"], "running", "{one}");
    assert!(units_in(one, "ready").is_empty(), "{one}");
    for name in ["api", "web"] {
        let old = need(unit(zero_before, name), name)?;
        let new = need(unit(zero, name), name)?;
        assert_eq!(pid(old), pid(new), "lane 0 {name} was restarted");
        let (status, _) = http_get(need(port(new), "port")?, "/healthz")?;
        assert_eq!(status, 200);
    }
    lab.eventually("lane 1 ports to close", Duration::from_secs(10), || {
        Ok(one_ports.iter().all(|p| !listening(*p)).then_some(()))
    })?;
    Ok(())
}

/// `lane clean` asks for --yes without a terminal, then stops the lane and frees its
/// port block.
#[test]
fn lane_clean_frees_the_lane_ports() -> Result<()> {
    let (lab, _feature) = two_lanes()?;
    let before = lab.lanes(lab.repo())?;
    let one = need(lane(&before, "fixture-two@1"), "lane 1")?;
    let block = one["port_block"].clone();
    assert!(block.is_array(), "{one}");
    let ports: Vec<u16> = unit_ports(&before)
        .into_iter()
        .filter(|(lane, _, _)| lane == "fixture-two@1")
        .map(|(_, _, p)| p)
        .collect();

    let refused = lab.devlanes(lab.repo(), &["lane", "clean", "1"])?;
    refused.expect_code(2)?;
    assert!(refused.stderr.contains("--yes"), "{refused}");

    let cleaned = lab.devlanes(lab.repo(), &["lane", "clean", "1", "--yes"]);
    let cleaned = cleaned?;
    cleaned.expect_code(0)?;
    lab.eventually("lane 1 ports to close", Duration::from_secs(10), || {
        Ok(ports.iter().all(|p| !listening(*p)).then_some(()))
    })?;
    let after = lab.lanes(lab.repo())?;
    if let Some(one) = lane(&after, "fixture-two@1") {
        assert!(units_in(one, "ready").is_empty(), "{one}");
        assert_ne!(
            one["port_block"], block,
            "lane 1 still holds its block: {one}"
        );
    }
    let zero = need(lane(&after, "fixture-two@0"), "lane 0")?;
    assert_eq!(units_in(zero, "ready"), ["api", "web"], "{zero}");
    Ok(())
}
