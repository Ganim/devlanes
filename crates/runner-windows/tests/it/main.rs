//! Live runs of the Windows runner against `cmd` scripts, Git's `sh.exe` and
//! `fixture-server` from devlanes-testing, which must be built first
//! (`cargo build -p devlanes-testing --bin fixture-server`, or any workspace
//! build); tests never run cargo themselves.
#![cfg(windows)]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use devlanes_core::{
    Error, LaunchSpec, OutputChunk, OutputStream, ProcessIdentity, StopPolicy, StopSignal,
    TerminalSize, Unit,
};
use devlanes_runner_windows::{
    ConsoleHelper, ExitReport, HELPER_ARG, StderrMode, WindowsRunner, WindowsUnit,
    console_ctrl_main,
};
use tokio::sync::broadcast::{Receiver, error::RecvError};

/// Upper bound for each test body.
const LIMIT: Duration = Duration::from_secs(60);
/// Grace for stops that are expected to succeed without escalating.
const GRACE: Duration = Duration::from_secs(20);
/// `STATUS_CONTROL_C_EXIT` as the signed code an exit report carries.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

/// Not a test of its own: started with [`HELPER_ARG`] among its arguments,
/// this binary is the runner's console helper, as `devlanes __console-ctrl`
/// is in the shipped binary.
#[test]
fn helper_entry() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some(at) = args.iter().position(|arg| arg == HELPER_ARG) {
        // A test that counts helpers puts a log file's path right before the
        // marker (the test harness takes it as a filter that matches nothing).
        if let Some(log) = at
            .checked_sub(1)
            .map(|before| Path::new(&args[before]))
            .filter(|path| path.is_absolute())
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .expect("helper log");
            writeln!(file, "{:?}", &args[at + 1..]).expect("helper log line");
        }
        let code = console_ctrl_main(args[at + 1..].iter().cloned());
        std::process::exit(i32::from(code));
    }
}

/// The runner with this test binary as its console helper.
fn runner() -> WindowsRunner {
    let helper = ConsoleHelper {
        program: std::env::current_exe().expect("test binary path"),
        args: ["--exact", "helper_entry", "--nocapture", HELPER_ARG]
            .map(OsString::from)
            .into(),
    };
    WindowsRunner::default().with_console_helper(helper)
}

/// `fixture-server.exe` next to the `deps` directory holding this binary.
fn fixture() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target directory")
        .join("fixture-server.exe");
    assert!(
        path.is_file(),
        "{} is missing; build it first with `cargo build -p devlanes-testing --bin fixture-server`",
        path.display()
    );
    path
}

/// The variables Windows programs need to start (Winsock needs `SystemRoot`),
/// with `PATH` holding System32 and `dirs`.
fn env(dirs: &[&Path]) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = ["SystemRoot", "windir", "TEMP", "TMP", "PATHEXT"]
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
        .collect();
    let system = devlanes_sys_windows::file::system_directory().expect("System32");
    let path = std::env::join_paths(dirs.iter().map(|dir| dir.to_path_buf()).chain([system]))
        .expect("PATH");
    env.insert("PATH".into(), path.to_string_lossy().into_owned());
    env
}

/// A pipe-mode spec started in `cwd`.
fn spec(program: &str, args: &[&str], cwd: &Path, env: BTreeMap<String, String>) -> LaunchSpec {
    LaunchSpec {
        program: program.into(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: cwd.to_path_buf(),
        env,
        pty: false,
        size: TerminalSize::default(),
    }
}

/// `fixture-server` with `args`, run from `cwd`.
fn fixture_spec(args: &[&str], cwd: &Path) -> LaunchSpec {
    let fixture = fixture();
    spec(&fixture.to_string_lossy(), args, cwd, env(&[]))
}

/// The code of the runner's refusal inside `error`, if it is one.
fn refusal(error: &Error) -> Option<&'static str> {
    let Error::Io(io) = error else { return None };
    io.get_ref()?
        .downcast_ref::<devlanes_core::SpawnRefusal>()
        .map(|refusal| refusal.code)
}

