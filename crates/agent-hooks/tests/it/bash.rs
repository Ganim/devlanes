//! The Bash reader, through Claude Code's `Bash` tool: the cases the plugin's
//! former `block-background.sh` was tested with, and a few more.

use std::path::Path;

use devlanes_agent_hooks::{Agent, Verdict, decide};

use crate::{payload, workspace};

/// The verdict for `command` run from `cwd` by Claude Code's Bash tool.
fn verdict(cwd: &Path, home: &Path, command: &str, background: bool) -> Verdict {
    decide(
        &payload("Bash", cwd, command, background),
        Agent::ClaudeCode,
        cfg!(windows),
        Path::new("/unused"),
        Some(home),
    )
}

/// `path` with forward slashes, which Bash keeps as they are.
fn slashed(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[test]
fn blocks_backgrounded_dev_servers_in_bash() {
    let tmp = workspace();
    let project = tmp.path().join("project/web");
    let other = tmp.path().join("other");
    let home = tmp.path().join("project");
    for command in [
        "pnpm dev &",
        "npm run dev &",
        "nohup pnpm dev > dev.log 2>&1 &",
        "pnpm dev &>/dev/null &",
        "npm start & sleep 5 && curl localhost:3000",
        "pnpm dev | tee dev.log &",
        "python -m http.server 8000 &",
        "node server.js &",
        "vite --port 5173 &",
        "pnpm dev & disown",
        "setsid -f pnpm dev",
        "setsid --fork pnpm dev",
        "tmux new -d \"pnpm dev\"",
        "tmux new-session -ds web npm start",
        "screen -dmS web pnpm dev",
        "PORT=3001 pnpm dev &",
        "env PORT=3001 pnpm dev &",
        "echo \"a & b\" && pnpm dev &",
        "(pnpm dev &)",
        "tsx --watch src/index.ts &",
        "\"vite\" &",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Block,
            "{command}"
        );
    }
    assert_eq!(
        verdict(&project, &home, "pnpm dev\nsleep 3", true),
        Verdict::Block,
        "run_in_background"
    );
    for command in [
        format!("cd {} && pnpm dev &", slashed(&tmp.path().join("project"))),
        "cd ../project/web && pnpm dev &".to_owned(),
        "cd '../project' && pnpm dev &".to_owned(),
        "cd ~ && pnpm dev &".to_owned(),
        "cd && pnpm dev &".to_owned(),
        "cd ~/web; pnpm dev &".to_owned(),
    ] {
        assert_eq!(
            verdict(&other, &home, &command, false),
            Verdict::Block,
            "{command}"
        );
    }
}

#[test]
fn allows_foreground_one_shot_and_quoted_commands_in_bash() {
    let tmp = workspace();
    let project = tmp.path().join("project/web");
    let other = tmp.path().join("other");
    let home = tmp.path().join("project");
    for command in [
        "pnpm dev",
        "pnpm build && pnpm test",
        "sleep 10 >/dev/null &",
        "tail -f dev.log &",
        "tail -f /var/log/server.log &",
        "echo \"pnpm dev &\"",
        "echo 'npm start &'",
        "echo pnpm dev \\&",
        "curl \"https://dev.example.invalid/?a=1&b=2\"",
        "cargo build 2>&1 | tail",
        "pnpm dev &>/dev/null",
        "vite build &",
        "setsid pnpm dev",
        "tmux new \"pnpm dev\"",
        "tmux attach -d",
        "screen -S web pnpm dev",
        "devlanes exec --name web -- pnpm dev",
        "devlanes logs web -f --timeout 30s &",
        "nohup devlanes logs web -f &",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Allow,
            "{command}"
        );
    }
    assert_eq!(
        verdict(&project, &home, "pnpm test", true),
        Verdict::Allow,
        "one-shot in the background"
    );
    assert_eq!(
        verdict(&project, &home, "devlanes start", true),
        Verdict::Allow,
        "devlanes in the background"
    );
    for command in [
        "pnpm dev &".to_owned(),
        format!("cd {} && pnpm dev &", slashed(&other)),
        "cd ../project; cd ../other; pnpm dev &".to_owned(),
        "cd - && pnpm dev &".to_owned(),
    ] {
        assert_eq!(
            verdict(&other, &home, &command, false),
            Verdict::Allow,
            "{command}"
        );
    }
    assert_eq!(
        verdict(
            &project,
            &home,
            &format!("cd {} && pnpm dev &", slashed(&other)),
            false
        ),
        Verdict::Allow,
        "cd out of the project"
    );
}

/// Claude Code runs its Bash tool in Git Bash on Windows, where `cd /c/...`
/// names drive C.
#[cfg(windows)]
#[test]
fn reads_git_bash_drive_paths_on_windows() {
    let tmp = workspace();
    let other = tmp.path().join("other");
    let project = slashed(&tmp.path().join("project"));
    let (drive, rest) = project.split_once(":/").expect("a drive path");
    let command = format!("cd /{}/{rest} && pnpm dev &", drive.to_ascii_lowercase());
    assert_eq!(
        verdict(&other, tmp.path(), &command, false),
        Verdict::Block,
        "{command}"
    );
}
