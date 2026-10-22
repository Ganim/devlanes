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
    // As the daemon does at startup: a test run from Git Bash inherits the
    // "ignore Ctrl-C" flag, and units would inherit it and never see Ctrl-C.
    devlanes_sys::windows::process::accept_ctrl_c()?;
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

/// This test binary running [`breakaway_leader`] (roles `stay` and `exit`) or
/// [`graceful_leader`] (the others) in `role`.
fn leader(role: &str) -> Result<LaunchSpec, std::io::Error> {
    let mut env = env();
    env.insert(ROLE.into(), role.into());
    let test = match role {
        "stay" | "exit" => "breakaway_leader",
        _ => "graceful_leader",
    };
    Ok(LaunchSpec {
        program: std::env::current_exe()?.to_string_lossy().into_owned(),
        args: ["--exact", test, "--nocapture", "--test-threads=1"]
            .map(String::from)
            .into(),
        cwd: std::env::temp_dir(),
        env,
        pty: false,
        size: TerminalSize::default(),
    })
}

/// Not a test of its own: as a unit, starts `ping` asking to leave the unit's
/// job (`CREATE_BREAKAWAY_FROM_JOB`) and, when refused, without asking, as tools
/// that handle the refusal do. Prints `breakaway <pid>` or `fallback <pid>`,
/// then waits (role `stay`) or exits (role `exit`). Without the role it does
/// nothing.
#[test]
fn breakaway_leader() -> std::io::Result<()> {
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let Ok(role) = std::env::var(ROLE) else {
        return Ok(());
    };
    let ping = || {
        let mut command = std::process::Command::new("ping");
        command
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null());
        command
    };
    match ping().creation_flags(CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(child) => println!("breakaway {}", child.id()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("fallback {}", ping().spawn()?.id());
        }
        Err(error) => return Err(error),
    }
    std::io::stdout().flush()?;
    // Long enough for a snapshot to see the child under this process.
    std::thread::sleep(Duration::from_millis(if role == "exit" {
        1500
    } else {
        30_000
    }));
    Ok(())
}

/// Not a test of its own: this binary as the console-control helper, when the
/// runner starts it with a request. Without one it does nothing.
#[test]
fn console_ctrl_helper() {
    if std::env::var_os(devlanes_runner_windows::ctrl::REQUEST_VAR).is_some() {
        std::process::exit(i32::from(devlanes_runner_windows::console_ctrl_main()));
    }
}

/// The runner with this binary as its console-control helper.
fn runner_with_helper() -> Result<WindowsRunner, std::io::Error> {
    Ok(
        WindowsRunner::default().with_console_helper(devlanes_runner_windows::ConsoleHelper {
            program: std::env::current_exe()?,
            args: [
                "--exact",
                "console_ctrl_helper",
                "--nocapture",
                "--test-threads=1",
            ]
            .map(Into::into)
            .into(),
        }),
    )
}

/// Not a test of its own: as a unit (role `graceful`), waits for Ctrl-C or
/// Ctrl-Break, prints which arrived and exits 0, the way a server stops cleanly.
/// Role `q9` starts one `ping` sharing its console and one with a console of its
/// own (`CREATE_NO_WINDOW`), prints `shared <pid>` and `own <pid>`, then exits
/// after a while. Without a role it does nothing.
#[test]
fn graceful_leader() -> std::io::Result<()> {
    match std::env::var(ROLE).as_deref() {
        Ok("graceful") => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
                let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
                println!("handlers ready");
                std::io::stdout().flush()?;
                let event = tokio::select! {
                    _ = ctrl_c.recv() => "c",
                    _ = ctrl_break.recv() => "break",
                };
                println!("graceful {event}");
                std::io::stdout().flush()?;
                std::process::exit(0);
            })
        }
        Ok("q9") => {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let ping = || {
                let mut command = std::process::Command::new("ping");
                command
                    .args(["-n", "60", "127.0.0.1"])
                    .stdout(std::process::Stdio::null());
                command
            };
            let shared = ping().spawn()?;
            let own = ping().creation_flags(CREATE_NO_WINDOW).spawn()?;
            println!("shared {}", shared.id());
            println!("own {}", own.id());
            std::io::stdout().flush()?;
            std::thread::sleep(Duration::from_millis(2500));
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The `shared` and `own` pings a `q9` leader reports, as processes.
async fn q9_pings(
    unit: &Guard,
) -> Result<[devlanes_sys::ProcessInfo; 2], Box<dyn std::error::Error>> {
    let mut output = unit.output();
    let mut seen = String::new();
    let mut pings = Vec::new();
    for name in ["shared ", "own "] {
        let pid: u32 = wait_for(&mut output, &mut seen, name)
            .await
            .and_then(|pid| pid.parse().ok())
            .ok_or_else(|| format!("no {name}line in {seen:?}"))?;
        pings.push(devlanes_sys::process(pid)?.ok_or_else(|| format!("{name}ping missing"))?);
    }
    pings.try_into().map_err(|_| "two pings".into())
}

/// Reads output until a line containing `marker` and returns the text after it.
async fn wait_for(
    output: &mut tokio::sync::broadcast::Receiver<devlanes_core::OutputChunk>,
    seen: &mut String,
    marker: &str,
) -> Option<String> {
    loop {
        if let Some(line) = seen.lines().find_map(|line| line.split_once(marker)) {
            return Some(line.1.trim().to_owned());
        }
        let chunk = output.recv().await.ok()?;
        seen.push_str(&String::from_utf8_lossy(&chunk.bytes));
    }
}

/// A pipe unit asked to stop gets Ctrl-Break through the helper and exits on its
/// own: no escalation, its own exit code.
#[tokio::test]
async fn pipe_units_stop_gracefully_on_ctrl_break() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let unit = spawn(runner_with_helper()?, leader("graceful")?).await?;
        let mut output = unit.output();
        let mut seen = String::new();
        wait_for(&mut output, &mut seen, "handlers ready")
            .await
            .ok_or("leader never got ready")?;
        let report = unit.stop_with_report(StopPolicy::default()).await?;
        assert_eq!(report.stop.map(|s| s.escalated), Some(false), "{report:?}");
        assert_eq!(report.outcome.code, Some(0), "{report:?}");
        wait_for(&mut output, &mut seen, "graceful ")
            .await
            .filter(|event| event == "break")
            .ok_or_else(|| format!("no graceful break in {seen:?}"))?;
        Ok(())
    })
    .await?
}

