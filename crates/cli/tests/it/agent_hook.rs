//! The agent plugin's `PreToolUse` hook (`devlanes __agent-hook`): which shell
//! commands it blocks, for Claude Code's Bash and PowerShell tools and Codex.

use std::path::{Path, PathBuf};

use devlanes_cli::agent_hook::{Verdict, decide};
use serde_json::json;

/// A temporary project (`project/devlanes.yaml`, `project/sub`) next to a
/// directory outside it (`other`).
struct Lab {
    _root: tempfile::TempDir,
    root: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().to_path_buf();
        std::fs::create_dir_all(path.join("project/sub")).expect("project dir");
        std::fs::create_dir_all(path.join("other")).expect("other dir");
        std::fs::write(path.join("project/devlanes.yaml"), "project: acme\n").expect("config");
        Self {
            _root: root,
            root: path,
        }
    }

    fn sub(&self) -> PathBuf {
        self.root.join("project/sub")
    }

    fn other(&self) -> PathBuf {
        self.root.join("other")
    }

    /// The root as a shell would type it: forward slashes, which Git Bash and
    /// PowerShell both accept.
    fn typed(&self) -> String {
        self.root.display().to_string().replace('\\', "/")
    }
}

/// The verdict for one tool call of `tool` in `dir`.
fn verdict(tool: &str, dir: &Path, command: &str, background: bool) -> Verdict {
    let payload = json!({
        "session_id": "s",
        "cwd": dir,
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": {
            "command": command,
            "description": "runs \"command\": x",
            "run_in_background": background,
        },
    });
    decide(&payload.to_string(), Path::new("/unused"), None)
}

/// Asserts every `(expected, dir, command, run_in_background)` case for `tool`.
fn check(tool: &str, cases: &[(Verdict, &Path, &str, bool)]) {
    let failures: Vec<String> = cases
        .iter()
        .filter(|(expected, dir, command, background)| {
            verdict(tool, dir, command, *background) != *expected
        })
        .map(|(expected, dir, command, _)| {
            format!("expected {expected:?} in {}: {command}", dir.display())
        })
        .collect();
    assert!(failures.is_empty(), "{tool}:\n{}", failures.join("\n"));
}

use Verdict::{Allow, Block};

/// The cases of the shell script this hook replaced, unchanged.
#[test]
fn bash_backgrounding_is_blocked_in_projects_only() {
    let lab = Lab::new();
    let (p, o) = (lab.sub(), lab.other());
    let (p, o) = (p.as_path(), o.as_path());
    let into_project = format!("cd {}/project && pnpm dev &", lab.typed());
    let out_of_project = format!("cd {}/other && pnpm dev &", lab.typed());
    check(
        "Bash",
        &[
            (Block, p, "pnpm dev &", false),
            (Block, p, "npm run dev &", false),
            (Block, p, "nohup pnpm dev > dev.log 2>&1 &", false),
            (Block, p, "pnpm dev &>/dev/null &", false),
            (
                Block,
                p,
                "npm start & sleep 5 && curl localhost:3000",
                false,
            ),
            (Block, p, "pnpm dev | tee dev.log &", false),
            (Block, p, "python -m http.server 8000 &", false),
            (Block, p, "node server.js &", false),
            (Block, p, "vite --port 5173 &", false),
            (Block, p, "pnpm dev & disown", false),
            (Block, p, "setsid -f pnpm dev", false),
            (Block, p, "tmux new -d \"pnpm dev\"", false),
            (Block, p, "tmux new-session -ds web npm start", false),
            (Block, p, "screen -dmS web pnpm dev", false),
            (Block, p, "PORT=3001 pnpm dev &", false),
            (Block, p, "pnpm dev\nsleep 3", true),
            (Block, o, &into_project, false),
            (Block, o, "cd ../project/sub && pnpm dev &", false),
            (Block, p, "echo \"a & b\" && pnpm dev &", false),
            (Allow, p, "pnpm dev", false),
            (Allow, p, "pnpm build && pnpm test", false),
            (Allow, p, "pnpm test", true),
            (Allow, p, "sleep 10 >/dev/null &", false),
            (Allow, p, "tail -f dev.log &", false),
            (Allow, p, "tail -f /var/log/server.log &", false),
            (Allow, p, "echo \"pnpm dev &\"", false),
            (Allow, p, "echo 'npm start &'", false),
            (
                Allow,
                p,
                "curl \"https://dev.example.invalid/?a=1&b=2\"",
                false,
            ),
            (Allow, p, "cargo build 2>&1 | tail", false),
            (Allow, p, "pnpm dev &>/dev/null", false),
            (Allow, p, "vite build &", false),
            (Allow, p, "setsid pnpm dev", false),
            (Allow, p, "devlanes exec --name web -- pnpm dev", false),
            (Allow, p, "devlanes logs web -f --timeout 30s &", false),
            (Allow, p, "devlanes start", true),
            (Allow, o, "pnpm dev &", false),
            (Allow, p, &out_of_project, false),
        ],
    );
}

