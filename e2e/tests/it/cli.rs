//! Ad-hoc units, config errors, daemon protocol skew and agent output wording.

#[cfg(unix)]
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::DirBuilderExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

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

/// A broken `devlanes.yaml` fails `config check` and `start` with exit 1, a
/// `devlanes.yaml:<line>:<column>` location and, for a misspelt key, the key it
/// probably meant.
#[test]
fn config_errors_point_at_the_line() -> Result<()> {
    let cases = [
        // Line 5: a misspelt service key.
        (
            "project: broken\nservices:\n  api:\n    run: [fixture-server]\n    raedy: { http: { path: /healthz } }\n",
            "devlanes.yaml is invalid at line 5,",
            Some("unknown key `raedy`; did you mean `ready`?"),
        ),
        // Line 2: a misspelt top-level key.
        (
            "project: broken\nservces:\n  api:\n    run: [fixture-server]\n",
            "devlanes.yaml is invalid at line 2,",
            Some("unknown key `servces`; did you mean `services`?"),
        ),
        // Line 4: a port that is not a number.
        (
            "project: broken\nservices:\n  api:\n    env: { PORT: [1] }\n    run: [fixture-server]\n",
            "devlanes.yaml is invalid at line 4,",
            None,
        ),
    ];
    for (yaml, location, suggestion) in cases {
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
            match suggestion {
                Some(text) => assert!(
                    output.stderr.contains(text),
                    "expected {text:?} in stderr\n{output}"
                ),
                None => assert!(!output.stderr.contains("did you mean"), "{output}"),
            }
        }
    }
    Ok(())
}

/// A repository without `devlanes.yaml`: `start` and `config check` exit 3 with the
/// worded `no_config` error (where the search looked, and how to create the file),
/// never the raw code.
#[test]
fn start_and_config_check_without_config() -> Result<()> {
    let lab = Lab::without_config(BUDGET)?;
    for args in [&["config", "check"][..], &["start"][..]] {
        for mode in ["plain", "json"] {
            let mut command = lab.command(lab.repo(), args)?;
            command.env("DEVLANES_OUTPUT", mode);
            let output = lab.run(command, devlanes_e2e::COMMAND_TIMEOUT)?;
            output.expect_code(3)?;
            assert!(output.stderr.starts_with("error: "), "{output}");
            assert!(
                output.stderr.contains("no devlanes.yaml in"),
                "expected the no_config wording\n{output}"
            );
            assert!(
                output.stderr.contains("create devlanes.yaml"),
                "expected a hint that creates the file\n{output}"
            );
            assert!(
                !format!("{}{}", output.stdout, output.stderr).contains("no_config"),
                "the raw code leaked\n{output}"
            );
        }
    }
    Ok(())
}

/// `daemon status` never starts a daemon: without one it says so and exits 1, and
/// no daemon or socket exists afterwards; with one it reports `running: true`.
#[test]
fn daemon_status_never_spawns() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let stopped = lab.devlanes(lab.repo(), &["daemon", "status", "--json"])?;
    stopped.expect_code(1)?;
    let report = stopped.json()?;
    assert_eq!(report["running"], false, "{stopped}");
    let socket = need(report["socket"].as_str(), "socket")?.to_owned();
    let mut plain = lab.command(lab.repo(), &["daemon", "status"])?;
    plain.env("DEVLANES_OUTPUT", "plain");
    let plain = lab.run(plain, devlanes_e2e::COMMAND_TIMEOUT)?;
    plain.expect_code(1)?;
    assert!(
        plain.stdout.contains("daemon is not running (socket"),
        "{plain}"
    );
    assert!(!lab.daemon_running(), "daemon status started a daemon");
    assert!(
        !std::path::Path::new(&socket).exists(),
        "daemon status created {socket}"
    );

    lab.json(lab.repo(), &["status"])?;
    let running = lab.json(lab.repo(), &["daemon", "status", "--json"])?;
    assert_eq!(running["running"], true, "{running}");
    assert!(running["pid"].is_u64(), "{running}");
    Ok(())
}

/// `doctor` never starts a daemon either: without one it reports `running: false`
/// with its local checks, and the stopped daemon is a warning, not a failure.
#[test]
fn doctor_without_a_daemon_checks_locally() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    let output = lab.devlanes(lab.repo(), &["doctor", "--json"])?;
    output.expect_code(0)?;
    let report = output.json()?;
    assert_eq!(report["running"], false, "{output}");
    assert!(report["user_config"]["valid"].is_boolean(), "{output}");
    assert!(report["release"]["version"].is_string(), "{output}");
    let mut plain = lab.command(lab.repo(), &["doctor"])?;
    plain.env("DEVLANES_OUTPUT", "plain");
    let plain = lab.run(plain, devlanes_e2e::COMMAND_TIMEOUT)?;
    assert!(plain.stdout.contains("warn not running (socket"), "{plain}");
    assert!(plain.stdout.contains("fix: devlanes start"), "{plain}");
    assert!(!lab.daemon_running(), "doctor started a daemon");
    Ok(())
}