/// Writes a batch file and returns its path.
fn script(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\r\n") + "\r\n").expect("script");
    path
}

/// Ends the unit's job when the test is done, passed or failed.
struct Cleanup(Arc<WindowsUnit>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.terminate();
    }
}

/// Ends recorded processes when the test is done, for units whose handle the
/// test dropped or whose processes it stops by identity.
struct Kill(Vec<ProcessIdentity>);

impl Drop for Kill {
    fn drop(&mut self) {
        for identity in &self.0 {
            let _ = devlanes_sys::terminate_verified(identity, 1);
        }
    }
}

/// Every chunk until the channel closes.
async fn drain(mut output: Receiver<OutputChunk>) -> Vec<OutputChunk> {
    let mut chunks = Vec::new();
    loop {
        match output.recv().await {
            Ok(chunk) => chunks.push(chunk),
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return chunks,
        }
    }
}

/// The text of `chunks` from `stream`.
fn text(chunks: &[OutputChunk], stream: OutputStream) -> String {
    chunks
        .iter()
        .filter(|chunk| chunk.stream == stream)
        .map(|chunk| String::from_utf8_lossy(&chunk.bytes).into_owned())
        .collect()
}

/// Reads output until it contains `needle`; returns everything read.
async fn read_until(output: &mut Receiver<OutputChunk>, needle: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(needle) {
        match output.recv().await {
            Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk.bytes)),
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => panic!("output ended before {needle:?}: {seen}"),
        }
    }
    seen
}

/// The port of the `LISTENING <address>` line `fixture-server` prints.
fn listening_port(output: &str) -> u16 {
    let line = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("LISTENING "))
        .expect("LISTENING line");
    line.parse::<std::net::SocketAddr>()
        .expect("address")
        .port()
}

/// A stop with `signal` and `timeout`.
fn policy(signal: StopSignal, timeout: Duration) -> StopPolicy {
    StopPolicy { signal, timeout }
}

/// Whether any of `identities` still names a running process.
fn any_alive(identities: &[ProcessIdentity]) -> bool {
    identities
        .iter()
        .any(|identity| devlanes_sys::matches(identity).unwrap_or(true))
}

/// Runs `body` with the test's time limit.
async fn bounded(body: impl Future<Output = ()>) {
    tokio::time::timeout(LIMIT, body).await.expect("bounded");
}

/// stdout and stderr arrive tagged, or merged as stdout, and the exit code is
/// reported. The script is found on `PATH` through `PATHEXT`.
#[tokio::test]
async fn output_and_exit_code() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        script(
            dir.path(),
            "talk.cmd",
            &["@echo out", "@>&2 echo err", "@exit /b 3"],
        );
        let spec = || spec("talk", &[], dir.path(), env(&[dir.path()]));

        let unit = runner().spawn_unit(spec()).await.expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let chunks = drain(unit.output()).await;
        let report = unit.wait_report().await.expect("report");
        assert_eq!(report.outcome.code, Some(3));
        assert_eq!(report.stop, None);
        assert!(text(&chunks, OutputStream::Stdout).contains("out"));
        assert!(text(&chunks, OutputStream::Stderr).contains("err"));

        let merged = runner()
            .with_stderr(StderrMode::Merged)
            .spawn_unit(spec())
            .await
            .expect("spawn");
        let _cleanup = Cleanup(merged.clone());
        let chunks = drain(merged.output()).await;
        assert!(chunks.iter().all(|c| c.stream == OutputStream::Stdout));
        let all = text(&chunks, OutputStream::Stdout);
        assert!(all.contains("out") && all.contains("err"), "{all}");
        assert_eq!(merged.wait().await.expect("exit").code, Some(3));
    })
    .await;
}

