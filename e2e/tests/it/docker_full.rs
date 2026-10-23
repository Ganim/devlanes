//! `devlanes with` against `fixtures/full`: compose, setup, migrate, services, seed.

use std::{fs, time::Duration};

use devlanes_e2e::{Lab, Output, Result, lane, unit};
use serde_json::Value;

use crate::need;

/// Compose project named in `fixtures/full/devlanes.yaml`.
const COMPOSE_PROJECT: &str = "devlanes-fixture-full";

/// Curls the API through the proxy, saves `status --json` and counts seeded rows.
const CHECK: &str = r#"set -e
url="$DEVLANES_PUBLIC_URL_API"
host="${url#https://}"
host="${host%%:*}"
curl -fsS --max-time 10 --cacert "$DEVLANES_CA_CERT" $E2E_CURL_FLAGS \
  --resolve "$host:$E2E_HTTPS_PORT:127.0.0.1" "$url/healthz"
devlanes status --json > "$E2E_STATUS"
docker compose -p devlanes-fixture-full exec -T postgres \
  psql -U postgres -d full_0 -tAc "SELECT count(*) FROM items" > "$E2E_ROWS""#;

/// Runs `devlanes with` with [`CHECK`] followed by `tail`.
fn with(lab: &Lab, name: &str, tail: &str) -> Result<(Output, Value, String)> {
    let status = lab.home().join(format!("{name}-status.json"));
    let rows = lab.home().join(format!("{name}-rows.txt"));
    let logs = lab.home().join(format!("{name}-logs"));
    let script = format!("{CHECK}\n{tail}");
    let mut command = lab.command(
        lab.repo(),
        &[
            "--timeout",
            "5m",
            "with",
            "--logs-dir",
            &logs.to_string_lossy(),
            "--",
            "sh",
            "-c",
            &script,
        ],
    )?;
    command
        .env("E2E_HTTPS_PORT", lab.https_port().to_string())
        .env("E2E_CURL_FLAGS", devlanes_e2e::CURL_FLAGS)
        .env("E2E_STATUS", &status)
        .env("E2E_ROWS", &rows);
    let output = lab.run(command, Duration::from_secs(420))?;
    let status = fs::read_to_string(&status)
        .map_err(|e| devlanes_e2e::Error::Unexpected(format!("no status ({e})\n{output}")))?;
    let rows = fs::read_to_string(&rows).unwrap_or_default();
    assert!(
        fs::read_dir(&logs).is_ok_and(|entries| entries.count() >= 2),
        "--logs-dir written\n{output}"
    );
    Ok((
        output,
        serde_json::from_str(&status)?,
        rows.trim().to_owned(),
    ))
}

/// Start time of a unit in a `status --json` document.
fn started(status: &Value, name: &str) -> Result<u64> {
    let lanes = status["items"].as_array().cloned().unwrap_or_default();
    let unit = need(
        lane(&lanes, "fixture-full@0").and_then(|l| unit(l, name)),
        &format!("{name} in status"),
    )?;
    unit["started_at_ms"]
        .as_u64()
        .ok_or_else(|| devlanes_e2e::Error::Unexpected(format!("{name} has no start time: {unit}")))
}

/// Plan 11.5a: exit 0 from a command that curls a service after infra, setup, migrate
/// and seed ran in order; then exit 3 is passed through and seed (`lane.create`) does not
/// run again.
#[test]
#[ignore = "needs docker"]
fn docker_with_full_fixture() -> Result<()> {
    let lab = Lab::docker("full", COMPOSE_PROJECT, Duration::from_secs(900))?;
    lab.write(".env", &format!("FULL_PG_PORT={}\n", lab.spare_port(0)))?;

    let (output, status, rows) = with(&lab, "ok", "")?;
    output.expect_code(0)?;
    assert_eq!(rows, "1", "setup, migrate and seed must have run\n{output}");
    let migrate = started(&status, "migrate")?;
    let api = started(&status, "api")?;
    let web = started(&status, "web")?;
    let seed = started(&status, "seed")?;
    assert!(
        migrate <= api && migrate <= web,
        "migrate before services: {status}"
    );
    assert!(seed >= api && seed >= web, "seed after services: {status}");

    let (output, _, rows) = with(&lab, "fail", "exit 3")?;
    output.expect_code(3)?;
    assert_eq!(rows, "1", "seed runs once per lane\n{output}");
    assert!(
        !lab.daemon_running(),
        "the auto-spawned daemon must be stopped"
    );
    Ok(())
}