/// A daemon that answers the hello with another protocol: the CLI exits 4 and says
/// what runs and what a restart costs, without spawning a second daemon. The fake
/// daemon stands in for a second binary build.
#[test]
fn protocol_mismatch_exits_4() -> Result<()> {
    let lab = Lab::new("two-services", BUDGET)?;
    #[cfg(unix)]
    let _fake = FakeDaemon::serve(&lab.home().join("run"), 999, 3)?;
    #[cfg(windows)]
    let _fake = FakeDaemon::serve(&lab, 999, 3)?;
    for args in [&["status"][..], &["start"][..]] {
        let mut command = lab.command(lab.repo(), args)?;
        command.env("DEVLANES_OUTPUT", "plain");
        let output = lab.run(command, Duration::from_secs(30))?;
        output.expect_code(4)?;
        assert!(
            output
                .stderr
                .contains("the running daemon is devlanes 0.0.0-fake (protocol 999"),
            "{output}"
        );
        assert!(
            output
                .stderr
                .contains("restarting it stops its 3 managed processes"),
            "{output}"
        );
        assert!(
            output.stderr.contains("hint: devlanes daemon restart"),
            "{output}"
        );
        assert!(!output.stderr.contains("protocol_mismatch"), "{output}");
    }
    assert!(
        !lab.daemon_running(),
        "no real daemon may start next to the fake"
    );
    Ok(())
}

/// A Unix socket at the daemon's path that answers every hello with a fixed
/// protocol and closes. Dropping it stops the thread and removes the socket.
#[cfg(unix)]
struct FakeDaemon {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl FakeDaemon {
    /// Listens on `<run>/daemon.sock` (`run` is created private, as the client expects).
    fn serve(run: &Path, protocol: u32, managed_processes: usize) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(run)?;
        let socket = run.join("daemon.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let hello = serde_json::json!({
            "type": "hello",
            "body": {
                "protocol": protocol,
                "version": "0.0.0-fake",
                "build_id": "fake",
                "features": [],
                "managed_processes": managed_processes,
            },
        });
        let mut frame = vec![0u8];
        frame.extend_from_slice(&serde_json::to_vec(&hello)?);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + BUDGET;
            while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = answer(stream, &frame);
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Self {
            socket,
            stop,
            thread: Some(thread),
        })
    }
}

#[cfg(unix)]
impl Drop for FakeDaemon {
    /// Stops accepting and removes the socket.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

/// The daemon's named pipe served by a thread that answers every hello with a
/// fixed protocol and closes. Dropping it stops the thread, which closes the pipe.
#[cfg(windows)]
struct FakeDaemon {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl FakeDaemon {
    /// Serves the pipe the client derives from the lab's home; `daemon status`
    /// reports its name without starting a daemon.
    fn serve(lab: &Lab, protocol: u32, managed_processes: usize) -> Result<Self> {
        let status = lab.devlanes(lab.repo(), &["daemon", "status", "--json"])?;
        let name = crate::need(status.json()?["socket"].as_str(), "pipe name")?.to_owned();
        let hello = serde_json::json!({
            "type": "hello",
            "body": {
                "protocol": protocol,
                "version": "0.0.0-fake",
                "build_id": "fake",
                "features": [],
                "managed_processes": managed_processes,
            },
        });
        let mut frame = vec![0u8];
        frame.extend_from_slice(&serde_json::to_vec(&hello)?);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        // The daemon's descriptor, so the pipe is owned by this user even on an
        // elevated runner, where a default one would be owned by Administrators
        // and fail the client's owner check.
        let pipes = devlanes_sys_windows::pipe::PipeFactory::current_user(&name)?;
        // The first instance exists before this returns, so the client never finds
        // the pipe missing and spawns a real daemon.
        let first = {
            let _context = runtime.enter();
            pipes.create(true)?
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            runtime.block_on(async move {
                let deadline = Instant::now() + BUDGET;
                let mut server = first;
                while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                    let Ok(connected) =
                        tokio::time::timeout(Duration::from_millis(20), server.connect()).await
                    else {
                        continue;
                    };
                    // The next instance exists before this one goes away, for the
                    // same reason as the first.
                    let Ok(next) = pipes.create(false) else {
                        break;
                    };
                    if connected.is_ok() {
                        let _ = answer_pipe(&mut server, &frame).await;
                    }
                    server = next;
                }
            });
        });
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

#[cfg(windows)]
impl Drop for FakeDaemon {
    /// Stops accepting; the pipe goes with the thread's runtime.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads the client's hello frame (u32 length, then the payload), sends `frame`
/// and waits up to two seconds for the client to hang up, so closing the pipe
/// cannot discard the answer before it is read.
#[cfg(windows)]
async fn answer_pipe(
    pipe: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    frame: &[u8],
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let exchange = async {
        let mut length = [0u8; 4];
        pipe.read_exact(&mut length).await?;
        let mut hello = vec![0u8; u32::from_be_bytes(length).min(1 << 20) as usize];
        pipe.read_exact(&mut hello).await?;
        let size = u32::try_from(frame.len()).map_err(std::io::Error::other)?;
        pipe.write_all(&size.to_be_bytes()).await?;
        pipe.write_all(frame).await?;
        pipe.flush().await
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    let mut rest = [0u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(2), pipe.read(&mut rest)).await;
    Ok(())
}

/// Reads the client's hello frame (u32 length, then the payload) and sends `frame`.
#[cfg(unix)]
fn answer(mut stream: UnixStream, frame: &[u8]) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let mut hello = vec![0u8; u32::from_be_bytes(length).min(1 << 20) as usize];
    stream.read_exact(&mut hello)?;
    let size = u32::try_from(frame.len()).map_err(std::io::Error::other)?;
    stream.write_all(&size.to_be_bytes())?;
    stream.write_all(frame)?;
    stream.flush()
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
    // A panic, unlike an error, prints the daemon log, which says where a
    // start that timed out waited (seen once on macOS).
    assert_eq!(failed.code, Some(1), "{failed}");
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