/// [`spec`] for a `pty: true` unit.
fn pty_spec(program: &str, args: &[&str], cwd: &Path, env: BTreeMap<String, String>) -> LaunchSpec {
    LaunchSpec {
        pty: true,
        ..spec(program, args, cwd, env)
    }
}

/// `text` without control sequences, as a reader of the screen sees it: a
/// pseudo console sends runs of spaces as cursor moves.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\x1b', Some('[')) => {
                chars.next();
                // Parameters and intermediates, then the final byte.
                while chars.next_if(|c| !('@'..='~').contains(c)).is_some() {}
                chars.next();
            }
            ('\x1b', Some(']')) => {
                // An OSC ends with BEL or ST.
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The widths `mode con` reported in `output`, in order.
fn reported_columns(output: &str) -> Vec<u32> {
    plain(output)
        .split("Columns:")
        .skip(1)
        .filter_map(|after| {
            let digits: String = after
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .collect()
}

/// Reads output until `done` holds for everything read; returns it.
async fn read_while(output: &mut Receiver<OutputChunk>, done: impl Fn(&str) -> bool) -> String {
    let mut seen = String::new();
    while !done(&seen) {
        match output.recv().await {
            Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk.bytes)),
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => panic!("output ended early: {seen:?}"),
        }
    }
    seen
}

/// A `pty: true` unit runs in a pseudo console, 300 columns wide while no
/// viewer has sized it: typed input reaches it, its output is terminal output
/// without the console host's mode toggles, and the last line written before
/// it exits arrives before the output ends.
#[tokio::test]
async fn pty_units_run_in_a_pseudo_console() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        // Delayed expansion (`!NAME!`), since `%NAME%` expands before `set /p` runs.
        let unit = runner()
            .spawn_unit(pty_spec(
                "cmd",
                &["/d", "/q", "/v:on", "/c", "set /p NAME=& echo hello !NAME!"],
                dir.path(),
                env(&[]),
            ))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        assert!(unit.renders_screen());
        assert_eq!(unit.size().expect("size"), TerminalSize::RENDERED_HEADLESS);
        let output = unit.output();
        unit.input(bytes::Bytes::from_static(b"acme\r"))
            .await
            .expect("input");
        let chunks = drain(output).await;
        assert!(chunks.iter().all(|c| c.stream == OutputStream::Terminal));
        let all = text(&chunks, OutputStream::Terminal);
        assert!(plain(&all).contains("hello acme"), "{all:?}");
        for toggle in ["\x1b[?9001", "\x1b[?1004"] {
            assert!(!all.contains(toggle), "{all:?}");
        }
        let report = unit.wait_report().await.expect("report");
        assert_eq!(report.outcome.code, Some(0), "{report:?}");
        assert!(matches!(
            unit.input(bytes::Bytes::from_static(b"x")).await,
            Err(Error::Exited)
        ));
    })
    .await;
}

/// A resize reaches the programs in the pseudo console: `mode con` reports
/// the new width, and the output marks where the console host's repaint at
/// that size begins.
#[tokio::test]
async fn pty_resizes_reach_the_console() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let size = TerminalSize {
            columns: 100,
            rows: 30,
        };
        let unit = runner()
            .spawn_unit(LaunchSpec {
                size,
                ..pty_spec("cmd", &["/d", "/q", "/k"], dir.path(), env(&[]))
            })
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        unit.input(bytes::Bytes::from_static(b"mode con\r"))
            .await
            .expect("input");
        read_while(&mut output, |seen| reported_columns(seen).contains(&100)).await;
        let wider = TerminalSize {
            columns: 140,
            rows: 30,
        };
        unit.resize(wider).await.expect("resize");
        assert_eq!(unit.size().expect("size"), wider);
        unit.input(bytes::Bytes::from_static(b"mode con\r"))
            .await
            .expect("input");
        let seen = read_while(&mut output, |seen| reported_columns(seen).contains(&140)).await;
        // The repaint at the new size is marked once, and the console host's
        // own size reports are gone.
        assert_eq!(seen.matches("\x1b[8;").count(), 1, "{seen:?}");
        let marker = seen.find("\x1b[8;30;140t").expect("size marker");
        assert!(
            seen[marker..].starts_with("\x1b[8;30;140t\x1b[?25l\x1b[H"),
            "{seen:?}"
        );
        unit.input(bytes::Bytes::from_static(b"exit\r"))
            .await
            .expect("input");
        assert_eq!(unit.wait().await.expect("exit").code, Some(0));
    })
    .await;
}

