//! `devlanes with -- <cmd>`: start, run, write logs, stop what it started, keep the
//! command's exit code.

use std::{fs, path::Path, time::Duration};

use devlanes_e2e::{Lab, Output, Result, lane, listening};

use crate::{BUDGET, need, units_in};

/// Curls the API's public URL through the proxy with the CA that `with` exports. The URL
/// must carry the lab's proxy port, which is not 443.
const CURL_API: &str = r#"url="$DEVLANES_PUBLIC_URL_API"
case "$url" in *":$E2E_HTTPS_PORT") ;; *) echo "public URL without the proxy port: $url" >&2; exit 90 ;; esac
host="${url#https://}"
host="${host%%:*}"
curl -fsS --max-time 10 --cacert "$DEVLANES_CA_CERT" \
  --resolve "$host:$E2E_HTTPS_PORT:127.0.0.1" "$url/healthz""#;

/// Runs `devlanes with --logs-dir <dir> -- sh -c <script>` in the primary worktree.
fn with(lab: &Lab, logs: &Path, script: &str) -> Result<Output> {
    let mut command = lab.command(
        lab.repo(),
        &[
            "with",
            "--logs-dir",
            &logs.to_string_lossy(),
            "--",
            "sh",
            "-c",
            script,
        ],
    )?;
    command.env("E2E_HTTPS_PORT", lab.https_port().to_string());
    lab.run(command, Duration::from_secs(90))
}

/// Checks the log artifacts hold each unit's lines of this run and returns the ports
/// the services printed (`LISTENING 127.0.0.1:<port>`).
fn assert_logs(logs: &Path) -> Result<Vec<u16>> {
    let mut texts = Vec::new();
    for entry in fs::read_dir(logs)? {
        texts.push(fs::read_to_string(entry?.path())?);
    }
    assert_eq!(texts.len(), 2, "one file per unit in {}", logs.display());
    for line in ["api-ready", "web-ready"] {
        assert!(
            texts.iter().any(|text| text.contains(line)),
            "no log file contains {line}: {texts:?}"
        );
    }
    let ports: Vec<u16> = texts
        .iter()
        .flat_map(|text| text.lines())
        .filter_map(|line| {
            line.strip_prefix("LISTENING 127.0.0.1:")?
                .trim()
                .parse()
                .ok()
        })
        .collect();
    assert_eq!(ports.len(), 2, "{texts:?}");
    Ok(ports)
}

/// Without a daemon: exit 0 from a command that curls the API, logs written, and the
/// auto-spawned daemon stopped afterwards.
#[test]
fn with_runs_the_command_and_stops_the_daemon_it_spawned() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let logs = lab.home().join("artifacts-ok");
    let output = with(&lab, &logs, CURL_API)?;
    output.expect_code(0)?;
    assert!(output.stdout.contains("\"path\":\"/healthz\""), "{output}");
    let ports = assert_logs(&logs)?;
    lab.eventually(
        "the auto-spawned daemon to stop",
        Duration::from_secs(15),
        || Ok((!lab.daemon_running()).then_some(())),
    )?;
    for port in ports {
        assert!(!listening(port), "port {port} still listening: {output}");
    }
    Ok(())
}

/// With a running daemon: the command's exit code 3 is kept, logs are written, the lane
/// is stopped and the daemon `with` did not spawn keeps running.
#[test]
fn with_keeps_the_exit_code_and_stops_the_lane() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    lab.json(lab.repo(), &["daemon", "status", "--json"])?;
    let logs = lab.home().join("artifacts-fail");
    let output = with(&lab, &logs, &format!("{CURL_API} && exit 3"))?;
    output.expect_code(3)?;
    let ports = assert_logs(&logs)?;
    assert!(
        lab.daemon_running(),
        "with must not stop a daemon it did not spawn"
    );
    let lanes = lab.lanes(lab.repo())?;
    let zero = need(lane(&lanes, "fixture-two@0"), "lane 0")?;
    assert!(
        units_in(zero, "ready").is_empty(),
        "lane still running: {zero}"
    );
    assert_ne!(zero["state"], "running", "{zero}");
    for port in ports {
        assert!(!listening(port), "port {port} still listening");
    }
    Ok(())
}
