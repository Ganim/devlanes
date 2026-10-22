//! The runner against real processes: output, exit codes, stops and sweeps.
#![cfg(windows)]

mod command;

use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    ops::Deref,
    os::windows::process::CommandExt,
    sync::Arc,
    time::Duration,
};

use devlanes_core::{
    ExitSweep, LaunchSpec, OutputStream, ProcessIdentity, StderrMode, StopPolicy, TerminalSize,
    Unit,
};
use devlanes_runner_windows::{WindowsRunner, WindowsUnit};
use tokio::time::timeout;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Selects what [`breakaway_leader`] does when this binary runs as a unit.
const ROLE: &str = "DEVLANES_RUNNER_TEST_ROLE";

/// A spawned unit whose job is ended when the test is done, passed or failed.
struct Guard(Arc<WindowsUnit>);

impl Deref for Guard {
    type Target = WindowsUnit;

    fn deref(&self) -> &WindowsUnit {
        &self.0
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.kill_job();
    }
}

/// A process the test ends itself, by identity, when it is done.
struct Ends(ProcessIdentity);

impl Drop for Ends {
    fn drop(&mut self) {
        let _ = devlanes_sys::windows::proc::terminate_verified(&self.0, 1);
    }
}

/// Spawns `spec` with `runner` under a cleanup guard.
async fn spawn(runner: WindowsRunner, spec: LaunchSpec) -> Result<Guard, devlanes_core::Error> {
    Ok(Guard(runner.spawn_unit(spec).await?))
}

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

/// This test binary running [`breakaway_leader`] in `role`.
fn leader(role: &str) -> Result<LaunchSpec, std::io::Error> {
    let mut env = env();
    env.insert(ROLE.into(), role.into());
    Ok(LaunchSpec {
        program: std::env::current_exe()?.to_string_lossy().into_owned(),
        args: [
            "--exact",
            "breakaway_leader",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(String::from)
        .into(),
        cwd: std::env::temp_dir(),
        env,
        pty: false,
        size: TerminalSize::default(),
    })
}

/// Not a test of its own: as a unit, starts `ping` outside the unit's job with
/// `CREATE_BREAKAWAY_FROM_JOB`, prints `breakaway <pid>`, then waits (role
/// `stay`) or exits (role `exit`). Without the role it does nothing.
#[test]
fn breakaway_leader() -> std::io::Result<()> {
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let Ok(role) = std::env::var(ROLE) else {
        return Ok(());
    };
    let child = std::process::Command::new("ping")
        .args(["-n", "60", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .spawn()?;
    println!("breakaway {}", child.id());
    std::io::stdout().flush()?;
    // Long enough for a snapshot to see the child under this process.
    std::thread::sleep(Duration::from_millis(if role == "exit" {
        1500
    } else {
        30_000
    }));
    Ok(())
}

/// Reads output until the `breakaway <pid>` line and returns the PID.
async fn breakaway_pid(
    mut output: tokio::sync::broadcast::Receiver<devlanes_core::OutputChunk>,
) -> Option<u32> {
    let mut text = Vec::new();
    while let Ok(chunk) = output.recv().await {
        text.extend_from_slice(&chunk.bytes);
        // libtest prints `test breakaway_leader ... ` on the same line first.
        for line in text.lines().map_while(Result::ok) {
            if let Some((_, pid)) = line.rsplit_once("breakaway ")
                && let Ok(pid) = pid.trim().parse()
            {
                return Some(pid);
            }
        }
    }
    None
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
        let unit = spawn(
            WindowsRunner::default(),
            cmd("echo out& echo err 1>&2& exit 7"),
        )
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

/// A first subscriber that arrives after the unit finished still reads its
/// output, then sees the end.
#[tokio::test]
async fn late_first_subscriber_sees_everything() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let unit = spawn(WindowsRunner::default(), cmd("echo early")).await?;
        unit.wait_report().await?;
        let seen = collect(unit.output()).await;
        assert_eq!(seen.get("stdout").map(|s| s.trim()), Some("early"));
        Ok(())
    })
    .await?
}

/// Merged stderr keeps write order on one stream.
#[tokio::test]
async fn merged_stderr() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let unit = spawn(
            WindowsRunner::default().with_stderr(StderrMode::Merged),
            cmd("echo one& echo two 1>&2"),
        )
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
        let unit = spawn(WindowsRunner::default(), cmd("ping -n 60 127.0.0.1 >NUL")).await?;
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

/// A process that broke away from the job is still tracked by parent, and a stop
/// ends it too.
#[tokio::test]
async fn stop_reaches_breakaway_descendants() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let runner = WindowsRunner::default().with_track_interval(Duration::from_millis(50));
        let unit = spawn(runner, leader("stay")?).await?;
        let pid = breakaway_pid(unit.output())
            .await
            .ok_or("no breakaway line")?;
        let info = devlanes_sys::process(pid)?.ok_or("breakaway process missing")?;
        let _ends = Ends(info.identity());
        let mut tracked = false;
        for _ in 0..100 {
            if unit.processes().await?.iter().any(|p| p.pid == pid) {
                tracked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(tracked, "breakaway {pid} not tracked");
        unit.stop_with_report(StopPolicy::default()).await?;
        assert!(
            !devlanes_sys::matches(&info.identity())?,
            "breakaway {pid} survived"
        );
        Ok(())
    })
    .await?
}