/// SIGINT is the Ctrl-C key in a pseudo console; the fixture shuts down on
/// it without escalation.
#[tokio::test]
async fn pty_sigint_types_ctrl_c() {
    devlanes_sys_windows::console::set_ctrl_c_ignored(false).expect("accept Ctrl-C");
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = fixture();
        let unit = runner()
            .spawn_unit(pty_spec(
                &fixture.to_string_lossy(),
                &["--port", "0"],
                dir.path(),
                env(&[]),
            ))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        let report = unit
            .stop_with_report(policy(StopSignal::Interrupt, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Interrupt, false);
        assert_eq!(report.outcome.code, Some(0), "{report:?}");
    })
    .await;
}

/// A `run:` string in a pseudo console stops on Ctrl-Break, shell and program.
#[tokio::test]
async fn pty_shell_units_stop_on_ctrl_break() {
    bounded(async {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = fixture().to_string_lossy().replace('\\', "/");
        let script = format!("'{fixture}' --port 0");
        let unit = runner()
            .with_shell(Some(shell))
            .spawn_unit(pty_spec("/bin/sh", &["-c", &script], dir.path(), env(&[])))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        let members: Vec<ProcessIdentity> = unit
            .processes()
            .await
            .expect("processes")
            .iter()
            .map(devlanes_sys::ProcessInfo::identity)
            .collect();
        let report = unit
            .stop_with_report(policy(StopSignal::Terminate, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Terminate, false);
        assert!(!any_alive(&members), "{members:?}");
    })
    .await;
}

/// A `.cmd` shim in a pseudo console asks "Terminate batch job (Y/N)?" once
/// its program stopped on Ctrl-Break; the runner answers it, and the stop
/// does not escalate.
#[tokio::test]
async fn pty_batch_prompts_are_answered() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = fixture();
        script(
            dir.path(),
            "serve.cmd",
            &[&format!("@\"{}\" --port 0", fixture.display())],
        );
        let unit = runner()
            .spawn_unit(pty_spec("serve", &[], dir.path(), env(&[dir.path()])))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        let rest = tokio::spawn(drain(output));
        let report = unit
            .stop_with_report(policy(StopSignal::Terminate, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Terminate, false);
        let rest = text(&rest.await.expect("join"), OutputStream::Terminal);
        assert!(plain(&rest).contains("Terminate batch job"), "{rest:?}");
    })
    .await;
}

/// A unit that writes without pause still ends promptly when stopped, and its
/// output closes: the console is closed only after its processes are gone,
/// and its output is drained to the end.
#[tokio::test]
async fn pty_teardown_of_a_chatty_unit() {
    bounded(async {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .with_shell(Some(shell))
            .spawn_unit(pty_spec(
                "/bin/sh",
                &["-c", "while :; do echo chatty chatty chatty chatty; done"],
                dir.path(),
                env(&[]),
            ))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "chatty").await;
        let rest = tokio::spawn(drain(output));
        let report = unit
            .stop_with_report(policy(StopSignal::Kill, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Kill, false);
        rest.await.expect("output closes");
        assert!(!any_alive(&[unit.identity()]));
    })
    .await;
}