/// Claude Code's PowerShell tool: PowerShell 7's trailing `&`, `Start-Process`
/// and `Start-Job`, with `-Wait`, URLs and the call operator passing.
#[test]
fn powershell_backgrounding_is_blocked_in_projects_only() {
    let lab = Lab::new();
    let (p, o) = (lab.sub(), lab.other());
    let (p, o) = (p.as_path(), o.as_path());
    let into_project = format!("Set-Location '{}/project'; pnpm dev &", lab.typed());
    let working_directory = format!(
        "Start-Process pnpm -ArgumentList dev -WorkingDirectory '{}/project'",
        lab.typed()
    );
    check(
        "PowerShell",
        &[
            (Block, p, "pnpm dev &", false),
            (Block, p, "npm run dev *> dev.log &", false),
            (Block, p, "& pnpm dev &", false),
            (Block, p, "Start-Process pnpm dev", false),
            (
                Block,
                p,
                "Start-Process pnpm -ArgumentList 'run','dev'",
                false,
            ),
            (
                Block,
                p,
                "Start-Process -FilePath npm -ArgumentList \"run dev\" -WindowStyle Hidden",
                false,
            ),
            (Block, p, "start-process -NoNewWindow node server.js", false),
            (Block, p, "Start-Process -FilePath:vite -PassThru", false),
            (Block, p, "saps pnpm start", false),
            (Block, p, "start pnpm dev", false),
            (Block, p, "Start-Job { pnpm dev }", false),
            (
                Block,
                p,
                "$job = Start-Job -ScriptBlock { npm run dev; } -Name web",
                false,
            ),
            (Block, p, "Start-ThreadJob { pnpm dev }", false),
            (Block, p, "cmd /c start pnpm dev", false),
            (Block, p, "cmd.exe /d /c \"start /b npm start\"", false),
            (Block, p, "$env:PORT = '3001'; pnpm dev &", false),
            (
                Block,
                p,
                "& 'C:/tools/node_modules/.bin/vite.cmd' --port 5173 &",
                false,
            ),
            (Block, p, "pnpm dev", true),
            (Block, o, &into_project, false),
            (Block, o, &working_directory, false),
            (Allow, p, "pnpm dev", false),
            (Allow, p, "& pnpm dev", false),
            (Allow, p, "pnpm build; pnpm test", false),
            (Allow, p, "npm run build 2>&1 | Out-Null", false),
            (Allow, p, "Start-Process pnpm dev -Wait", false),
            (Allow, p, "start /wait pnpm dev", false),
            (
                Allow,
                p,
                "Start-Process https://web.acme-2.localhost",
                false,
            ),
            (Allow, p, "start http://localhost:5173/dev", false),
            (Allow, p, "Start-Process code .", false),
            (Allow, p, "Start-Process powershell -Verb RunAs", false),
            (Allow, p, "Start-Job -FilePath ./build.ps1", false),
            (Allow, p, "Start-Process vite -ArgumentList build", false),
            (Allow, p, "Write-Output 'pnpm dev &'", false),
            (Allow, p, "Write-Output \"Start-Process pnpm dev\"", false),
            (
                Allow,
                p,
                "Start-Process devlanes -ArgumentList 'logs','web','-f'",
                false,
            ),
            (Allow, p, "devlanes exec --name web -- pnpm dev", false),
            (Allow, o, "Start-Process pnpm dev", false),
            (Allow, o, "pnpm dev &", false),
        ],
    );
}

