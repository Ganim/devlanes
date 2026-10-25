# devlanes agent plugin

Instructions that teach coding agents to run dev services through the devlanes CLI instead of backgrounding them in a shell, to write `devlanes.yaml`, and to migrate other process-manager setups to it.

| Path | Content |
|---|---|
| `skills/devlanes/SKILL.md` | The skill: daily use. Claude Code, the skills CLI and Codex all use this file. |
| `skills/devlanes/references/configuration.md` | How to write a good `devlanes.yaml`; the skill tells agents to read it before editing the config. |
| `skills/devlanes/references/migration.md` | Mapping from mprocs, dekit, process-compose, Procfile runners, pm2, tmuxinator, tmuxp, package.json parallel scripts, mise and Docker Compose. |
| `.claude-plugin/plugin.json` | Claude Code plugin manifest. |
| `.codex-plugin/plugin.json` | Codex plugin manifest. |
| `hooks/hooks.json`, `hooks/codex.json` | `PreToolUse` hooks for Claude Code and Codex that reject backgrounded dev servers. Both run `devlanes __agent-hook` (`crates/agent-hooks`). |
| `codex/AGENTS.md` | Short form of the skill for an `AGENTS.md` file. |

When the skill changes, update `codex/AGENTS.md` to match. A `devlanes-cli` test fails when a command in it no longer parses.

## Usage

### Claude Code

Install from the repository's marketplace:

```sh
claude plugin marketplace add devlanes-run/devlanes
claude plugin install devlanes@devlanes
```

For one session from a checkout, without installing:

```sh
claude --plugin-dir ./agent-plugin
```

The plugin adds the `devlanes` skill and a hook that runs before every Bash and PowerShell tool call (see [Hook](#hook)).

### Codex

Codex reads the same marketplace file and the plugin's `.codex-plugin/plugin.json`:

```sh
codex plugin marketplace add devlanes-run/devlanes
codex plugin install devlanes@devlanes
```

The plugin adds the skill and a hook before every shell command. Codex runs no plugin hook until you trust it: open `/hooks` in Codex, review the devlanes hook and trust it. Codex asks again whenever the hook's definition changes.

Without the plugin, install the skill into `.agents/skills/`, where Codex reads skills, or copy `codex/AGENTS.md` into the repository's `AGENTS.md` (or `~/.codex/AGENTS.md` for every repository). Neither adds the hook.

```sh
npx skills add devlanes-run/devlanes --skill devlanes -a codex
```

### Skills CLI

```sh
npx skills add devlanes-run/devlanes --skill devlanes
```

Add `-g` to install for every project, and `-a claude-code` or `-a codex` to pick the agent. This installs the skill only, without the hook.

The skills CLI finds the skill through the repository's `.claude-plugin/marketplace.json`, which lists `agent-plugin` as a plugin.

## Hook

Both agents' hooks run devlanes itself:

```sh
devlanes __agent-hook --agent claude-code; exit 0   # hooks/hooks.json, on Bash and PowerShell
devlanes __agent-hook --agent codex; exit 0         # hooks/codex.json, on Bash
```

The hook blocks a call (a `PreToolUse` answer with `permissionDecision: "deny"`) when all of these hold:

- a command is detached: it ends with a lone `&` outside quotes (`pnpm dev &`, `nohup ... &`, `... &>/dev/null &`, `... & disown`), runs under `setsid -f`, `tmux new -d` or `screen -dm`, or the call sets `run_in_background`;
- that command looks like a dev server: a `dev`, `start`, `serve`, `watch`, `--watch`, `preview` or `up` argument, a `server.js`-style script, or a tool such as `vite`, `next`, `wrangler` or `uvicorn` without `build`, `test`, `lint` or `check`;
- the directory it runs in, after any `cd` before it, or one of that directory's parents contains `devlanes.yaml`.

A detached `devlanes` command, such as `devlanes logs api -f &`, passes. The block message tells the agent to use `devlanes start` or `devlanes exec --name NAME -- COMMAND`. To turn the hook off, set `DEVLANES_BLOCK_BACKGROUND=0` in the agent's environment, for example under `env` in `~/.claude/settings.json`.

It needs devlanes of the plugin's version or later on the agent's `PATH` (each release ships both with the same version). When devlanes is missing or older, the "not found" error or devlanes' usage error still goes to the hook's stderr, but `; exit 0` makes the call pass: the usage error exits 2, which both agents read as a block of every call. The line works the same in sh, Bash, Git Bash and PowerShell. Codex on Windows runs `commandWindows` instead, `cmd /d /c "devlanes __agent-hook --agent codex & exit 0"`, which also works when Codex falls back to `cmd.exe` for hooks. For Claude Code, `onFailure: "continue"` lets the call go ahead if the hook times out.

The hook reads each command in the shell that runs it: Claude Code's Bash tool in Bash (Git Bash on Windows) and its PowerShell tool in PowerShell. Codex calls every shell command `Bash`, and runs it in PowerShell on Windows, so its hook reads Codex's commands as PowerShell there and as Bash elsewhere.

### PowerShell

PowerShell commands count as detached with:

- a trailing `&` outside quotes (`pnpm dev &`, `pnpm dev | Tee-Object dev.log &`), or `run_in_background`;
- `Start-Process` (`start`, `saps`) without `-Wait` or `-WhatIf`. The program is `-FilePath` or the first positional argument, its arguments are `-ArgumentList` and the other positional ones, and it runs in `-WorkingDirectory`; parameters may come in any order, by alias (`-Args`) or by unambiguous prefix (`-Wo`), and any other word, such as `--watch`, is an argument of the program;
- `Start-Job` (`sajb`) and `Start-ThreadJob`, judged by the commands in their script block;
- `cmd /c start` without `/wait`.

`cd`, `Set-Location`, `sl` and `pushd` move the directory. Quotes, backtick escapes, here-strings, `#` and `<# #>` comments and the call operator (`& 'C:\tools\vite.cmd'`) are read, so `Write-Output 'pnpm dev &'` passes.

It does not catch:

- splatting (`Start-Process @params`) and abbreviated parameters of commands other than `Start-Process`;
- commands built at run time: arguments in variables (`Start-Process npm $arguments`), `Invoke-Expression`, `-EncodedCommand`, a nested `pwsh -Command "pnpm dev &"`, and scripts started with `-FilePath` or `.\dev.ps1`;
- .NET calls such as `[Diagnostics.Process]::Start(...)`, scheduled tasks and services.

It can also block too much: `Start-Process server.js` opens the file in its associated program, which may be an editor, and still counts as starting a server.

## Behaviour

The CLI starts the daemon on first use, except inside a sandbox. It treats the process as sandboxed when `CODEX_SANDBOX` or `APP_SANDBOX_CONTAINER_ID` is set, or when it cannot bind a test Unix socket next to the daemon socket. It then fails with `auto-spawn refused in sandbox` instead of starting a daemon whose processes the sandbox would confine.

Run this once, outside the sandbox:

```sh
devlanes daemon install
```

It writes a per-user service (`~/Library/LaunchAgents/run.devlanes.daemon.plist` on macOS, `~/.config/systemd/user/devlanes.service` on Linux) that starts the daemon now and at every login. `devlanes daemon uninstall` removes it.

A sandboxed CLI still has to connect to the daemon's Unix socket. Whether the Codex sandbox allows that connection has not been tested yet. If it does not, approve running `devlanes` commands outside the sandbox.

## License

Apache-2.0
