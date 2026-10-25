//! The PowerShell reader, through Claude Code's `PowerShell` tool.

use std::path::Path;

use devlanes_agent_hooks::{Agent, Verdict, decide};

use crate::{payload, workspace};

/// The verdict for `command` run from `cwd` by Claude Code's PowerShell tool.
fn verdict(cwd: &Path, home: &Path, command: &str, background: bool) -> Verdict {
    decide(
        &payload("PowerShell", cwd, command, background),
        Agent::ClaudeCode,
        cfg!(windows),
        Path::new("/unused"),
        Some(home),
    )
}

#[test]
fn blocks_backgrounded_dev_servers_in_powershell() {
    let tmp = workspace();
    let project = tmp.path().join("project/web");
    let home = tmp.path().join("project");
    for command in [
        "pnpm dev &",
        "npm run dev & Start-Sleep 5; curl http://localhost:5173",
        "pnpm dev | Tee-Object dev.log &",
        "pnpm dev 2>&1 &",
        "& 'C:\\tools\\vite.cmd' --port 5173 &",
        "$job = pnpm dev &",
        "Start-Process npm -ArgumentList 'run','dev'",
        "Start-Process -FilePath pnpm -ArgumentList \"dev\" -NoNewWindow -PassThru",
        "start npm 'run dev'",
        "saps node server.js",
        "Start-Process python -ArgumentList '-m','http.server','8000' -RedirectStandardOutput out.log",
        "Start-Process pwsh -ArgumentList '-NoExit','-Command','pnpm dev'",
        "Start-Job { pnpm dev }",
        "$job = Start-Job -ScriptBlock { Set-Location web; npm start }",
        "Start-ThreadJob -ScriptBlock { vite }",
        "sajb { & pnpm dev }",
        "cmd /c start pnpm dev",
        "cmd.exe /c \"start /b npm run dev\"",
        "cmd /c start \"\" /d web pnpm dev",
        "cmd /c start \"Dev Server\" pnpm dev",
        "cmd /c 'start \"Dev\" pnpm dev'",
        "if ($true) { pnpm dev & }",
        "Write-Output 'a & b'; pnpm dev &",
        "# start the server\npnpm dev &",
        "<# a comment with { #> pnpm dev &",
        "pnpm `\n  dev &",
        "Start-Process npm -ArgumentList 'run','dev' -Wait:$false",
        "Start-Process -ErrorAction Stop vite",
        "Start-Process -ea:Stop -OutVariable p vite",
        "& \"C:\\Program Files\\nodejs\\npm.cmd\" run dev &",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Block,
            "{command}"
        );
    }
    assert_eq!(
        verdict(&project, &home, "pnpm dev", true),
        Verdict::Block,
        "run_in_background"
    );
    let other = tmp.path().join("other");
    for command in [
        "cd ../project; pnpm dev &",
        "Set-Location -Path ../project/web; Start-Process npm -ArgumentList 'run','dev'",
        "Start-Process npm -ArgumentList 'run','dev' -WorkingDirectory ../project",
        "Start-Job -WorkingDirectory ../project { pnpm dev }",
        "Start-Process npm -ArgumentList 'run','dev' -WorkingDirectory:../project",
        "Start-Job -WorkingDirectory:../project { pnpm dev }",
        "cmd /c start \"Dev Server\" /d ../project pnpm dev",
        "cd ~; pnpm dev &",
    ] {
        assert_eq!(
            verdict(&other, &home, command, false),
            Verdict::Block,
            "{command}"
        );
    }
}

/// Named parameters come in any order, by alias or unambiguous prefix, and
/// the program is `-FilePath` or the first positional argument wherever it
/// stands.
#[test]
fn reads_start_process_parameters_in_any_order() {
    let tmp = workspace();
    let project = tmp.path().join("project/web");
    let other = tmp.path().join("other");
    let home = tmp.path().join("project");
    for command in [
        "Start-Process -ArgumentList '--port 5173' -FilePath vite",
        "Start-Process -ArgumentList 'run','dev' npm",
        "Start-Process -Args 'run','dev' -FilePath npm",
        "Start-Process -Arg 'dev' pnpm",
        "Start-Process -A dev -F pnpm",
        "Start-Process -NoNewWindow -PassThru -ArgumentList 'run dev' -FilePath npm",
        "Start-Process -Path node -ArgumentList server.js",
        "Start-Process -PSPath vite",
        "Start-Process -FilePath:vite -Args:'--port','5173'",
        "Start-Process -Wo ../project/web npm 'run dev'",
        "Start-Process -Verb RunAs npm 'run dev'",
        "Start-Process -E @{PORT='3001'} pnpm dev",
        "Start-Process -V RunAs pnpm dev",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Block,
            "{command}"
        );
    }
    for command in [
        "Start-Process -ArgumentList 'start' -FilePath devlanes",
        "Start-Process -Args 'logs','web','-f' devlanes",
        "Start-Process -ArgumentList 'run','dev' npm -Wai",
        "Start-Process -WhatIf npm 'run dev'",
        "Start-Process -wi npm 'run dev'",
        "Start-Process -ArgumentList '--port 5173' -FilePath vite -Wait",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Allow,
            "{command}"
        );
    }
    for command in [
        "Start-Process -WorkingDirectory ../project -ArgumentList 'run','dev' -FilePath npm",
        "Start-Process -Wor ../project npm 'run dev'",
    ] {
        assert_eq!(
            verdict(&other, &home, command, false),
            Verdict::Block,
            "{command}"
        );
    }
}

