//! Spawn failures that name their cause, verbose diagnostics on stderr, and
//! secrets kept out of diagnostics, `daemon.log` and debug bundles.

use std::{fs, process::Command, time::Duration};

use devlanes_e2e::{Error, Lab, Result};

use crate::BUDGET;

/// A service whose program does not exist on the lane's PATH.
const MISSING: &str = "\
project: diag-missing
services:
  api:
    run: [devlanes-no-such-program, --flag]
";

/// A service that gets a secret from the shell through its environment.
const SECRET: &str = "\
project: diag-secret
services:
  api:
    run: [fixture-server, --log-line, api-ready]
    env:
      API_TOKEN: ${{ env.ACME_API_TOKEN }}
    ready: { http: { path: /healthz } }
";

/// The value the secret test hides; long and unusual, so a match is a leak.
const SECRET_VALUE: &str = "e2e-s3cret-value-4711-zz";

/// `start` names the missing program, the reason and the PATH searched, in the
/// failure and in the unit's log; `logs` shows the `[devlanes]` line.
#[test]
fn spawn_failure_names_the_program_and_reaches_the_log() -> Result<()> {
    let lab = Lab::with_config(MISSING, BUDGET)?;
    let output = lab.devlanes(lab.repo(), &["start", "--json"])?;
    output.expect_code(1)?;
    let result = output.json()?;
    let failure = &result["failures"][0];
    let data = &failure["reason"]["data"];
    assert_eq!(failure["reason"]["code"], "spawn_failed", "{result}");
    assert_eq!(data["program"], "devlanes-no-such-program", "{result}");
    assert_eq!(data["reason"], "not_found", "{result}");
    assert_eq!(data["errno"], "ENOENT", "{result}");
    assert!(
        data["path_searched"]
            .as_array()
            .is_some_and(|dirs| !dirs.is_empty()),
        "{result}"
    );
    let tail = failure["log_tail"][0].as_str().unwrap_or_default();
    assert!(
        tail.starts_with(
            "[devlanes] could not start: devlanes-no-such-program: command not found on PATH"
        ),
        "{result}"
    );
    let logs = lab.json(lab.repo(), &["logs", "api", "--json"])?;
    assert!(
        logs["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|line| line["text"] == tail)),
        "{logs}"
    );
    // Plain output words it and points at the lane's PATH.
    let plain = lab.devlanes(lab.repo(), &["start", "--output", "plain"])?;
    assert!(
        plain
            .stdout
            .contains("couldn't start: devlanes-no-such-program: command not found on PATH"),
        "{plain}"
    );
    assert!(
        plain
            .stdout
            .contains("devlanes env show api --lane diag-missing@0"),
        "{plain}"
    );
    Ok(())
}

/// `-vv` writes client and daemon diagnostics and a timing summary to stderr while
/// stdout stays NDJSON; neither they, `daemon.log` at trace level nor a debug
/// bundle of `env show` contain the value of a secret the service received.
#[test]
fn verbose_output_stays_on_stderr_and_hides_secrets() -> Result<()> {
    let lab = Lab::with_config(SECRET, BUDGET)?;
    let with_secret = |args: &[&str]| -> Result<Command> {
        let mut command = lab.command(lab.repo(), args)?;
        command
            .env("ACME_API_TOKEN", SECRET_VALUE)
            .env("DEVLANES_LOG", "trace");
        Ok(command)
    };
    let output = lab.run(
        with_secret(&["-vv", "start", "--json"])?,
        Duration::from_secs(120),
    )?;
    output.expect_code(0)?;
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).map_err(|error| {
            Error::Unexpected(format!(
                "stdout line is not JSON ({error}): {line}\n{output}"
            ))
        })?;
    }
    for expected in [
        "daemon +",
        "spawning",
        "env_keys=",
        "timings:",
        "request start",
    ] {
        assert!(
            output.stderr.contains(expected),
            "missing {expected:?}\n{output}"
        );
    }
    assert!(!output.stdout.contains("daemon +"), "{output}");
    assert!(!output.stderr.contains(SECRET_VALUE), "{output}");
    assert!(!output.stdout.contains(SECRET_VALUE), "{output}");

    // The service did get the value: the lane's environment shows it.
    let env = lab.run(
        with_secret(&["env", "show", "api", "--output", "plain"])?,
        Duration::from_secs(30),
    )?;
    assert!(env.stdout.contains(SECRET_VALUE), "{env}");

    let archive = lab.home().join("bundle.tar.gz");
    let archive_text = archive.to_string_lossy().into_owned();
    let bundle = lab.run(
        with_secret(&[
            "debug",
            "bundle",
            "--to",
            &archive_text,
            "--",
            "env",
            "show",
            "api",
            "--output",
            "plain",
        ])?,
        Duration::from_secs(120),
    )?;
    bundle.expect_code(0)?;
    let unpacked = lab.home().join("bundle");
    fs::create_dir_all(&unpacked)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpacked)
        .status()?;
    assert!(status.success(), "tar -x");
    let mut seen = Vec::new();
    for entry in fs::read_dir(&unpacked)? {
        let path = entry?.path();
        let text = fs::read_to_string(&path)?;
        assert!(
            !text.contains(SECRET_VALUE),
            "{} leaks the secret:\n{text}",
            path.display()
        );
        seen.push(
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            "README.txt",
            "config-check.json",
            "daemon.log",
            "doctor.json",
            "rerun.txt",
            "status.json",
            "versions.json"
        ]
    );
    let rerun = fs::read_to_string(unpacked.join("rerun.txt"))?;
    assert!(rerun.contains("API_TOKEN=[redacted]"), "{rerun}");
    assert!(
        rerun.contains("DEVLANES_LANE=0"),
        "other values stay: {rerun}"
    );
    assert!(rerun.contains("request env"), "the timing summary: {rerun}");

    lab.devlanes(lab.repo(), &["stop"])?.expect_code(0)?;
    let log = fs::read_to_string(lab.home().join("logs/daemon.log"))?;
    assert!(log.contains("spawning"), "the daemon logged at trace level");
    assert!(!log.contains(SECRET_VALUE), "daemon.log leaks the secret");
    Ok(())
}
