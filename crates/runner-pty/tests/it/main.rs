//! Process-lifecycle tests with standard tools. Every test is time-bounded and kills
//! what it spawned, including escaped descendants, through [`Cleanup`].

// The runner is Unix only; the crate is empty elsewhere.
#![cfg(unix)]

use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use devlanes_core::{
    Error, LaunchSpec, OutputChunk, OutputStream, ProcessIdentity, StopPolicy, StopSignal,
    TerminalSize, Unit,
};
use devlanes_runner_pty::{ExitSweep, PtyRunner, PtyUnit, StderrMode};
use devlanes_sys::Signal;
use tokio::sync::broadcast::{self, error::RecvError};

/// Upper bound for any single test body.
const LIMIT: Duration = Duration::from_secs(20);
/// Marks the self-reexec helper process; see [`helper_setsid_sleeper`].
const HELPER_ENV: &str = "DEVLANES_RUNNER_PTY_HELPER";

/// Fails the test instead of hanging when a lifecycle step never completes.
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(LIMIT, future)
        .await
        .expect("test exceeded its time limit")
}

/// SIGKILLs everything a test started on drop: each unit's session (also after its
/// leader was reaped), its tracked set, registered extra PIDs and their
/// descendants. Every signal is verified by start time.
#[derive(Default)]
struct Cleanup {
    units: Vec<Arc<PtyUnit>>,
    extra: Vec<ProcessIdentity>,
}

impl Cleanup {
    /// Tracks a unit; its session and tracked set are swept at drop time.
    fn unit(&mut self, unit: &Arc<PtyUnit>) {
        self.units.push(Arc::clone(unit));
    }

    /// Tracks a process outside the unit's session.
    fn pid(&mut self, pid: u32) {
        if let Ok(Some(identity)) = devlanes_sys::identity(pid) {
            self.extra.push(identity);
        }
    }