/// Script blocks, nested shells, `iex`, comments, here-strings and the smaller
/// PowerShell forms, for the PowerShell tool.
#[test]
fn powershell_script_blocks_nested_shells_and_comments() {
    let lab = Lab::new();
    let (p, o) = (lab.sub(), lab.other());
    let (p, o) = (p.as_path(), o.as_path());
    let typed = lab.typed();
    let job_into = format!("Start-Job {{ Set-Location '{typed}/project'; pnpm dev }}");
    let job_out = format!("Start-Job {{ Set-Location '{typed}/other'; pnpm dev }}");
    let job_stays = format!("Start-Job {{ Set-Location '{typed}/project' }}; pnpm dev &");
    let pwsh_wd = format!("pwsh -wd '{typed}/project' -c \"Start-Process pnpm dev\"");
    let start_into = format!("start /d {typed}/project pnpm dev");
    let start_out = format!("start /d {typed}/other pnpm dev");
    check(
        "PowerShell",
        &[
            (Block, o, &job_into, false),
            (Block, p, "Start-Job { & pnpm dev }", false),
            (
                Block,
                p,
                "$job = Start-Job { npm run dev | Out-File dev.log }",
                false,
            ),
            (Block, p, "$job=Start-Job { pnpm dev }", false),
            (
                Block,
                p,
                "Start-Job -Name web -ScriptBlock {\n  Set-Location ..\n  pnpm dev\n}",
                false,
            ),
            (
                Block,
                p,
                "powershell -NoProfile -ExecutionPolicy Bypass -Command \"Start-Process pnpm dev\"",
                false,
            ),
            (
                Block,
                p,
                "pwsh -nop -ep Bypass -w Hidden -c \"& { Start-Process pnpm dev }\"",
                false,
            ),
            (
                Block,
                p,
                "powershell.exe -ExecutionPolicy:Bypass -c Start-Process pnpm dev",
                false,
            ),
            (Block, o, &pwsh_wd, false),
            (Block, p, "iex \"Start-Process pnpm dev\"", false),
            (Block, p, "Invoke-Expression -Command 'pnpm dev &'", false),
            (Block, p, "& { Start-Process pnpm dev }", false),
            (
                Block,
                p,
                "Start-Process pnpm -ArgumentList @('run','dev')",
                false,
            ),
            (Block, p, "Start-Process pnpm dev -Wait:$false", false),
            (Block, o, &start_into, false),
            (Block, p, "pnpm dev & # start in the background", false),
            (Block, p, "<# a comment #> pnpm dev &", false),
            (Allow, p, &job_out, false),
            (Allow, o, &job_stays, false),
            (Allow, p, "Start-Job { pnpm build; pnpm test }", false),
            (Allow, p, "pwsh -EncodedCommand ZQBjAGgAbwA=", false),
            (
                Allow,
                p,
                "powershell -ExecutionPolicy Bypass -Command \"Start-Process pnpm dev -Wait\"",
                false,
            ),
            (Allow, p, "Start-Process pnpm dev -Wait:$true", false),
            (Allow, p, &start_out, false),
            (Allow, p, "$m = @'\nDon't start & stop\n'@", false),
            (
                Allow,
                p,
                "$m = @\"\npnpm dev &\n\"@; Write-Output $m",
                false,
            ),
            (Allow, p, "pnpm build # then pnpm dev &", false),
            (Allow, p, "<# Start-Process pnpm dev #> pnpm build", false),
            (Allow, p, "Start-Process code server.js", false),
        ],
    );
}