/// Batch arguments get std's escaping, so `cmd.exe` does not run what follows
/// an `&`, and a line break is refused with a typed error.
#[tokio::test]
async fn batch_arguments_are_escaped_or_refused() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        // `%1` keeps the quotes the escaping added; `%~1` would strip them and
        // let the script itself run what follows the `&`.
        script(dir.path(), "args.cmd", &["@echo [%1]"]);
        let env = || env(&[dir.path()]);
        let unit = runner()
            .spawn_unit(spec("args", &["a&echo injected"], dir.path(), env()))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let output = text(&drain(unit.output()).await, OutputStream::Stdout);
        assert_eq!(output.trim(), r#"["a&echo injected"]"#);

        let error = runner()
            .spawn_unit(spec("args", &["a\nb"], dir.path(), env()))
            .await
            .expect_err("refused");
        assert_eq!(refusal(&error), Some("unsafe_batch_argument"), "{error:?}");
        assert!(error.to_string().contains("batch file"), "{error}");
    })
    .await;
}

/// Spawn failures come back as errors without starting anything: a missing
/// program or directory as I/O errors, a missing shell or an overlong
/// working directory as typed refusals.
#[tokio::test]
async fn spawn_failures_are_typed() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = runner()
            .spawn_unit(spec("no-such-tool", &[], dir.path(), env(&[dir.path()])))
            .await
            .expect_err("missing program");
        assert!(
            matches!(&missing, Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
            "{missing:?}"
        );
        assert_eq!(refusal(&missing), Some("not_found"));

        let gone = dir.path().join("gone");
        let error = runner()
            .spawn_unit(fixture_spec(&["--port", "0"], &gone))
            .await
            .expect_err("missing directory");
        assert!(matches!(error, Error::Io(_)), "{error:?}");

        let long = dir.path().join("x".repeat(300));
        let error = runner()
            .spawn_unit(fixture_spec(&["--port", "0"], &long))
            .await
            .expect_err("long directory");
        assert_eq!(refusal(&error), Some("cwd_too_long"), "{error:?}");
        assert!(error.to_string().contains("characters long"), "{error}");

        let error = runner()
            .with_shell(None)
            .spawn_unit(spec("/bin/sh", &["-c", "exit 0"], dir.path(), env(&[])))
            .await
            .expect_err("no shell");
        assert_eq!(refusal(&error), Some("shell_missing"), "{error:?}");
        assert!(error.to_string().contains("Git for Windows"), "{error}");
    })
    .await;
}

/// A foreground command is resolved like a unit: a `.cmd` file through
/// `PATHEXT`, its exit code reported, a missing program typed.
#[tokio::test]
async fn foreground_commands_resolve_like_units() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        script(dir.path(), "fails.cmd", &["@exit /b 7"]);
        let path = dir.path().to_path_buf();
        let code = tokio::task::spawn_blocking(move || {
            let child =
                devlanes_runner_windows::spawn_foreground("fails", &[], &env(&[&path]), &path)
                    .expect("spawn");
            let exited = child.wait_timeout(LIMIT).expect("wait");
            if exited.is_none() {
                let _ = child.terminate(1);
            }
            exited
        })
        .await
        .expect("join")
        .expect("exited in time");
        assert_eq!(devlanes_runner_windows::shell_exit_code(code), 7);
        let missing =
            devlanes_runner_windows::spawn_foreground("no-such-tool", &[], &env(&[]), dir.path())
                .expect_err("missing");
        assert_eq!(missing.code(), "not_found");
    })
    .await;
}

/// A shell unit finds Git's tools (`sleep`, `basename`) through the
/// directory appended to its `PATH`, which only holds System32 here.
#[tokio::test]
async fn run_strings_find_git_tools() {
    bounded(async {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .with_shell(Some(shell))
            .spawn_unit(spec(
                "/bin/sh",
                &["-c", "sleep 0 && basename /a/tool"],
                dir.path(),
                env(&[]),
            ))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let output = text(&drain(unit.output()).await, OutputStream::Stdout);
        assert_eq!(output, "tool\n");
        assert_eq!(unit.wait().await.expect("exit").code, Some(0));
    })
    .await;
}

