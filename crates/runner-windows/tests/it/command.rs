//! `command::build` against real Windows: the command lines it builds reach
//! programs with the intended arguments, and resolution agrees with `where.exe`.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Read,
    os::windows::process::CommandExt,
    process::Stdio,
    time::{Duration, Instant},
};

use devlanes_runner_windows::command;

/// Set when [`argv_round_trips`] starts this test binary to print its argv.
const CHILD: &str = "DEVLANES_ARGV_CHILD";

/// Longest a started program may run.
const TIMEOUT: Duration = Duration::from_secs(30);

/// This process's environment, as a unit would get it.
fn current_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// `%SystemRoot%\System32\cmd.exe`.
fn cmd_exe() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    format!(r"{root}\System32\cmd.exe")
}

/// Starts `built` with std the way the runner would and returns its stdout;
/// `extra` is added to the inherited environment. Killed after [`TIMEOUT`].
fn run(built: &command::Command, extra: &[(&str, &str)]) -> String {
    let mut process = std::process::Command::new(&built.application);
    if !built.arguments().is_empty() {
        process.raw_arg(built.arguments());
    }
    let mut child = process
        .envs(extra.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + TIMEOUT;
    while child.try_wait().expect("wait").is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{} timed out", built.application.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let bytes = reader.join().expect("reader").expect("read stdout");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
/// Only does something when [`argv_round_trips`] starts it: prints each
/// argument after `--` as Rust parsed it from the command line.
fn argv_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let args: Vec<OsString> = std::env::args_os().collect();
    let start = args
        .iter()
        .position(|arg| arg == "--")
        .map_or(args.len(), |at| at + 1);
    for arg in &args[start..] {
        println!("ARG {arg:?}");
    }
}

#[test]
/// Tricky arguments reach a program unchanged: this test binary runs
/// [`argv_child`] (libtest takes the arguments after `--` as extra filters,
/// which match no test with `--exact`).
fn argv_round_trips() {
    let exe = std::env::current_exe().expect("test binary");
    let tricky = [
        "",
        "plain",
        "a b",
        r#"a"b"#,
        r#"""#,
        r"trailing\",
        r"a b\",
        r"a b\\",
        r#"a\"b"#,
        r#"say "hi" now"#,
        r"\\server\share\",
        "tab\there",
        "ünïcode",
        "%PATH%",
        "&|<>^",
        "--flag=x y",
    ];
    let mut args: Vec<OsString> = ["--exact", "command::argv_child", "--nocapture", "--"]
        .map(OsString::from)
        .into();
    args.extend(tricky.iter().map(OsString::from));
    let built = command::build(
        exe.as_os_str(),
        &args,
        &BTreeMap::new(),
        &std::env::temp_dir(),
    )
    .expect("built");
    let out = run(&built, &[(CHILD, "1")]);
    let seen: Vec<&str> = out
        .lines()
        .filter(|line| line.starts_with("ARG "))
        .collect();
    let expected: Vec<String> = tricky.iter().map(|arg| format!("ARG {arg:?}")).collect();
    assert_eq!(seen, expected, "{out}");
}

#[test]
/// The cmd shell's command text reaches cmd.exe as written, quotes and
/// operators included.
fn cmd_shell_text_runs_verbatim() {
    let args = ["/d", "/s", "/c", r#"echo "a b"&echo x^&y&echo 100%"#].map(OsString::from);
    let built = command::build(
        cmd_exe().as_ref(),
        &args,
        &current_env(),
        &std::env::temp_dir(),
    )
    .expect("built");
    let out = run(&built, &[]);
    assert_eq!(out.lines().collect::<Vec<_>>(), [r#""a b""#, "x&y", "100%"]);
}

#[test]
/// A batch file found through `PATHEXT` gets its arguments without cmd.exe
/// expanding variables or running operators in them.
fn batch_arguments_are_not_interpreted() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("show.cmd"),
        "@echo off\r\necho [%*]\r\necho [%~1]\r\n",
    )
    .expect("script");
    let mut env = current_env();
    env.retain(|name, _| !name.eq_ignore_ascii_case("PATH"));
    env.insert("Path".to_owned(), dir.path().display().to_string());
    let args = ["%PATH%", "a b", "x&echo injected"].map(OsString::from);
    let built = command::build("show".as_ref(), &args, &env, dir.path()).expect("built");
    assert!(
        built
            .application
            .to_string_lossy()
            .eq_ignore_ascii_case(&cmd_exe())
    );
    let out = run(&built, &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [r#"["%PATH%" "a b" "x&echo injected"]"#, "[%PATH%]"],
        "{out}"
    );
}

#[test]
/// Resolution finds what `where.exe` finds on this machine's `PATH`.
fn resolution_agrees_with_where() {
    let env = current_env();
    let cwd = std::env::temp_dir();
    for name in ["cmd", "where"] {
        let built = command::build(name.as_ref(), &[], &env, &cwd).expect("built");
        let lookup = command::build("where".as_ref(), &[OsString::from(name)], &env, &cwd)
            .expect("where.exe");
        let found = run(&lookup, &[]);
        let first = found.lines().next().unwrap_or_default();
        assert!(
            first.eq_ignore_ascii_case(&built.application.to_string_lossy()),
            "{name}: where.exe {first:?}, built {:?}",
            built.application
        );
    }
}