    /// Live processes to kill in `snapshot`: verified known processes, the sessions
    /// they anchor, and descendants of either (by parent PID and start order).
    fn targets(&self, snapshot: &[devlanes_sys::ProcessInfo]) -> Vec<ProcessIdentity> {
        let mut known = self.extra.clone();
        for unit in &self.units {
            known.push(unit.identity());
            known.extend(unit.tracked());
            if let Some(report) = unit.exit_report() {
                known.extend(
                    report
                        .escaped
                        .iter()
                        .map(devlanes_sys::ProcessInfo::identity),
                );
                known.extend(
                    report
                        .left_running
                        .iter()
                        .map(devlanes_sys::ProcessInfo::identity),
                );
            }
        }
        let mut found: BTreeMap<u32, devlanes_sys::ProcessInfo> = BTreeMap::new();
        for process in snapshot {
            if known
                .iter()
                .any(|k| k.pid == process.pid && k.start_time == process.start_time)
            {
                found.insert(process.pid, process.clone());
            }
        }
        for unit in &self.units {
            for process in devlanes_sys::session_members_known(snapshot, &unit.identity(), &known) {
                found.insert(process.pid, process);
            }
        }
        loop {
            let before = found.len();
            for process in snapshot {
                let parent = found.get(&process.ppid);
                if parent.is_some_and(|p| process.start_time >= p.start_time) {
                    found.entry(process.pid).or_insert_with(|| process.clone());
                }
            }
            if found.len() == before {
                break;
            }
        }
        found
            .values()
            .filter(|p| !p.zombie && p.pid != std::process::id())
            .map(devlanes_sys::ProcessInfo::identity)
            .collect()
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // Repeat briefly: a dying parent can fork once more between scan and kill.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let snapshot = devlanes_sys::processes().unwrap_or_default();
            let targets = self.targets(&snapshot);
            if targets.is_empty() {
                return;
            }
            for identity in &targets {
                let _ = devlanes_sys::signal_verified(identity, Signal::KILL);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A spec with a minimal, explicit environment.
fn spec(program: &str, args: &[&str], pty: bool) -> LaunchSpec {
    LaunchSpec {
        program: program.into(),
        args: args.iter().map(|&a| a.to_owned()).collect(),
        cwd: std::env::temp_dir(),
        env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        pty,
        size: TerminalSize::default(),
    }
}

/// `/bin/sh -c script`.
fn shell(script: &str, pty: bool) -> LaunchSpec {
    spec("/bin/sh", &["-c", script], pty)
}

/// Collects output until the channel closes, i.e. the unit finished and readers hit EOF.
async fn collect(mut output: broadcast::Receiver<OutputChunk>) -> Vec<OutputChunk> {
    let mut chunks = Vec::new();
    loop {
        match output.recv().await {
            Ok(chunk) => chunks.push(chunk),
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return chunks,
        }
    }
}

/// Concatenates chunks of one stream as lossy text.
fn text(chunks: &[OutputChunk], stream: OutputStream) -> String {
    let bytes: Vec<u8> = chunks
        .iter()
        .filter(|c| c.stream == stream)
        .flat_map(|c| c.bytes.iter().copied())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Reads output until `needle` appears, returning everything read so far.
async fn read_until(output: &mut broadcast::Receiver<OutputChunk>, needle: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(needle) {
        match output.recv().await {
            Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk.bytes)),
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => panic!("output closed before {needle:?}; saw {seen:?}"),
        }
    }
    seen
}

/// Parses the first decimal number after `prefix`.
fn number_after(haystack: &str, prefix: &str) -> u32 {
    let start = haystack.find(prefix).expect("prefix present") + prefix.len();
    haystack[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("number after prefix")
}

/// Reports whether a PID still names a live, non-zombie process.
fn alive(pid: u32) -> bool {
    devlanes_sys::process(pid)
        .ok()
        .flatten()
        .is_some_and(|p| !p.zombie)
}

/// Polls an async condition every 20 ms until it holds.
async fn eventually<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    while !condition().await {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Short track interval so descendant tests do not wait for the 500 ms default.
fn runner() -> PtyRunner {
    PtyRunner::default().with_track_interval(Duration::from_millis(50))
}

/// Entry point re-executed by tests needing a process that calls `setsid` itself.
/// Ignored in normal runs and inert without the marker variable.
#[test]
#[ignore = "helper process entry point, started by setsid_grandchild_killed_via_descendants"]
fn helper_setsid_sleeper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    rustix::process::setsid().expect("setsid");
    println!("SETSID pid={}", std::process::id());
    std::thread::sleep(Duration::from_secs(30));
}

/// PTY output arrives, stdio is a terminal and the first subscriber misses nothing.
#[tokio::test]
async fn pty_echo_is_lossless_for_first_subscriber() -> Result<(), Error> {
    bounded(async {
        let unit = runner()
            .spawn_unit(shell("test -t 0 && test -t 1 && printf tty-ok", true))
            .await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        // Subscribing late still sees output written right after launch.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let chunks = collect(unit.output()).await;
        assert_eq!(text(&chunks, OutputStream::Terminal), "tty-ok");
        let exit = unit.wait().await?;
        assert_eq!((exit.code, exit.signal), (Some(0), None));
        // Subscribers arriving after exit are closed immediately.
        assert!(collect(unit.output()).await.is_empty());
        Ok(())
    })
    .await
}

/// Exit codes and terminating signals are reported for both runners.
#[tokio::test]
async fn exit_codes_and_signals() -> Result<(), Error> {
    bounded(async {
        for pty in [true, false] {
            let unit = runner().spawn_unit(shell("exit 7", pty)).await?;
            let exit = unit.wait().await?;
            assert_eq!((exit.code, exit.signal), (Some(7), None), "pty={pty}");
            let unit = runner().spawn_unit(shell("kill -9 $$", pty)).await?;
            let exit = unit.wait().await?;
            assert_eq!((exit.code, exit.signal), (None, Some(9)), "pty={pty}");
        }
        Ok(())
    })
    .await
}

/// The leader leads its own session and the environment is exactly the spec's.
#[tokio::test]
async fn session_leader_with_explicit_env() -> Result<(), Error> {
    bounded(async {
        let mut launch = shell("env; sleep 5", true);
        launch.env.insert("DL_MARK".into(), "present".into());
        let unit = runner().spawn_unit(launch).await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let identity = unit.identity();
        assert_eq!(identity.sid, identity.pid);
        let info = devlanes_sys::process(identity.pid)?.expect("leader visible");
        assert_eq!(
            (info.sid, info.pgid),
            (Some(identity.pid), Some(identity.pid))
        );
        assert_eq!(info.start_time, identity.start_time);

        let mut output = unit.output();
        let seen = read_until(&mut output, "DL_MARK=present").await;
        if std::env::var_os("HOME").is_some() {
            assert!(!seen.contains("HOME="), "daemon environment leaked: {seen}");
        }
        unit.stop(StopPolicy::default()).await?;
        Ok(())
    })
    .await
}

/// Pipe units get /dev/null stdin and keep or merge stderr as configured.
#[tokio::test]
async fn pipe_streams() -> Result<(), Error> {
    bounded(async {
        let script = "cat; echo out; echo err >&2";
        let unit = runner().spawn_unit(shell(script, false)).await?;
        let chunks = collect(unit.output()).await;
        assert_eq!(text(&chunks, OutputStream::Stdout), "out\n");
        assert_eq!(text(&chunks, OutputStream::Stderr), "err\n");
        assert_eq!(unit.wait().await?.code, Some(0));
        assert!(matches!(
            unit.input(Bytes::from_static(b"x")).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            unit.resize(TerminalSize::default()).await,
            Err(Error::Unsupported(_))
        ));

        let merged = runner().with_stderr(StderrMode::Merged);
        let unit = merged.spawn_unit(shell(script, false)).await?;
        let chunks = collect(unit.output()).await;
        assert_eq!(text(&chunks, OutputStream::Stdout), "out\nerr\n");
        assert_eq!(text(&chunks, OutputStream::Stderr), "");
        Ok(())
    })
    .await
}

/// The default size is 120x40; resize and input reach an interactive shell.
#[tokio::test]
async fn resize_and_input() -> Result<(), Error> {
    bounded(async {
        let unit = runner().spawn_unit(spec("/bin/sh", &[], true)).await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let mut output = unit.output();
        assert_eq!(unit.size()?, TerminalSize::default());
        unit.input(Bytes::from_static(b"stty size\n")).await?;
        read_until(&mut output, "40 120").await;
        unit.resize(TerminalSize {
            columns: 100,
            rows: 50,
        })
        .await?;
        unit.input(Bytes::from_static(b"stty size; exit 3\n"))
            .await?;
        read_until(&mut output, "50 100").await;
        assert_eq!(unit.wait().await?.code, Some(3));
        assert!(matches!(
            unit.input(Bytes::from_static(b"late")).await,
            Err(Error::Exited)
        ));
        Ok(())
    })
    .await
}

/// A unit ignoring SIGTERM is killed after the timeout.
#[tokio::test]
async fn sigterm_ignoring_unit_escalates_to_sigkill() -> Result<(), Error> {
    bounded(async {
        let script = "trap '' TERM; echo ready; while :; do sleep 1; done";
        let unit = runner().spawn_unit(shell(script, true)).await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let mut output = unit.output();
        read_until(&mut output, "ready").await;
        let started = Instant::now();
        let report = unit
            .stop_with_report(StopPolicy {
                signal: StopSignal::Terminate,
                timeout: Duration::from_millis(300),
            })
            .await?;
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(report.outcome.signal, Some(9));
        let stop = report.stop.expect("stop recorded");
        assert_eq!((stop.signal, stop.escalated), (15, true));
        assert!(report.escaped.is_empty(), "{:?}", report.escaped);
        assert!(unit.processes().await?.is_empty());
        Ok(())
    })
    .await
}

/// Grandchildren in the session, including ones in another process group, die on stop.
#[tokio::test]
async fn session_grandchildren_are_stopped() -> Result<(), Error> {
    bounded(async {
        // `set -m` puts the background job in its own process group.
        let script = "set -m; sleep 30 & echo child=$!; wait";
        let unit = runner().spawn_unit(shell(script, true)).await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let mut output = unit.output();
        let child = number_after(&read_until(&mut output, "\n").await, "child=");
        cleanup.pid(child);
        eventually(|| async {
            unit.descendants()
                .await
                .is_ok_and(|d| d.iter().any(|p| p.pid == child))
        })
        .await;
        let info = devlanes_sys::process(child)?.expect("grandchild visible");
        assert_ne!(info.pgid, Some(unit.identity().pid));
        assert_eq!(info.sid, Some(unit.identity().sid));

        let report = unit.stop_with_report(StopPolicy::default()).await?;
        assert!(!report.stop.expect("stopped").escalated);
        assert!(!alive(child));
        Ok(())
    })
    .await
}

/// A grandchild that called setsid leaves the session but is stopped via the
/// descendant set.
#[tokio::test]
async fn setsid_grandchild_killed_via_descendants() -> Result<(), Error> {
    bounded(async {
        let helper = std::env::current_exe()?;
        let mut launch = shell(
            "\"$HELPER\" --exact helper_setsid_sleeper --ignored --nocapture --test-threads 1 & wait",
            true,
        );
        launch
            .env
            .insert("HELPER".into(), helper.to_string_lossy().into_owned());
        launch.env.insert(HELPER_ENV.into(), "1".into());
        let unit = runner().spawn_unit(launch).await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let mut output = unit.output();
        let escapee = number_after(&read_until(&mut output, "SETSID pid=").await, "SETSID pid=");
        cleanup.pid(escapee);
        eventually(|| async {
            unit.descendants()
                .await
                .is_ok_and(|d| d.iter().any(|p| p.pid == escapee && p.sid == Some(escapee)))
        })
        .await;
        assert!(
            unit.subjects()
                .await?
                .iter()
                .any(|s| s.pid == escapee && s.sid != unit.identity().sid)
        );

        let report = unit
            .stop_with_report(StopPolicy {
                signal: StopSignal::Terminate,
                timeout: Duration::from_secs(5),
            })
            .await?;
        assert!(report.escaped.is_empty(), "{:?}", report.escaped);
        assert!(!alive(escapee), "setsid grandchild survived stop");
        Ok(())
    })
    .await
}

/// After a natural leader exit, a session member that outlives it is swept, even
/// when no periodic snapshot ever saw it before it was reparented to init.
#[tokio::test]
async fn leftovers_swept_after_leader_exit() -> Result<(), Error> {
    bounded(async {
        // Pipes, so no terminal hangup kills the background job first. The long
        // interval leaves only the immediate first tick, which runs before the
        // delayed fork, so adoption must come from the scan done while reaping.
        let unit = PtyRunner::default()
            .with_track_interval(Duration::from_secs(60))
            .spawn_unit(shell("sleep 0.2; sleep 30 & echo child=$!", false))
            .await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let mut output = unit.output();
        let child = number_after(&read_until(&mut output, "\n").await, "child=");
        cleanup.pid(child);
        let report = unit.wait_report().await?;
        assert_eq!(report.outcome.code, Some(0));
        assert!(report.stop.is_none());
        assert!(report.swept.iter().any(|p| p.pid == child), "{report:?}");
        assert!(report.escaped.is_empty(), "{report:?}");
        assert!(!alive(child));
        Ok(())
    })
    .await
}

/// `sh -c script` where `$HELPER` re-runs this binary as [`helper_setsid_sleeper`].
fn with_helper(script: &str, pty: bool) -> Result<LaunchSpec, Error> {
    let mut launch = shell(script, pty);
    let helper = std::env::current_exe()?;
    launch
        .env
        .insert("HELPER".into(), helper.to_string_lossy().into_owned());
    launch.env.insert(HELPER_ENV.into(), "1".into());
    Ok(launch)
}

/// Starts the setsid helper in the background from `sh`.
const START_HELPER: &str =
    "\"$HELPER\" --exact helper_setsid_sleeper --ignored --nocapture --test-threads 1 &";

/// A daemonized grandchild that was never seen in a snapshot keeps the output open
/// after the leader exits: it is reported as escaped, the readers end and the
/// terminal is released instead of leaking.
#[tokio::test]
async fn untracked_output_holder_is_reported_and_released() -> Result<(), Error> {
    bounded(async {
        for pty in [false, true] {
            // Forked after the first snapshot and orphaned before the next one.
            let script = format!("sleep 0.3; ( {START_HELPER} ); sleep 0.2; exit 0");
            let unit = PtyRunner::default()
                .with_track_interval(Duration::from_secs(60))
                .spawn_unit(with_helper(&script, pty)?)
                .await?;
            let mut cleanup = Cleanup::default();
            cleanup.unit(&unit);
            let output = unit.output();
            let report = unit.wait_report().await?;
            let chunks = collect(output).await;
            let seen: String = chunks
                .iter()
                .map(|c| String::from_utf8_lossy(&c.bytes).into_owned())
                .collect();
            let holder = number_after(&seen, "SETSID pid=");
            cleanup.pid(holder);
            assert!(alive(holder), "pty={pty}: holder should still run");
            // macOS revokes a session's terminal when its leader exits, so there the
            // holder loses the slave and the master reaches EOF on its own.
            if !(pty && cfg!(target_os = "macos")) {
                assert!(
                    report.escaped.iter().any(|p| p.pid == holder),
                    "pty={pty}: {report:?}"
                );
            }
            if pty {
                assert!(matches!(unit.size(), Err(Error::Exited)));
            }
        }
        Ok(())
    })
    .await
}

/// Tasks may start daemons: with `KeepDaemonized`, a descendant that left the
/// session survives the natural exit and is listed in `left_running`; the default
/// sweep kills it.
#[tokio::test]
async fn daemonized_task_descendants_are_left_running() -> Result<(), Error> {
    bounded(async {
        for keep in [true, false] {
            let sweep = if keep {
                ExitSweep::KeepDaemonized
            } else {
                ExitSweep::KillAll
            };
            let script = format!("{START_HELPER} sleep 0.5; exit 0");
            let unit = runner()
                .with_exit_sweep(sweep)
                .spawn_unit(with_helper(&script, false)?)
                .await?;
            let mut cleanup = Cleanup::default();
            cleanup.unit(&unit);
            let mut output = unit.output();
            let daemon = number_after(&read_until(&mut output, "SETSID pid=").await, "SETSID pid=");
            cleanup.pid(daemon);
            let report = unit.wait_report().await?;
            if keep {
                assert!(alive(daemon), "daemon was killed");
                assert!(
                    report.left_running.iter().any(|p| p.pid == daemon),
                    "{report:?}"
                );
                assert!(report.escaped.iter().all(|p| p.pid != daemon), "{report:?}");
            } else {
                assert!(!alive(daemon), "daemon survived");
                assert!(report.swept.iter().any(|p| p.pid == daemon), "{report:?}");
                assert!(report.left_running.is_empty());
            }
        }
        Ok(())
    })
    .await
}

/// A stopped (SIGSTOP) unit still ends on the stop signal, because SIGCONT follows it.
#[tokio::test]
async fn stopped_unit_acts_on_stop_signal() -> Result<(), Error> {
    bounded(async {
        let unit = runner()
            .spawn_unit(shell("echo ready; kill -STOP $$; sleep 30", false))
            .await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        let mut output = unit.output();
        read_until(&mut output, "ready").await;
        eventually(|| async {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &unit.identity().pid.to_string()])
                .output()
                .is_ok_and(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim_start()
                        .starts_with('T')
                })
        })
        .await;
        let report = unit
            .stop_with_report(StopPolicy {
                signal: StopSignal::Terminate,
                timeout: Duration::from_secs(5),
            })
            .await?;
        assert_eq!(report.outcome.signal, Some(15), "{report:?}");
        assert!(!report.stop.expect("stopped").escalated);
        Ok(())
    })
    .await
}