/// Only `Start-Process` and common parameters are parameters: other words
/// with a dash are arguments of the started program.
#[test]
fn keeps_dash_arguments_of_start_process() {
    let tmp = workspace();
    let project = tmp.path().join("project/web");
    let home = tmp.path().join("project");
    for command in [
        "Start-Process tsx -ArgumentList '--watch','src/index.ts'",
        "Start-Process -ArgumentList '--watch','src/index.ts' -FilePath tsx",
        "Start-Process tsx --watch",
        "Start-Process tsx -ArgumentList --watch,src/index.ts",
        "Start-Process node -- --watch server.mjs",
        "Start-Process node -ArgumentList '-Wait','--watch'",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Block,
            "{command}"
        );
    }
    assert_eq!(
        verdict(
            &project,
            &home,
            "Start-Process tsx -ArgumentList 'src/index.ts'",
            false
        ),
        Verdict::Allow
    );
}

#[test]
fn allows_foreground_one_shot_and_quoted_commands_in_powershell() {
    let tmp = workspace();
    let project = tmp.path().join("project/web");
    let home = tmp.path().join("project");
    for command in [
        "pnpm dev",
        "pnpm build; pnpm test",
        "Start-Sleep 10 &",
        "Get-Content -Wait dev.log &",
        "vite build &",
        "Write-Output 'pnpm dev &'",
        "Write-Output \"npm start `& more\"",
        "echo 'it''s pnpm dev &'",
        "# pnpm dev &",
        "<# pnpm dev & #> Get-ChildItem",
        "git commit -m @'\nStart the dev server's & watcher\nStart-Process npm 'run dev'\n'@",
        "curl.exe \"https://dev.example.invalid/?a=1&b=2\"",
        "cargo build 2>&1 | Select-Object -Last 20",
        "Start-Process npm -ArgumentList 'run','dev' -Wait",
        "Start-Process npm -ArgumentList 'run','dev' -Wait:$true",
        "Start-Process http://localhost:5173",
        "& \"C:\\Users\\Jo Smith\\AppData\\Local\\Programs\\devlanes\\devlanes.exe\" start &",
        "Start-Job { pnpm build } | Wait-Job | Receive-Job",
        "Start-Job -FilePath .\\tasks.ps1",
        "cmd /c start /wait pnpm dev",
        "cmd /c start \"Dev Server\" /wait pnpm dev",
        "cmd /c 'start \"Dev\" /wait pnpm dev'",
        "cmd /c \"start \"\"Dev\"\" /wait pnpm dev\"",
        "cmd /c pnpm dev",
        "Invoke-Command { pnpm dev }",
        "devlanes exec --name web -- pnpm dev",
        "devlanes logs web -f --timeout 30s &",
        "Start-Process devlanes -ArgumentList 'logs','web','-f'",
        "$env:PORT = 3001; pnpm dev",
    ] {
        assert_eq!(
            verdict(&project, &home, command, false),
            Verdict::Allow,
            "{command}"
        );
    }
    assert_eq!(
        verdict(&project, &home, "devlanes start", true),
        Verdict::Allow
    );
    let other = tmp.path().join("other");
    for command in [
        "pnpm dev &",
        "Start-Job { pnpm dev }",
        "cd ../project; cd ../other; pnpm dev &",
        "Start-Process npm -ArgumentList 'run','dev' -WorkingDirectory ../other",
        "Start-Process npm -ArgumentList 'run','dev' -WorkingDirectory:../other",
    ] {
        assert_eq!(
            verdict(&other, &home, command, false),
            Verdict::Allow,
            "{command}"
        );
    }
}