/// A `pty: true` unit stopped with SIGINT gets Ctrl-C as a key and exits on its own.
#[tokio::test]
async fn pty_units_stop_gracefully_on_ctrl_c() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let mut spec = leader("graceful")?;
        spec.pty = true;
        let unit = spawn(WindowsRunner::default(), spec).await?;
        let mut output = unit.output();
        let mut seen = String::new();
        wait_for(&mut output, &mut seen, "handlers ready")
            .await
            .ok_or("leader never got ready")?;
        let policy = StopPolicy {
            signal: devlanes_core::StopSignal::Interrupt,
            timeout: Duration::from_secs(10),
        };
        let report = unit.stop_with_report(policy).await?;
        assert_eq!(report.stop.map(|s| s.escalated), Some(false), "{report:?}");
        assert_eq!(report.outcome.code, Some(0), "{report:?}");
        Ok(())
    })
    .await?
}

/// A `.cmd` shim left asking "Terminate batch job (Y/N)?" after its program
/// stopped is ended without counting as escalation.
#[tokio::test]
async fn batch_shims_do_not_hold_a_stop() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir()?;
        let shim = dir.path().join("serve.cmd");
        std::fs::write(
            &shim,
            format!(
                "@\"{}\" --exact graceful_leader --nocapture --test-threads=1\r\n@echo after\r\n",
                std::env::current_exe()?.display()
            ),
        )?;
        let mut spec = leader("graceful")?;
        spec.program = shim.to_string_lossy().into_owned();
        spec.args.clear();
        spec.pty = true;
        let unit = spawn(WindowsRunner::default(), spec).await?;
        let mut output = unit.output();
        let mut seen = String::new();
        wait_for(&mut output, &mut seen, "handlers ready")
            .await
            .ok_or("leader never got ready")?;
        let policy = StopPolicy {
            signal: devlanes_core::StopSignal::Interrupt,
            timeout: Duration::from_secs(20),
        };
        let started = std::time::Instant::now();
        let report = unit.stop_with_report(policy).await?;
        assert_eq!(report.stop.map(|s| s.escalated), Some(false), "{report:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited for the timeout"
        );
        Ok(())
    })
    .await?
}

/// After a pipe task exits (Q9), a process still on its console is a leftover
/// and ends, while one with a console of its own is a daemon it started and keeps
/// running.
#[tokio::test]
async fn tasks_keep_processes_with_their_own_console() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let runner = runner_with_helper()?
            .with_track_interval(Duration::from_millis(100))
            .with_exit_sweep(ExitSweep::KeepDaemonized);
        let unit = spawn(runner, leader("q9")?).await?;
        let [shared, own] = q9_pings(&unit).await?;
        let _ends = (Ends(shared.identity()), Ends(own.identity()));
        let report = unit.wait_report().await?;
        assert!(
            report.swept.iter().any(|p| p.pid == shared.pid),
            "{report:?}"
        );
        assert!(
            report.left_running.iter().any(|p| p.pid == own.pid),
            "{report:?}"
        );
        assert!(devlanes_sys::matches(&own.identity())?, "kept daemon ended");
        assert!(
            !report
                .left_running
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case("conhost.exe")),
            "console hosts are not the task's daemons: {report:?}"
        );
        Ok(())
    })
    .await?
}

/// Git for Windows' `sh.exe`, from `git --exec-path`
/// (`<git>\mingw64\libexec\git-core`) as the daemon finds it.
fn git_sh() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .arg("--exec-path")
        .output()?;
    let exec = std::path::PathBuf::from(String::from_utf8(output.stdout)?.trim());
    let sh = exec
        .ancestors()
        .nth(3)
        .ok_or("unexpected git exec path")?
        .join(r"bin\sh.exe");
    Ok(sh)
}