/// Stops landing right as the leader exits on its own still produce one report
/// that every waiter, early or late, receives.
#[tokio::test]
async fn stop_racing_natural_exit_reports_once() -> Result<(), Error> {
    bounded(async {
        for round in 0..40_u64 {
            let lifetime = format!("0.0{}", round % 10);
            let pty = round % 2 == 0;
            let unit = runner()
                .spawn_unit(shell(&format!("sleep {lifetime}"), pty))
                .await?;
            let mut cleanup = Cleanup::default();
            cleanup.unit(&unit);
            let early = {
                let unit = Arc::clone(&unit);
                tokio::spawn(async move { unit.wait_report().await })
            };
            tokio::time::sleep(Duration::from_millis((round % 10) * 10)).await;
            let stopped = unit
                .stop_with_report(StopPolicy {
                    signal: StopSignal::Terminate,
                    timeout: Duration::from_secs(2),
                })
                .await?;
            let early = early
                .await
                .map_err(|e| Error::Integration(e.to_string()))??;
            assert_eq!(early, stopped, "round {round}");
            assert_eq!(unit.wait_report().await?, stopped, "round {round}");
            let outcome = stopped.outcome;
            assert!(
                outcome.code == Some(0) || outcome.signal == Some(15),
                "round {round}: {outcome:?}"
            );
        }
        Ok(())
    })
    .await
}