/// `/bin/sh -c` runs through Git's `sh.exe`, which receives the script
/// intact (backslashes and quotes included) and MSYS path conversion off.
#[tokio::test]
async fn run_strings_go_through_git_sh() {
    bounded(async {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let script = r#"printf '[%s]\n' 'a\b' "q\"q" "$MSYS_NO_PATHCONV" /c/x
exit 4"#;
        let unit = runner()
            .with_shell(Some(shell))
            .spawn_unit(spec("/bin/sh", &["-c", script], dir.path(), env(&[])))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let output = text(&drain(unit.output()).await, OutputStream::Stdout);
        assert_eq!(output, "[a\\b]\n[q\"q]\n[1]\n[/c/x]\n");
        assert_eq!(unit.wait().await.expect("exit").code, Some(4));
    })
    .await;
}

/// SIGTERM is Ctrl-Break to the unit's console; the fixture shuts down on it
/// without escalation, and its port is reported while it runs.
#[tokio::test]
async fn sigterm_sends_ctrl_break() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .spawn_unit(fixture_spec(&["--port", "0"], dir.path()))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        let port = listening_port(&read_until(&mut output, "LISTENING").await);
        assert!(
            unit.listening_ports()
                .await
                .expect("ports")
                .iter()
                .any(|socket| socket.addr.port() == port)
        );
        let report = unit
            .stop_with_report(policy(StopSignal::Terminate, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Terminate, false);
        assert!(!any_alive(&[unit.identity()]));
    })
    .await;
}

/// SIGINT is Ctrl-C for a unit started without the shell.
#[tokio::test]
async fn sigint_sends_ctrl_c() {
    // Units inherit the ignore-Ctrl-C flag, which the terminal that started
    // the tests may have set; the daemon clears it the same way.
    devlanes_sys_windows::console::set_ctrl_c_ignored(false).expect("accept Ctrl-C");
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .spawn_unit(fixture_spec(&["--port", "0"], dir.path()))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        let report = unit
            .stop_with_report(policy(StopSignal::Interrupt, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Interrupt, false);
        assert_eq!(report.outcome.code, Some(0), "{report:?}");
    })
    .await;
}

/// SIGINT to a `run:` string is Ctrl-Break, since the shell's programs
/// ignore Ctrl-C; it ends both the shell and the program.
#[tokio::test]
async fn sigint_through_the_shell_is_ctrl_break() {
    bounded(async {
        let shell = devlanes_shell_windows::windows_shell()
            .await
            .expect("Git for Windows' sh.exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = fixture().to_string_lossy().replace('\\', "/");
        let script = format!("'{fixture}' --port 0");
        let unit = runner()
            .with_shell(Some(shell))
            .spawn_unit(spec("/bin/sh", &["-c", &script], dir.path(), env(&[])))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        let members: Vec<ProcessIdentity> = unit
            .processes()
            .await
            .expect("processes")
            .iter()
            .map(devlanes_sys::ProcessInfo::identity)
            .collect();
        let report = unit
            .stop_with_report(policy(StopSignal::Interrupt, GRACE))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Interrupt, false);
        assert!(!any_alive(&members), "{members:?}");
    })
    .await;
}

/// A unit that ignores the event is terminated at the timeout and the stop
/// reports the escalation.
#[tokio::test]
async fn ignored_events_escalate() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .spawn_unit(fixture_spec(
                &["--port", "0", "--ignore-sigterm"],
                dir.path(),
            ))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        let report = unit
            .stop_with_report(policy(StopSignal::Terminate, Duration::from_secs(1)))
            .await
            .expect("stop");
        assert_stopped(&report, StopSignal::Terminate, true);
        assert_eq!(report.outcome.code, Some(1));
        assert!(!any_alive(&[unit.identity()]));
    })
    .await;
}