/// `` ` `` still escapes inside a double-quoted Windows path, `#` starts a
/// comment right after `{`, `(` and `|`, and `iex` runs in the caller's
/// scope, so its `Set-Location` moves the commands after it.
#[test]
fn powershell_escapes_comments_and_iex_scope() {
    let lab = Lab::new();
    let (p, o) = (lab.sub(), lab.other());
    let (p, o) = (p.as_path(), o.as_path());
    let typed = lab.typed();
    let iex_into = format!("iex 'cd {typed}/project'; pnpm dev &");
    let invoke_into = format!("Invoke-Expression \"Set-Location {typed}/project\"; pnpm dev &");
    let iex_out = format!("iex 'cd {typed}/other'; pnpm dev &");
    check(
        "PowerShell",
        &[
            (Block, o, &iex_into, false),
            (Block, o, &invoke_into, false),
            (Block, p, "& {# start it\n Start-Process pnpm dev }", false),
            (Allow, p, &iex_out, false),
            (Allow, p, "Write-Output \"C:\\acme`\"; pnpm dev &\"", false),
            (
                Allow,
                p,
                "& {# don't\n pnpm build }; Write-Output 'pnpm dev &'",
                false,
            ),
            (
                Allow,
                p,
                "(# don't\n pnpm build); Write-Output 'pnpm dev &'",
                false,
            ),
            (
                Allow,
                p,
                "Start-Job { pnpm build |# don't\n Out-Null }; Write-Output 'pnpm dev &'",
                false,
            ),
        ],
    );
}

/// Editors whose names mean other programs elsewhere count only on their OS:
/// Nix's `devenv up` starts services, and Linux `open` (Debian's `openvt`)
/// runs the command it is given.
#[test]
fn os_specific_editors_count_only_on_their_os() {
    let lab = Lab::new();
    let p = lab.sub();
    let p = p.as_path();
    let editor = |is: bool| if is { Allow } else { Block };
    let windows = cfg!(windows);
    let macos = cfg!(target_os = "macos");
    for tool in ["Bash", "PowerShell"] {
        check(
            tool,
            &[
                (editor(windows), p, "devenv up &", false),
                (editor(windows), p, "notepad server.js &", false),
                (editor(macos), p, "open server.js &", false),
                (Allow, p, "code server.js &", false),
            ],
        );
    }
    check(
        "PowerShell",
        &[(editor(windows), p, "Start-Process notepad server.js", false)],
    );
}

/// Editors started in the background open a file, they don't serve it.
#[test]
fn bash_editors_in_the_background_pass() {
    let lab = Lab::new();
    let p = lab.sub();
    check(
        "Bash",
        &[
            (Allow, &p, "code server.js &", false),
            (Allow, &p, "cursor server.ts &", false),
            (Block, &p, "node server.js &", false),
        ],
    );
}

/// Windows `Bash` calls also carry PowerShell (Codex runs PowerShell but names
/// the tool `Bash`) and Git Bash's `start`, `/c/...` paths and `.cmd` shims.
#[cfg(windows)]
#[test]
fn windows_bash_calls_also_check_windows_launchers() {
    let lab = Lab::new();
    let (p, o) = (lab.sub(), lab.other());
    let (p, o) = (p.as_path(), o.as_path());
    let typed = lab.typed();
    let (drive, rest) = typed.split_once(":/").expect("a drive path");
    let msys = format!("cd /{}/{rest}/project && pnpm dev &", drive.to_lowercase());
    check(
        "Bash",
        &[
            (Block, p, "Start-Process pnpm dev", false),
            (Block, p, "Start-Job { pnpm dev }", false),
            (Block, p, "start pnpm dev", false),
            (Block, p, "cmd //c start pnpm dev", false),
            (
                Block,
                p,
                "pwsh -NoProfile -Command \"Start-Process pnpm dev\"",
                false,
            ),
            (Block, p, "node_modules/.bin/vite.cmd &", false),
            (Block, o, &msys, false),
            (Allow, p, "start https://web.acme-2.localhost", false),
            (Allow, p, "Start-Process pnpm dev -Wait", false),
            (Allow, p, "devlanes.exe logs web -f &", false),
        ],
    );
}

