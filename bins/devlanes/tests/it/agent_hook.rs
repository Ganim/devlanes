//! `devlanes __agent-hook` as the agent plugin runs it: the tool call as JSON
//! on stdin, a JSON deny on stdout and exit 0 to block. The plugin's hook
//! commands must also exit 0 with no output, in the shells Claude Code and
//! Codex run them with, when devlanes is missing or too old to know
//! `__agent-hook`.

use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Exit status, stdout and stderr of a finished process.
type Outcome = (Option<i32>, String, String);

/// Runs `command` with `payload` on stdin. The child is killed if it outlives
/// the deadline.
fn run_with_stdin(mut command: Command, payload: &str) -> Outcome {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start the hook");
    let mut stdin = child.stdin.take().expect("stdin");
    // Without a devlanes that reads it, the shell may exit before the write.
    match stdin.write_all(payload.as_bytes()) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            panic!("write payload: {error}")
        }
        _ => {}
    }
    drop(stdin);
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    // Read on threads so a full pipe cannot stall the child.
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the hook did not exit within 60 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (
        status.code(),
        out.join().expect("stdout reader"),
        err.join().expect("stderr reader"),
    )
}

/// A `tool` call of `command` in `cwd`.
fn payload(tool: &str, cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "session_id": "s",
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": {"command": command, "description": "x", "run_in_background": false},
    })
    .to_string()
}

/// A command that blocks only when read in the shell Codex uses on this OS:
/// Codex runs its `Bash` tool in PowerShell on Windows.
const CODEX_BLOCKED: &str = if cfg!(windows) {
    "Start-Process npm 'run dev'"
} else {
    "setsid -f pnpm dev"
};

/// Asserts that `stdout` is the PreToolUse deny with the devlanes message.
fn assert_denied(outcome: &Outcome, context: &str) {
    let (code, stdout, stderr) = outcome;
    assert_eq!(*code, Some(0), "{context}: {stderr}");
    let answer: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("{context}: not JSON ({error}): {stdout} {stderr}"));
    let output = &answer["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], "PreToolUse", "{context}");
    assert_eq!(output["permissionDecision"], "deny", "{context}");
    assert!(
        output["permissionDecisionReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("devlanes exec --name NAME")),
        "{context}: {stdout}"
    );
}

/// Asserts that the hook let the call through: exit 0 and nothing on stdout.
fn assert_allowed(outcome: &Outcome, context: &str) {
    let (code, stdout, stderr) = outcome;
    assert_eq!(*code, Some(0), "{context}: {stderr}");
    assert_eq!(stdout.trim(), "", "{context}");
}

/// A temp directory holding a devlanes project.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("devlanes.yaml"), "project: acme\n").expect("config");
    dir
}

#[test]
fn agent_hook_denies_through_the_binary() {
    let dir = project();
    let home = dir.path().join("home");
    let hook = |payload: &str, args: &[&str], opt_out: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devlanes"));
        command
            .arg("__agent-hook")
            .args(args)
            .env("DEVLANES_HOME", &home)
            .env_remove("DEVLANES_BLOCK_BACKGROUND");
        if opt_out {
            command.env("DEVLANES_BLOCK_BACKGROUND", "0");
        }
        run_with_stdin(command, payload)
    };
    let claude = ["--agent", "claude-code"];
    let codex = ["--agent", "codex"];
    let bash = payload("Bash", dir.path(), "pnpm dev &");
    let powershell = payload(
        "PowerShell",
        dir.path(),
        "Start-Process npm -ArgumentList 'run','dev'",
    );
    assert_denied(&hook(&bash, &claude, false), "Claude Code, Bash");
    assert_denied(
        &hook(&powershell, &claude, false),
        "Claude Code, PowerShell",
    );
    assert_denied(
        &hook(&payload("Bash", dir.path(), CODEX_BLOCKED), &codex, false),
        "Codex",
    );
    assert_allowed(&hook(&bash, &claude, true), "turned off");
    assert_allowed(&hook(&bash, &[], false), "no agent");
    assert_allowed(&hook(&bash, &["--agent", "cursor"], false), "other agent");
    assert_allowed(
        &hook(&powershell, &["--shell", "powershell"], false),
        "other arguments",
    );
    let foreground = payload("Bash", dir.path(), "pnpm dev");
    assert_allowed(&hook(&foreground, &claude, false), "foreground");
    assert_allowed(&hook("not json", &claude, false), "bad payload");
    assert!(!home.exists(), "the hook must not create a devlanes home");
}

/// A hook handler's `field` from one of the plugin's hook files.
fn plugin_command(file: &str, field: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agent-plugin/hooks")
        .join(file);
    let hooks: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("hook file"))
            .expect("hook file is JSON");
    let groups = hooks["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse hooks");
    assert_eq!(groups.len(), 1, "{file}: one PreToolUse hook");
    groups[0]["hooks"][0][field]
        .as_str()
        .unwrap_or_else(|| panic!("{file}: a {field}"))
        .to_owned()
}

/// Directories with a devlanes that answers `__agent-hook` the way a release
/// without it does (a usage error on stderr and exit 2), and with no devlanes.
fn old_and_missing() -> (tempfile::TempDir, tempfile::TempDir) {
    let old = tempfile::tempdir().expect("temp dir");
    let script = old.path().join("devlanes");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'error: unrecognized subcommand' >&2\nexit 2\n",
    )
    .expect("old devlanes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod old devlanes");
    }
    #[cfg(windows)]
    std::fs::write(
        old.path().join("devlanes.cmd"),
        "@echo error: unrecognized subcommand 1>&2\r\n@exit /b 2\r\n",
    )
    .expect("old devlanes.cmd");
    (old, tempfile::tempdir().expect("temp dir"))
}