/// Many waiters, including late ones, observe the same outcome.
#[tokio::test]
async fn concurrent_waiters_share_exit() -> Result<(), Error> {
    bounded(async {
        let unit: Arc<dyn Unit> = runner()
            .spawn_unit(shell("sleep 0.3; exit 5", false))
            .await?;
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let unit = Arc::clone(&unit);
                tokio::spawn(async move { unit.wait().await })
            })
            .collect();
        for waiter in waiters {
            let exit = waiter
                .await
                .map_err(|e| Error::Integration(e.to_string()))??;
            assert_eq!(exit.code, Some(5));
        }
        assert_eq!(unit.wait().await?.code, Some(5));
        // Stopping an exited unit is a no-op.
        unit.stop(StopPolicy::default()).await?;
        Ok(())
    })
    .await
}

/// tokio::process helpers keep their exit statuses while units are reaped by PID.
#[tokio::test]
async fn helper_commands_coexist_with_unit_reaping() -> Result<(), Error> {
    bounded(async {
        let units: Vec<_> = futures_join((0..4).map(|i| async move {
            runner()
                .spawn_unit(shell(&format!("sleep 0.{i}; exit {i}"), i % 2 == 0))
                .await
        }))
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
        let helpers: Vec<_> = (0..10)
            .map(|i| {
                tokio::spawn(async move {
                    tokio::process::Command::new("/bin/sh")
                        .args(["-c", &format!("sleep 0.0{i}; exit 3")])
                        .status()
                        .await
                })
            })
            .collect();
        for helper in helpers {
            let status = helper
                .await
                .map_err(|e| Error::Integration(e.to_string()))??;
            assert_eq!(status.code(), Some(3));
        }
        for (i, unit) in units.iter().enumerate() {
            assert_eq!(unit.wait().await?.code, i32::try_from(i).ok());
        }
        Ok(())
    })
    .await
}