/// Children and grandchildren are job members: listed as processes and
/// subjects without the console host, and ended by SIGKILL with the leader;
/// repeating the stop changes nothing.
#[tokio::test]
async fn stop_ends_the_whole_tree() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .spawn_unit(fixture_spec(
                &["--port", "0", "--spawn-grandchild"],
                dir.path(),
            ))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let mut output = unit.output();
        read_until(&mut output, "grandchild pid=").await;
        let processes = unit.processes().await.expect("processes");
        assert_eq!(processes.len(), 3, "{processes:?}");
        assert!(
            processes
                .iter()
                .all(|p| p.name.eq_ignore_ascii_case("fixture-server.exe")),
            "{processes:?}"
        );
        let members: Vec<ProcessIdentity> = processes
            .iter()
            .map(devlanes_sys::ProcessInfo::identity)
            .collect();
        // The supervisor's next refresh lists them as subjects too.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let subjects = unit.subjects().await.expect("subjects");
        assert!(members.iter().all(|m| subjects.contains(m)), "{subjects:?}");

        let kill = policy(StopSignal::Kill, GRACE);
        let (first, second) =
            tokio::join!(unit.stop_with_report(kill), unit.stop_with_report(kill));
        let report = first.expect("stop");
        assert_eq!(second.expect("second stop"), report);
        assert_stopped(&report, StopSignal::Kill, false);
        assert!(!any_alive(&members), "{members:?}");
        assert!(unit.subjects().await.expect("subjects").is_empty());
    })
    .await;
}

/// When the leader exits on its own, processes it left in the job are swept.
#[tokio::test]
async fn natural_exit_sweeps_leftovers() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = fixture();
        script(
            dir.path(),
            "leave.cmd",
            &[
                &format!(
                    "@start \"\" /b \"{}\" --descendant child",
                    fixture.display()
                ),
                "@ping -n 3 127.0.0.1 >NUL",
                "@exit /b 0",
            ],
        );
        let unit = runner()
            .spawn_unit(spec("leave", &[], dir.path(), env(&[dir.path()])))
            .await
            .expect("spawn");
        let _cleanup = Cleanup(unit.clone());
        let output = drain(unit.output()).await;
        let report = unit.wait_report().await.expect("report");
        assert_eq!(report.outcome.code, Some(0));
        assert_eq!(report.stop, None);
        assert!(report.left_running.is_empty());
        assert!(report.escaped.is_empty(), "{report:?}");
        let swept: Vec<ProcessIdentity> = report
            .swept
            .iter()
            .map(devlanes_sys::ProcessInfo::identity)
            .collect();
        assert!(
            report
                .swept
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case("fixture-server.exe")),
            "{report:?} {}",
            text(&output, OutputStream::Stdout)
        );
        assert!(!any_alive(&swept), "{swept:?}");
    })
    .await;
}

/// Dropping every handle leaves the unit running; daemon restarts rely on it.
#[tokio::test]
async fn dropping_the_handle_keeps_the_unit() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = runner()
            .spawn_unit(fixture_spec(&["--port", "0"], dir.path()))
            .await
            .expect("spawn");
        let identity = unit.identity();
        let _kill = Kill(vec![identity]);
        let mut output = unit.output();
        read_until(&mut output, "LISTENING").await;
        drop(output);
        drop(unit);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(devlanes_sys::matches(&identity).expect("lookup"));
    })
    .await;
}

/// The console helper that also appends a line to `log` for every event it
/// is started for.
fn counting_helper(log: &Path) -> ConsoleHelper {
    ConsoleHelper {
        program: std::env::current_exe().expect("test binary path"),
        args: vec![
            "--exact".into(),
            "helper_entry".into(),
            "--nocapture".into(),
            log.as_os_str().to_owned(),
            HELPER_ARG.into(),
        ],
    }
}