/// Runs the hook command `shell` builds with `payload` and `PATH` set to the
/// directory of the devlanes under test, an old devlanes or none, followed by
/// `system`: it denies with the current devlanes and passes otherwise.
fn check_shell(
    name: &str,
    shell: impl Fn() -> Command,
    payload: &str,
    system: &[&Path],
    home: &Path,
) {
    let (old, missing) = old_and_missing();
    let current = Path::new(env!("CARGO_BIN_EXE_devlanes"))
        .parent()
        .expect("target dir");
    let run = |path_dir: &Path| {
        let mut child = shell();
        child
            .env(
                "PATH",
                std::env::join_paths(std::iter::once(path_dir).chain(system.iter().copied()))
                    .expect("PATH"),
            )
            .env_remove("DEVLANES_BLOCK_BACKGROUND")
            .env("DEVLANES_HOME", home);
        run_with_stdin(child, payload)
    };
    assert_denied(&run(current), &format!("{name}: current devlanes"));
    assert_allowed(&run(old.path()), &format!("{name}: old devlanes"));
    assert_allowed(&run(missing.path()), &format!("{name}: no devlanes"));
}

/// On macOS and Linux Claude Code runs hook commands with sh, and Codex with
/// the session's shell (`bash -c`, `zsh -c`) or `$SHELL -lc`.
#[cfg(unix)]
#[test]
fn plugin_commands_pass_with_a_missing_or_old_devlanes() {
    let dir = project();
    let home = dir.path().join("home");
    let system = [Path::new("/usr/bin"), Path::new("/bin")];
    let cases = [
        (
            "Claude Code",
            plugin_command("hooks.json", "command"),
            payload("Bash", dir.path(), "pnpm dev &"),
        ),
        (
            "Codex",
            plugin_command("codex.json", "command"),
            payload("Bash", dir.path(), CODEX_BLOCKED),
        ),
    ];
    for (agent, command, payload) in &cases {
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !Path::new(shell).exists() {
                continue;
            }
            let run = || {
                let mut child = Command::new(shell);
                child.arg("-c").arg(command);
                child
            };
            check_shell(&format!("{agent} in {shell}"), run, payload, &system, &home);
        }
    }
}

/// On Windows Claude Code runs hook commands with Git Bash, else PowerShell.
/// Codex runs `commandWindows` with the session's PowerShell (`-NoProfile
/// -Command`), or `cmd.exe /C "..."` when it has none.
#[cfg(windows)]
#[test]
fn plugin_commands_pass_with_a_missing_or_old_devlanes() {
    use std::os::windows::process::CommandExt;

    let dir = project();
    let home = dir.path().join("home");
    let system32 = Path::new(&std::env::var_os("SystemRoot").expect("SystemRoot")).join("System32");
    let powershell = system32.join(r"WindowsPowerShell\v1.0\powershell.exe");
    let system = [system32.as_path()];
    let in_powershell = |program: &Path, command: &str| {
        let mut child = Command::new(program);
        child.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        child
    };

    let claude = plugin_command("hooks.json", "command");
    let claude_payload = payload("Bash", dir.path(), "pnpm dev &");
    check_shell(
        "Claude Code in Windows PowerShell",
        || in_powershell(&powershell, &claude),
        &claude_payload,
        &system,
        &home,
    );
    let sh = git_sh();
    check_shell(
        "Claude Code in Git Bash",
        || {
            let mut child = Command::new(&sh);
            child.arg("-c").arg(&claude);
            child
        },
        &claude_payload,
        &system,
        &home,
    );

    let codex = plugin_command("codex.json", "commandWindows");
    let codex_payload = payload("Bash", dir.path(), CODEX_BLOCKED);
    check_shell(
        "Codex in Windows PowerShell",
        || in_powershell(&powershell, &codex),
        &codex_payload,
        &system,
        &home,
    );
    if let Some(pwsh) = pwsh() {
        check_shell(
            "Codex in PowerShell 7",
            || in_powershell(&pwsh, &codex),
            &codex_payload,
            &system,
            &home,
        );
    }
    check_shell(
        "Codex in cmd",
        || {
            // What Codex's hook runner starts when the session has no shell.
            let mut child = Command::new(system32.join("cmd.exe"));
            child.arg("/C").raw_arg(format!("\"{codex}\""));
            child
        },
        &codex_payload,
        &system,
        &home,
    );
}

/// Git for Windows' `sh.exe`, three levels above `git --exec-path`.
#[cfg(windows)]
fn git_sh() -> std::path::PathBuf {
    let mut git = Command::new("git");
    git.arg("--exec-path");
    let (code, stdout, stderr) = run_with_stdin(git, "");
    assert_eq!(code, Some(0), "git --exec-path: {stderr}");
    Path::new(stdout.trim())
        .ancestors()
        .nth(3)
        .expect("Git's root")
        .join(r"usr\bin\sh.exe")
}

/// PowerShell 7's `pwsh.exe` on `PATH`, when it is installed.
#[cfg(windows)]
fn pwsh() -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("pwsh.exe"))
        .find(|path| path.exists())
}