/// Runs futures as concurrent tasks and returns their results in input order.
async fn futures_join<T, F>(futures: impl Iterator<Item = F>) -> Vec<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let handles: Vec<_> = futures.map(tokio::spawn).collect();
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.expect("task panicked"));
    }
    results
}

/// Children get the daemon's original soft open-file limit after the daemon raised its own.
#[tokio::test]
async fn children_get_original_nofile_limit() -> Result<(), Error> {
    bounded(async {
        devlanes_sys::raise_nofile_limit()?;
        let original = devlanes_sys::child_nofile_limit().expect("recorded");
        let expected = original
            .soft
            .map_or_else(|| "unlimited".to_owned(), |soft| soft.to_string());
        let unit = runner().spawn_unit(shell("ulimit -n", false)).await?;
        let chunks = collect(unit.output()).await;
        assert_eq!(text(&chunks, OutputStream::Stdout).trim(), expected);
        Ok(())
    })
    .await
}

/// Invalid launches and resizes fail with typed errors.
#[tokio::test]
async fn invalid_requests() -> Result<(), Error> {
    bounded(async {
        assert!(matches!(
            runner().spawn_unit(spec("", &[], true)).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            runner()
                .spawn_unit(spec("/nonexistent/devlanes-test", &[], false))
                .await,
            Err(Error::Io(_))
        ));
        let unit = runner().spawn_unit(shell("sleep 5", true)).await?;
        let mut cleanup = Cleanup::default();
        cleanup.unit(&unit);
        assert!(matches!(
            unit.resize(TerminalSize {
                columns: 0,
                rows: 1
            })
            .await,
            Err(Error::InvalidInput(_))
        ));
        let report = unit
            .stop_with_report(StopPolicy {
                signal: StopSignal::Terminate,
                timeout: Duration::ZERO,
            })
            .await?;
        assert!(report.outcome.signal.is_some());
        Ok(())
    })
    .await
}