/// Starts `fixture-server` with `args` and a child and grandchild, waits
/// until all three run and returns the unit and their identities, the
/// leader first.
async fn fixture_tree(
    runner: &WindowsRunner,
    args: &[&str],
    cwd: &Path,
) -> (Arc<WindowsUnit>, Vec<ProcessIdentity>) {
    let mut args = args.to_vec();
    args.extend(["--port", "0", "--spawn-grandchild"]);
    let unit = runner
        .spawn_unit(fixture_spec(&args, cwd))
        .await
        .expect("spawn");
    let mut output = unit.output();
    read_until(&mut output, "grandchild pid=").await;
    let leader = unit.identity();
    let mut tree: Vec<ProcessIdentity> = unit
        .processes()
        .await
        .expect("processes")
        .iter()
        .map(devlanes_sys::ProcessInfo::identity)
        .collect();
    assert_eq!(tree.len(), 3, "{tree:?}");
    tree.sort_by_key(|p| p != &leader);
    (unit, tree)
}

/// An orphan stop asks each unit once (one helper, through the leader) with
/// `WM_CLOSE` and Ctrl-Break: a unit that exits on it is not terminated; one
/// that ignores it is terminated after the grace period, its unrecorded
/// child and grandchild found through their parents.
#[tokio::test]
async fn orphan_stops_ask_each_unit_once_then_terminate() {
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("helpers.log");
        let helper = counting_helper(&log);
        let runner = WindowsRunner::default().with_console_helper(helper.clone());
        let (polite, polite_tree) = fixture_tree(&runner, &[], dir.path()).await;
        let (stubborn, stubborn_tree) =
            fixture_tree(&runner, &["--ignore-sigterm"], dir.path()).await;
        let _cleanup = (Cleanup(polite.clone()), Cleanup(stubborn.clone()));
        let _kill = Kill([polite_tree.clone(), stubborn_tree.clone()].concat());

        // Every process of the polite unit was recorded, as a daemon would.
        let (leader, members) = (polite_tree[0], polite_tree[1..].to_vec());
        let polite_helper = helper.clone();
        let stop = tokio::task::spawn_blocking(move || {
            devlanes_runner_windows::stop_orphan(Some(&polite_helper), leader, &members, GRACE)
        })
        .await
        .expect("join");
        assert_eq!(stop.asked_through, Some(leader.pid), "{stop:?}");
        assert_eq!((stop.running, stop.terminated), (3, 0), "{stop:?}");
        assert!(!any_alive(&polite_tree), "{polite_tree:?}");

        // Only the stubborn unit's leader was recorded.
        let leader = stubborn_tree[0];
        let stop = tokio::task::spawn_blocking(move || {
            devlanes_runner_windows::stop_orphan(Some(&helper), leader, &[], Duration::from_secs(1))
        })
        .await
        .expect("join");
        assert_eq!(stop.asked_through, Some(leader.pid), "{stop:?}");
        assert_eq!((stop.running, stop.terminated), (3, 3), "{stop:?}");
        // Termination is asynchronous; the processes are gone a moment later.
        while any_alive(&stubborn_tree) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let helpers = std::fs::read_to_string(&log).expect("helper log");
        assert_eq!(helpers.lines().count(), 2, "{helpers}");
    })
    .await;
}

/// Asserts that `report` ended through an explicit stop with `signal`.
fn assert_stopped(report: &ExitReport, signal: StopSignal, escalated: bool) {
    let stop = report.stop.expect("stop outcome");
    assert_eq!(stop.signal, signal, "{report:?}");
    assert_eq!(stop.escalated, escalated, "{report:?}");
    assert!(report.left_running.is_empty(), "{report:?}");
    assert!(report.escaped.is_empty(), "{report:?}");
    // A Ctrl-Break that reached a default handler ends with this code; it
    // stays a code rather than becoming a signal.
    if report.outcome.code == Some(CONTROL_C_EXIT) {
        assert_eq!(report.outcome.signal, None);
    }
}