/// After a task exits on its own, a process it started outside its job keeps
/// running and is reported as left running.
#[tokio::test]
async fn task_keeps_processes_outside_its_job() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let runner = WindowsRunner::default()
            .with_track_interval(Duration::from_millis(50))
            .with_exit_sweep(ExitSweep::KeepDaemonized);
        let unit = spawn(runner, leader("exit")?).await?;
        let pid = breakaway_pid(unit.output())
            .await
            .ok_or("no breakaway line")?;
        let info = devlanes_sys::process(pid)?.ok_or("breakaway process missing")?;
        let _ends = Ends(info.identity());
        let report = unit.wait_report().await?;
        assert_eq!(report.outcome.code, Some(0));
        assert!(
            report.left_running.iter().any(|p| p.pid == pid),
            "{report:?}"
        );
        assert!(
            devlanes_sys::matches(&info.identity())?,
            "kept process ended"
        );
        Ok(())
    })
    .await?
}

/// A `pty: true` unit runs in a pseudo console: its output is one terminal
/// stream, typed input reaches it, it can be resized, and its output ends once
/// it exits.
#[tokio::test]
async fn pty_units_read_input_and_resize() -> TestResult {
    timeout(Duration::from_secs(30), async {
        // Delayed expansion (`!NAME!`), since `%NAME%` expands before `set /p` runs.
        let mut spec = cmd("set /p NAME=name? & echo hello !NAME!");
        spec.args.insert(2, "/v:on".into());
        spec.pty = true;
        let unit = spawn(WindowsRunner::default(), spec).await?;
        let output = tokio::spawn(collect(unit.output()));
        unit.resize(TerminalSize {
            columns: 100,
            rows: 30,
        })
        .await?;
        unit.input(bytes::Bytes::from_static(b"acme\r")).await?;
        let report = unit.wait_report().await?;
        assert_eq!(report.outcome.code, Some(0));
        let seen = output.await?;
        assert!(!seen.contains_key("stdout"), "{seen:?}");
        let terminal = seen.get("terminal").ok_or("no terminal output")?;
        assert!(terminal.contains("hello acme"), "{terminal:?}");
        assert!(matches!(
            unit.input(bytes::Bytes::from_static(b"late")).await,
            Err(devlanes_core::Error::Exited)
        ));
        Ok(())
    })
    .await?
}

/// A pipe unit has no terminal to type into or resize.
#[tokio::test]
async fn pipe_units_have_no_terminal() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let unit = spawn(WindowsRunner::default(), cmd("ping -n 30 127.0.0.1 >NUL")).await?;
        assert!(matches!(
            unit.input(bytes::Bytes::from_static(b"x")).await,
            Err(devlanes_core::Error::Unsupported(_))
        ));
        unit.stop_with_report(StopPolicy::default()).await?;
        Ok(())
    })
    .await?
}

/// Stopping a `pty: true` unit ends it and its output.
#[tokio::test]
async fn pty_units_stop() -> TestResult {
    timeout(Duration::from_secs(30), async {
        let mut spec = cmd("ping -n 60 127.0.0.1");
        spec.pty = true;
        let unit = spawn(WindowsRunner::default(), spec).await?;
        let output = tokio::spawn(collect(unit.output()));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let report = unit.stop_with_report(StopPolicy::default()).await?;
        assert!(report.stop.is_some());
        assert!(report.escaped.is_empty(), "{report:?}");
        // Output ends rather than hanging on the closed console.
        output.await?;
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
        Ok(unit) => {
            unit.kill_job();
            panic!("spawned {unit:?}");
        }
    }
}

/// After a task exits on its own, leftovers still inside its job are ended, even
/// in the mode that keeps daemons started outside the job.
#[tokio::test]
async fn task_leftovers_in_the_job_are_swept() -> TestResult {
    timeout(Duration::from_secs(30), async {
        // `start /b` keeps the ping in the job; it is a leftover to sweep.
        let unit = spawn(
            WindowsRunner::default().with_exit_sweep(ExitSweep::KeepDaemonized),
            cmd("start /b ping -n 60 127.0.0.1 >NUL& ping -n 2 127.0.0.1 >NUL"),
        )
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
