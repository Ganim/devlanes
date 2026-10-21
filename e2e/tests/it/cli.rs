//! Ad-hoc units, config errors and agent output wording.

use std::time::Duration;

use devlanes_e2e::{Lab, Result, alive, fixture_yaml, lane, pid, unit};

use crate::{BUDGET, need};

/// `exec --name` adopts a command as a unit; `logs` reads it without attaching and
/// `stop` ends the process.
#[test]
fn exec_unit_logs_and_stop() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let exec = lab.json(
        lab.repo(),
        &[
            "exec",
            "--name",
            "demo",
            "--",
            "fixture-server",
            "--port",
            "0",
            "--log-line",
            "demo-hello",
        ],
    )?;
    assert_eq!(exec["name"], "demo", "{exec}");
    assert_eq!(exec["kind"], "exec", "{exec}");
    let process = need(pid(&exec), "exec pid")?;

    let logs = lab.eventually("demo output", Duration::from_secs(15), || {
        let logs = lab.json(lab.repo(), &["logs", "demo", "--tail", "5", "--json"])?;
        let found = logs["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|line| line["text"] == "demo-hello"));
        Ok(found.then_some(logs))
    })?;
    assert!(
        logs["items"][0]["unit"] == "demo",
        "lines carry the unit name: {logs}"
    );
    let lanes = lab.lanes(lab.repo())?;
    let demo = need(
        lane(&lanes, "fixture-two@0").and_then(|l| unit(l, "demo")),
        "demo in status",
    )?;
    assert_eq!(demo["kind"], "exec");
    assert_eq!(pid(demo), Some(process));

    lab.devlanes(lab.repo(), &["stop", "demo"])?
        .expect_code(0)?;
    lab.eventually("demo to exit", Duration::from_secs(15), || {
        Ok((!alive(process)).then_some(()))
    })?;
    Ok(())
}

/// A broken `devlanes.yaml` fails `config check` and `start` with exit 1 and a
/// `devlanes.yaml:<line>:<column>` location.
#[test]
fn config_errors_point_at_the_line() -> Result<()> {
    let cases = [
        // Line 5: a misspelt service key.
        (
            "project: broken\nservices:\n  api:\n    run: [fixture-server]\n    raedy: { http: { path: /healthz } }\n",
            "devlanes.yaml:5:",
        ),
        // Line 2: a misspelt top-level key.
        (
            "project: broken\nservces:\n  api:\n    run: [fixture-server]\n",
            "devlanes.yaml:2:",
        ),
        // Line 4: a port that is not a number.
        (
            "project: broken\nservices:\n  api:\n    env: { PORT: [1] }\n    run: [fixture-server]\n",
            "devlanes.yaml:4:",
        ),
    ];
    for (yaml, location) in cases {
        let lab = Lab::with_config(yaml, BUDGET)?;
        for args in [&["config", "check"][..], &["start"][..]] {
            let mut command = lab.command(lab.repo(), args)?;
            command.env("DEVLANES_OUTPUT", "plain");
            let output = lab.run(command, devlanes_e2e::COMMAND_TIMEOUT)?;
            output.expect_code(1)?;
            assert!(
                output.stderr.contains(location),
                "expected {location} in stderr\n{output}"
            );
            assert!(output.stderr.starts_with("error: "), "{output}");
        }
    }
    Ok(())
}

/// In agent mode every status line starts with a stable lowercase word: `ready`,
/// `failed`, `stopped`, `running`, `idle`.
#[test]
fn agent_output_starts_with_stable_words() -> Result<()> {
    let yaml = format!(
        // The ready check keeps it from counting as ready before it exits.
        "{}  crash:\n    run: [sh, -c, 'echo boom-line; exit 7']\n    autostart: false\n    ready: {{ http: {{ path: /healthz }} }}\n",
        fixture_yaml("two-services")?
    );
    let lab = Lab::with_config(&yaml, BUDGET)?;
    let agent = |args: &[&str]| -> Result<devlanes_e2e::Output> {
        let mut command = lab.command(lab.repo(), args)?;
        command.env("DEVLANES_OUTPUT", "agent");
        lab.run(command, devlanes_e2e::COMMAND_TIMEOUT)
    };
    // The first line of a start summarises the lane (`fixture-two@0 (main): 2/2 ready`).
    let body = |stdout: &str| -> Vec<String> {
        stdout
            .lines()
            .skip(1)
            .filter(|l| !l.starts_with(' ') && !l.starts_with("note:"))
            .map(str::to_owned)
            .collect()
    };

    let started = agent(&["start"])?;
    started.expect_code(0)?;
    let lines = body(&started.stdout);
    assert_eq!(lines.len(), 2, "{started}");
    assert!(lines.iter().all(|l| l.starts_with("ready ")), "{started}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("https://api.fixture-two.localhost"))
    );

    let failed = agent(&["--timeout", "30s", "start", "crash"])?;
    failed.expect_code(1)?;
    let first = body(&failed.stdout);
    assert!(
        first.first().is_some_and(|l| l.starts_with("failed crash")),
        "failures come first: {failed}"
    );
    assert!(failed.stdout.contains("boom-line"), "log tail: {failed}");
    assert!(
        failed.stdout.contains("devlanes logs"),
        "next command: {failed}"
    );

    let status = agent(&["status"])?;
    let words: Vec<&str> = status
        .stdout
        .lines()
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(words.first(), Some(&"failed"), "failures first: {status}");
    assert!(
        words
            .iter()
            .all(|w| ["failed", "ready", "running"].contains(w)),
        "{status}"
    );

    let stopped = agent(&["stop"])?;
    stopped.expect_code(0)?;
    let lines: Vec<&str> = stopped
        .stdout
        .lines()
        .filter(|l| !l.starts_with("note:"))
        .collect();
    assert!(!lines.is_empty(), "{stopped}");
    assert!(lines.iter().all(|l| l.starts_with("stopped ")), "{stopped}");

    // The crashed unit keeps the lane `failed`; the others now read `stopped`.
    let status = agent(&["status"])?;
    let words: Vec<&str> = status
        .stdout
        .lines()
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(!words.is_empty(), "{status}");
    assert!(
        words
            .iter()
            .all(|w| ["failed", "stopped", "idle"].contains(w)),
        "{status}"
    );
    assert!(!words.contains(&"ready"), "{status}");
    Ok(())
}
