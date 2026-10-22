//! The runner against real processes: output, exit codes, stops and sweeps.
#![cfg(windows)]

mod command;

use std::{collections::BTreeMap, time::Duration};

use devlanes_core::{
    ExitSweep, LaunchSpec, OutputStream, StderrMode, StopPolicy, TerminalSize, Unit,
};
use devlanes_runner_windows::WindowsRunner;
use tokio::time::timeout;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The daemon's environment minus everything a unit does not need, so the tests
/// also show that units only get `spec.env`.
fn env() -> BTreeMap<String, String> {
    ["SystemRoot", "Path", "PATHEXT", "ComSpec", "TEMP"]
        .into_iter()
        .filter_map(|name| Some((name.to_owned(), std::env::var(name).ok()?)))
        .collect()
}

/// `cmd.exe /d /s /c <text>`, the shape resolve gives `run:` strings without Git.
fn cmd(text: &str) -> LaunchSpec {
    LaunchSpec {
        program: "cmd.exe".into(),
        args: vec!["/d".into(), "/s".into(), "/c".into(), text.into()],
        cwd: std::env::temp_dir(),
        env: env(),
        pty: false,
        size: TerminalSize::default(),
    }
}

/// Collects output until the stream closes, by stream.
async fn collect(
    mut output: tokio::sync::broadcast::Receiver<devlanes_core::OutputChunk>,
) -> BTreeMap<String, String> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    while let Ok(chunk) = output.recv().await {
        let key = match chunk.stream {
            OutputStream::Stdout => "stdout",
            OutputStream::Stderr => "stderr",
            OutputStream::Terminal => "terminal",
        };
        seen.entry(key.into())
            .or_default()
            .push_str(&String::from_utf8_lossy(&chunk.bytes));
    }
    seen
}

/// Output arrives per stream from launch on, and the exit code comes back.
#[tokio::test]
async fn output_and_exit_code() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let unit = WindowsRunner::default()
            .spawn_unit(cmd("echo out& echo err 1>&2& exit 7"))
            .await?;
        let output = tokio::spawn(collect(unit.output()));
        let report = unit.wait_report().await?;
        assert_eq!(report.outcome.code, Some(7));
        let seen = output.await?;
        assert_eq!(seen.get("stdout").map(|s| s.trim()), Some("out"));
        assert_eq!(seen.get("stderr").map(|s| s.trim()), Some("err"));
        Ok(())
    })
    .await?
}

/// Merged stderr keeps write order on one stream.
#[tokio::test]
async fn merged_stderr() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let unit = WindowsRunner::default()
            .with_stderr(StderrMode::Merged)
            .spawn_unit(cmd("echo one& echo two 1>&2"))
            .await?;
        let output = tokio::spawn(collect(unit.output()));
        unit.wait_report().await?;
        let seen = output.await?;
        let text: Vec<&str> = seen["stdout"].split_whitespace().collect();
        assert_eq!(text, ["one", "two"]);
        assert!(!seen.contains_key("stderr"));
        Ok(())
    })
    .await?
}

/// A stop ends the leader and the grandchild it started, which the job holds.
#[tokio::test]
async fn stop_ends_the_whole_tree() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let unit = WindowsRunner::default()
            .spawn_unit(cmd("ping -n 60 127.0.0.1 >NUL"))
            .await?;
        // Wait until the ping grandchild is tracked.
        let mut members = Vec::new();
        for _ in 0..100 {
            members = unit.processes().await?;
            if members.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(members.len() >= 2, "{members:?}");
        let report = unit.stop_with_report(StopPolicy::default()).await?;
        assert!(report.stop.is_some());
        assert_eq!(report.outcome.code, Some(1));
        assert!(report.escaped.is_empty(), "{report:?}");
        for member in members {
            assert!(
                !devlanes_sys::matches(&member.identity())?,
                "{member:?} survived"
            );
        }
        Ok(())
    })
    .await?
}

/// A missing program fails the spawn with `NotFound`, which the daemon's spawn
/// diagnostics turn into "not found on PATH".
#[tokio::test]
async fn missing_program_is_not_found() {
    let mut spec = cmd("");
    spec.program = "devlanes-no-such-program".into();
    spec.args.clear();
    match WindowsRunner::default().spawn_unit(spec).await {
        Err(devlanes_core::Error::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok(unit) => panic!("spawned {unit:?}"),
    }
}

/// After a task exits on its own, leftovers still inside its job are ended, even
/// in the mode that keeps daemons started outside the job.
#[tokio::test]
async fn task_leftovers_in_the_job_are_swept() -> TestResult {
    timeout(Duration::from_secs(30), async {
        // `start /b` keeps the ping in the job; it is a leftover to sweep.
        let unit = WindowsRunner::default()
            .with_exit_sweep(ExitSweep::KeepDaemonized)
            .spawn_unit(cmd(
                "start /b ping -n 60 127.0.0.1 >NUL& ping -n 2 127.0.0.1 >NUL",
            ))
            .await?;
        let report = unit.wait_report().await?;
        assert_eq!(report.outcome.code, Some(0));
        assert!(report.left_running.is_empty(), "{report:?}");
        assert!(report.escaped.is_empty(), "{report:?}");
        assert!(
            report
                .swept
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case("PING.EXE")),
            "{report:?}"
        );
        Ok(())
    })
    .await?
}