/// A `run:` string's background command (`sleep &` in Git's `sh`, which breaks
/// its children out of the job) still on the task's console is a leftover and
/// ends with the task.
#[tokio::test]
async fn sh_leftovers_on_the_console_end_with_the_task() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let runner = runner_with_helper()?.with_exit_sweep(ExitSweep::KeepDaemonized);
        let spec = LaunchSpec {
            program: git_sh()?.to_string_lossy().into_owned(),
            args: vec!["-c".into(), "sleep 37 & echo \"sleep $!\"; sleep 1".into()],
            ..cmd("")
        };
        let unit = spawn(runner, spec).await?;
        let mut output = unit.output();
        let mut seen = String::new();
        wait_for(&mut output, &mut seen, "sleep ")
            .await
            .ok_or_else(|| format!("no sleep line in {seen:?}"))?;
        let report = unit.wait_report().await?;
        let sleeps = |list: &[devlanes_sys::ProcessInfo]| {
            list.iter()
                .any(|p| p.name.eq_ignore_ascii_case("sleep.exe"))
        };
        assert!(sleeps(&report.swept), "{report:?}");
        assert!(!sleeps(&report.left_running), "{report:?}");
        Ok(())
    })
    .await?
}

/// When the task's console cannot be listed (the helper cannot attach), nothing
/// is ended on a guess: every process the task left running is kept and
/// reported (decision Q9).
#[tokio::test]
async fn tasks_keep_everything_when_the_console_cannot_be_listed() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let failing = devlanes_runner_windows::ConsoleHelper {
            program: std::env::var_os("ComSpec")
                .unwrap_or_else(|| "cmd.exe".into())
                .into(),
            args: ["/d", "/c", "exit 7"].map(Into::into).into(),
        };
        let runner = WindowsRunner::default()
            .with_console_helper(failing)
            .with_exit_sweep(ExitSweep::KeepDaemonized);
        let unit = spawn(runner, leader("q9")?).await?;
        let pings = q9_pings(&unit).await?;
        let _ends = pings.each_ref().map(|ping| Ends(ping.identity()));
        let report = unit.wait_report().await?;
        for kept in &pings {
            assert!(
                report.left_running.iter().any(|p| p.pid == kept.pid),
                "{report:?}"
            );
        }
        assert!(report.swept.is_empty(), "{report:?}");
        Ok(())
    })
    .await?
}

/// Reads output until the `breakaway <pid>` or `fallback <pid>` line and returns
/// the word and the PID.
async fn breakaway_pid(
    mut output: tokio::sync::broadcast::Receiver<devlanes_core::OutputChunk>,
) -> Option<(&'static str, u32)> {
    let mut text = Vec::new();
    while let Ok(chunk) = output.recv().await {
        text.extend_from_slice(&chunk.bytes);
        // libtest prints `test breakaway_leader ... ` on the same line first.
        for line in text.lines().map_while(Result::ok) {
            for word in ["breakaway", "fallback"] {
                if let Some((_, pid)) = line.rsplit_once(&format!("{word} "))
                    && let Ok(pid) = pid.trim().parse()
                {
                    return Some((word, pid));
                }
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

/// Leaving the unit's job is refused; a tool that then starts its child
/// normally keeps it in the job, where a stop ends it.
#[tokio::test]
async fn breakaway_is_refused_and_a_stop_ends_the_fallback() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let runner = WindowsRunner::default().with_track_interval(Duration::from_millis(50));
        let unit = spawn(runner, leader("stay")?).await?;
        let (word, pid) = breakaway_pid(unit.output())
            .await
            .ok_or("no breakaway or fallback line")?;
        let info = devlanes_sys::process(pid)?.ok_or("ping missing")?;
        let _ends = Ends(info.identity());
        assert_eq!(word, "fallback", "breakaway was allowed");
        unit.stop_with_report(StopPolicy::default()).await?;
        assert!(
            !devlanes_sys::matches(&info.identity())?,
            "ping {pid} survived"
        );
        Ok(())
    })
    .await?
}

/// Without a helper to list the task's console, job membership decides what a
/// task keeps: its job's members end with it.
#[tokio::test]
async fn task_without_a_helper_ends_its_job_members() -> TestResult {
    timeout(Duration::from_secs(60), async {
        let runner = WindowsRunner::default()
            .with_track_interval(Duration::from_millis(50))
            .with_exit_sweep(ExitSweep::KeepDaemonized);
        let unit = spawn(runner, leader("exit")?).await?;
        let (_, pid) = breakaway_pid(unit.output())
            .await
            .ok_or("no breakaway or fallback line")?;
        let info = devlanes_sys::process(pid)?.ok_or("ping missing")?;
        let _ends = Ends(info.identity());
        let report = unit.wait_report().await?;
        assert_eq!(report.outcome.code, Some(0));
        assert!(report.swept.iter().any(|p| p.pid == pid), "{report:?}");
        assert!(
            !devlanes_sys::matches(&info.identity())?,
            "job member survived"
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
