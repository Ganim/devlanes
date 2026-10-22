//! `devlanes __agent-hook` as the agent plugin runs it: JSON on stdin, a JSON
//! deny on stdout and exit 0 to block, `DEVLANES_BLOCK_BACKGROUND=0` to turn it
//! off; and the plugin's command line, which must exit 0 in every shell an
//! agent runs it with, also when an older devlanes on `PATH` exits 2.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Runs `command` with `payload` on stdin and returns its exit status, stdout
/// and stderr. The child is killed if it outlives the deadline.
fn run_with_stdin(mut command: Command, payload: &str) -> (Option<i32>, String, String) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start the hook");
    let mut stdin = child.stdin.take().expect("stdin");
    // With no devlanes or an old one nothing reads stdin, and a shell that
    // exits before the write leaves a broken pipe. Whether the hook read its
    // input does not matter to the agent; the exit status and output do.
    match stdin.write_all(payload.as_bytes()) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            panic!("write payload: {error}")
        }
        _ => {}
    }
    drop(stdin);
    // Read on threads so a full pipe cannot stall the child.
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
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

/// Runs the hook on `payload` and returns its exit status, stdout and stderr.
fn run_hook(payload: &str, home: &Path, opt_out: bool) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devlanes"));
    command
        .arg("__agent-hook")
        .env("DEVLANES_HOME", home)
        .env_remove("DEVLANES_BLOCK_BACKGROUND");
    if opt_out {
        command.env("DEVLANES_BLOCK_BACKGROUND", "0");
    }
    run_with_stdin(command, payload)
}

/// Asserts that `stdout` is the deny both agents read, with the block message.
fn assert_denied(stdout: &str, context: &str) {
    let answer: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("{context}: not JSON ({error}): {stdout}"));
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

#[test]
fn agent_hook_blocks_through_the_binary_and_can_be_turned_off() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("devlanes.yaml"), "project: acme\n").expect("config");
    let home = dir.path().join("home");
    for tool in ["Bash", "PowerShell"] {
        let payload = serde_json::json!({
            "cwd": dir.path(),
            "tool_name": tool,
            "tool_input": {"command": "pnpm dev &"},
        })
        .to_string();
        let (code, stdout, stderr) = run_hook(&payload, &home, false);
        assert_eq!(code, Some(0), "{tool}: {stderr}");
        assert_denied(&stdout, tool);
        assert_eq!(
            run_hook(&payload, &home, true),
            (Some(0), String::new(), String::new())
        );
    }
    let allowed = serde_json::json!({
        "cwd": dir.path(),
        "tool_name": "Bash",
        "tool_input": {"command": "pnpm dev"},
    })
    .to_string();
    assert_eq!(
        run_hook(&allowed, &home, false),
        (Some(0), String::new(), String::new())
    );
    assert!(!home.exists(), "the hook must not create a devlanes home");
}

/// The `command` of the plugin's one hook, read from `agent-plugin/hooks/hooks.json`.
fn plugin_command() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agent-plugin/hooks/hooks.json");
    let hooks: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("hooks.json"))
            .expect("hooks.json is JSON");
    hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .expect("a command hook")
        .to_owned()
}

/// A directory with a fake `devlanes` that answers `__agent-hook` the way
/// 0.1.0 does: a usage error on stderr and exit 2.
fn old_devlanes() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.path().join("devlanes");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"error: unrecognized subcommand '$1'\" >&2\nexit 2\n",
        )
        .expect("fake devlanes");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }
    #[cfg(windows)]
    std::fs::write(
        dir.path().join("devlanes.cmd"),
        "@echo error: unrecognized subcommand '%1' 1>&2\r\n@exit /b 2\r\n",
    )
    .expect("fake devlanes");
    dir
}

/// `PATH` with `first` in front of the current one.
fn path_with(first: &Path) -> std::ffi::OsString {
    let rest = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(first.to_path_buf()).chain(std::env::split_paths(&rest)))
        .expect("PATH")
}

/// The first executable `name` (`name.exe` on Windows) on this process's
/// `PATH`. Windows' app execution aliases (`WindowsApps\pwsh.exe`) are reparse
/// points that only `symlink_metadata` reads.
fn on_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir()))
}

/// Git Bash, found the way Claude Code looks for it: next to the `git.exe` on
/// `PATH`, as `<Git>\bin\bash.exe`. `bash.exe` in `System32` is WSL's and is
/// never used.
#[cfg(windows)]
fn git_bash() -> Option<PathBuf> {
    let git = on_path("git")?;
    git.ancestors().skip(1).find_map(|dir| {
        let bash = dir.join("bin").join("bash.exe");
        (bash.is_file() && dir.join("usr").join("bin").is_dir()).then_some(bash)
    })
}