/// What Codex sends as `Bash` on Windows is often PowerShell: assignments, the
/// call operator, script blocks, nested shells and `\` paths in double quotes
/// get the same verdicts under both tool names.
#[cfg(windows)]
#[test]
fn windows_powershell_text_gets_the_same_verdict_from_both_tools() {
    let lab = Lab::new();
    let (p, o) = (lab.sub(), lab.other());
    let (p, o) = (p.as_path(), o.as_path());
    let native = lab.root.display().to_string();
    let typed = lab.typed();
    let cd_into = format!("cd \"{native}\\project\\\"; pnpm dev &");
    let cd_out = format!("cd \"{native}\\other\\\"; pnpm dev &");
    let job_into = format!("Start-Job {{ Set-Location {native}\\project; pnpm dev }}");
    let start_into = format!("start /d {native}\\project pnpm dev");
    let cases: &[(Verdict, &Path, &str, bool)] = &[
        (Block, o, &cd_into, false),
        (Block, o, &job_into, false),
        (Block, o, &start_into, false),
        (Block, p, "$job = Start-Job { pnpm dev }", false),
        (Block, p, "$job=Start-Job { pnpm dev }", false),
        (Block, p, "& pnpm dev &", false),
        (Block, p, "Start-Job { & pnpm dev }", false),
        (
            Block,
            p,
            "powershell -NoProfile -ExecutionPolicy Bypass -Command \"Start-Process pnpm dev\"",
            false,
        ),
        (
            Block,
            p,
            "Start-Process pnpm -ArgumentList @('run','dev')",
            false,
        ),
        (Block, p, "Start-Process pnpm dev -Wait:$false", false),
        (Allow, p, &cd_out, false),
        (Allow, p, "$m = @'\nDon't start & stop\n'@", false),
        (Allow, p, "Start-Process code server.js", false),
        (Allow, p, "& pnpm dev", false),
    ];
    check("PowerShell", cases);
    check("Bash", cases);
    // Git Bash text keeps POSIX quoting: `\"` inside an ordinary string escapes,
    // and PowerShell-looking text inside quotes does not make a call PowerShell.
    check(
        "Bash",
        &[
            (Allow, p, "echo \"say \\\"pnpm dev &\\\" later\"", false),
            (
                Allow,
                p,
                "git commit -m \"Fix Get-Started: say \\\"pnpm dev &\\\" later\"",
                false,
            ),
            (
                Allow,
                p,
                "awk '{ n++; $2 = \"x\"; print }' f; echo \"say \\\"pnpm dev &\\\" later\"",
                false,
            ),
            (
                Block,
                o,
                &format!("cd {typed}/project && pnpm dev &"),
                false,
            ),
        ],
    );
}

/// Unix `Bash` calls stay POSIX: Windows launcher names are ordinary commands.
#[cfg(unix)]
#[test]
fn unix_bash_calls_ignore_windows_launchers() {
    let lab = Lab::new();
    let p = lab.sub();
    check(
        "Bash",
        &[
            (Allow, &p, "Start-Process pnpm dev", false),
            (Allow, &p, "start pnpm dev", false),
        ],
    );
}

/// Codex sends no `run_in_background`, and payloads the hook cannot read pass.
#[test]
fn incomplete_payloads_pass() {
    let lab = Lab::new();
    let dir = lab.sub();
    let codex = json!({
        "session_id": "s",
        "turn_id": "t",
        "cwd": dir,
        "hook_event_name": "PreToolUse",
        "model": "m",
        "tool_name": "Bash",
        "tool_use_id": "u",
        "tool_input": {"command": "pnpm dev &"},
    });
    assert_eq!(
        decide(&codex.to_string(), Path::new("/unused"), None),
        Block
    );
    let no_cwd = json!({"tool_name": "Bash", "tool_input": {"command": "pnpm dev &"}});
    assert_eq!(decide(&no_cwd.to_string(), &dir, None), Block);
    for payload in ["", "not json", "{}", r#"{"tool_input": {"command": 3}}"#] {
        assert_eq!(decide(payload, &dir, None), Allow, "{payload}");
    }
}

/// `cd ~` resolves against the home the caller passes.
#[test]
fn cd_home_moves_into_the_home_directory() {
    let lab = Lab::new();
    let home = lab.root.join("project");
    let payload = json!({
        "cwd": lab.other(),
        "tool_name": "Bash",
        "tool_input": {"command": "cd ~/sub && pnpm dev &"},
    });
    assert_eq!(
        decide(&payload.to_string(), Path::new("/unused"), Some(&home)),
        Block
    );
}