/// The shells the agents run a shell-form hook with on this OS, each running
/// `line`: `sh -c` (Claude Code), and on Windows `cmd.exe /C "<line>"` (Codex,
/// with its quoting) and Windows PowerShell 5.1 (Claude Code without Git Bash
/// or `pwsh`). Also, when installed: Git Bash and PowerShell 7 on Windows
/// (Claude Code), and `bash`, `zsh` and `fish` with `-lc` on Unix (Codex's
/// `$SHELL -lc`), whose login profiles read `home`. Absolute paths, because
/// the tests change `PATH`.
fn shells(line: &str, home: &Path) -> Vec<(String, Command)> {
    let mut shells = Vec::new();
    #[cfg(unix)]
    {
        let mut sh = Command::new("/bin/sh");
        sh.args(["-c", line]);
        shells.push(("sh -c".to_owned(), sh));
        for name in ["bash", "zsh", "fish"] {
            let Some(shell) = on_path(name) else { continue };
            let mut login = Command::new(shell);
            login
                .args(["-lc", line])
                .env("HOME", home)
                .env_remove("ZDOTDIR")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("BASH_ENV")
                .env_remove("ENV");
            shells.push((format!("{name} -lc"), login));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = home;
        let system =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
                .join("System32");
        let mut cmd = Command::new(system.join("cmd.exe"));
        cmd.arg("/C").raw_arg(format!("\"{line}\""));
        shells.push(("cmd.exe /C".to_owned(), cmd));
        let mut powershell = Command::new(system.join("WindowsPowerShell/v1.0/powershell.exe"));
        powershell.args(["-NoProfile", "-NonInteractive", "-Command", line]);
        shells.push(("powershell -Command".to_owned(), powershell));
        if let Some(pwsh) = on_path("pwsh") {
            let mut pwsh = Command::new(pwsh);
            pwsh.args(["-NoProfile", "-NonInteractive", "-Command", line]);
            shells.push(("pwsh -Command".to_owned(), pwsh));
        }
        // Git Bash runs `devlanes.exe` but not the old `devlanes.cmd`, so the
        // old case behaves like the missing one there.
        if let Some(bash) = git_bash() {
            let mut bash = Command::new(bash);
            bash.args(["-c", line]);
            shells.push(("Git Bash -c".to_owned(), bash));
        }
    }
    shells
}

/// A home whose login profiles set `PATH` from `DEVLANES_HOOK_TEST_PATH`, the
/// way an install's profile line puts `devlanes` on it: Debian's
/// `/etc/profile` replaces `PATH` for `bash -l`.
#[cfg(unix)]
fn login_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("home");
    let posix = "PATH=\"$DEVLANES_HOOK_TEST_PATH\"; export PATH\n";
    std::fs::write(home.path().join(".bash_profile"), posix).expect(".bash_profile");
    std::fs::write(home.path().join(".zprofile"), posix).expect(".zprofile");
    let fish = home.path().join(".config/fish");
    std::fs::create_dir_all(&fish).expect("fish config dir");
    std::fs::write(
        fish.join("config.fish"),
        "set -gx PATH (string split : -- $DEVLANES_HOOK_TEST_PATH)\n",
    )
    .expect("config.fish");
    home
}

/// No login profiles are read on Windows.
#[cfg(windows)]
fn login_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("home")
}

/// The plugin's command exits 0 in every shell [`shells`] finds, blocks
/// through the current binary, and lets everything through when an older
/// devlanes or none is on `PATH` (that devlanes would otherwise block every
/// call with its exit 2). Prints the shells it ran.
#[test]
fn plugin_command_masks_old_and_missing_devlanes_in_every_shell() {
    let line = plugin_command();
    let login = login_home();
    println!(
        "shells: {}",
        shells(&line, login.path())
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let project = tempfile::tempdir().expect("temp dir");
    std::fs::write(project.path().join("devlanes.yaml"), "project: acme\n").expect("config");
    let home = project.path().join("home");
    let payload = serde_json::json!({
        "cwd": project.path(),
        "tool_name": "Bash",
        "tool_input": {"command": "pnpm dev &"},
    })
    .to_string();
    let current = Path::new(env!("CARGO_BIN_EXE_devlanes"))
        .parent()
        .expect("binary directory")
        .to_path_buf();
    let old = old_devlanes();
    let none = tempfile::tempdir().expect("temp dir");
    // The old and missing cases get no other `PATH` entry: on Windows cargo
    // puts the build directory, with the current `devlanes.exe`, on `PATH`,
    // where Git Bash would find it past the old `devlanes.cmd`.
    for (devlanes, path) in [
        ("current", path_with(&current)),
        ("old", old.path().as_os_str().to_owned()),
        ("missing", none.path().as_os_str().to_owned()),
    ] {
        for (shell, mut command) in shells(&line, login.path()) {
            command
                .current_dir(project.path())
                .env("PATH", &path)
                .env("DEVLANES_HOOK_TEST_PATH", &path)
                .env("DEVLANES_HOME", &home)
                .env_remove("DEVLANES_BLOCK_BACKGROUND");
            let context = format!("{devlanes} devlanes, {shell}");
            let (code, stdout, stderr) = run_with_stdin(command, &payload);
            assert_eq!(code, Some(0), "{context}: {stderr}");
            if devlanes == "current" {
                assert_denied(&stdout, &context);
            } else {
                assert_eq!(stdout.trim(), "", "{context}");
            }
        }
    }
}
