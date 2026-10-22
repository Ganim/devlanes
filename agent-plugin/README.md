# devlanes agent plugin

Instructions that teach coding agents to run dev services through the devlanes CLI instead of backgrounding them in a shell, to write `devlanes.yaml`, and to migrate other process-manager setups to it.

| Path | Content |
|---|---|
| `skills/devlanes/SKILL.md` | The skill: daily use. Claude Code, the skills CLI and Codex all use this file. |
| `skills/devlanes/references/configuration.md` | How to write a good `devlanes.yaml`; the skill tells agents to read it before editing the config. |
| `skills/devlanes/references/migration.md` | Mapping from mprocs, dekit, process-compose, Procfile runners, pm2, tmuxinator, tmuxp, package.json parallel scripts, mise and Docker Compose. |
| `.claude-plugin/plugin.json` | Claude Code plugin manifest. |
| `hooks/hooks.json` | Optional `PreToolUse` hook that rejects backgrounded dev servers. It runs `devlanes __agent-hook`; the tests are in `crates/cli/tests/it/agent_hook.rs`. |
| `codex/AGENTS.md` | Short form of the skill for an `AGENTS.md` file. |

The short form's source is `crates/cli/agent-guide.md`, which `devlanes agent-guide` prints. `codex/AGENTS.md` is a copy of it, and a `devlanes-cli` test fails when the two differ or when a command in it no longer parses. When the skill changes, edit `agent-guide.md` and copy it over.

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

The plugin adds the `devlanes` skill and the hook. The hook runs `devlanes __agent-hook` before every Bash and PowerShell tool call, so `devlanes` must be on Claude Code's `PATH`; without it every shell call shows a hook error notice and nothing is blocked. It blocks the call (exit 2) when all of these hold:

- a command is detached: it ends with a lone `&` outside quotes (`pnpm dev &`, `nohup ... &`, `... &>/dev/null &`, `... & disown`, and PowerShell 7's `pnpm dev &`), runs under `setsid -f`, `tmux new -d` or `screen -dm`, or the call sets `run_in_background`. From the PowerShell tool, and from the Bash tool on Windows, `Start-Process` (without `-Wait`), `Start-Job`, `Start-ThreadJob`, cmd's `start` (without `/wait`) and the same commands inside `cmd /c` or `pwsh -c` count too;
- that command looks like a dev server: a `dev`, `start`, `serve`, `watch`, `preview` or `up` argument, a `server.js`-style script, or a tool such as `vite`, `next`, `wrangler` or `uvicorn` without `build` or `test`;
- the directory it runs in, after any `cd DIR &&` (or `Set-Location`, or `Start-Process -WorkingDirectory`) before it, or one of that directory's parents contains `devlanes.yaml`.

A detached `devlanes` command, such as `devlanes logs api -f &`, passes, and so does `Start-Process` or `start` with a URL, which opens a browser. The block message tells Claude to use `devlanes start` or `devlanes exec --name NAME -- COMMAND`. To turn the hook off, set `DEVLANES_BLOCK_BACKGROUND=0` in Claude Code's environment, for example under `env` in `~/.claude/settings.json`.

The hook is a shell-form command, so Claude Code runs it with `sh -c` on macOS and Linux and with Git Bash, or PowerShell when Git Bash is missing, on Windows; all of them find `devlanes.exe` or an npm `devlanes.cmd` shim on `PATH`. `onFailure: continue` keeps a missing `devlanes` from blocking every PowerShell call on Windows without Git Bash, where Claude Code otherwise blocks on a failed hook.

### Skills CLI

```sh
npx skills add devlanes-run/devlanes --skill devlanes
```

Add `-g` to install for every project, and `-a claude-code` or `-a codex` to pick the agent. This installs the skill only, without the hook.

The skills CLI finds the skill through the repository's `.claude-plugin/marketplace.json`, which lists `agent-plugin` as a plugin.

### Codex

Install the skill into `.agents/skills/`, where Codex reads skills:

```sh
npx skills add devlanes-run/devlanes --skill devlanes -a codex
```

Or copy `codex/AGENTS.md` into the repository's `AGENTS.md`, or into `~/.codex/AGENTS.md` for every repository.

Codex also reads this directory as a plugin (it accepts `.claude-plugin/plugin.json`) and then loads `hooks/hooks.json`. It skips the hook until you trust it in `/hooks`. Codex runs the command with `$SHELL -lc` on macOS and Linux and `cmd.exe /C` on Windows, ignores `onFailure`, and names its shell tool `Bash` even when the command is PowerShell, which is why Windows `Bash` calls get the PowerShell rules too.

## Behaviour

The CLI starts the daemon on first use, except inside a sandbox. It treats the process as sandboxed when `CODEX_SANDBOX` or `APP_SANDBOX_CONTAINER_ID` is set, or when it cannot bind a test Unix socket next to the daemon socket. On Windows, where Codex never sets `CODEX_SANDBOX`, it also does so when `CODEX_SANDBOX_NETWORK_DISABLED` or `SBX_NONET_ACTIVE` is set or when the process token is restricted, an AppContainer or below medium integrity (Codex's unelevated sandbox). It then fails with `auto-spawn refused in sandbox` instead of starting a daemon whose processes the sandbox would confine.

Run this once, outside the sandbox:

```sh
devlanes daemon install
```

It writes a per-user service (`~/Library/LaunchAgents/run.devlanes.daemon.plist` on macOS, `~/.config/systemd/user/devlanes.service` on Linux, a logon entry in the user's `Run` registry key on Windows) that starts the daemon now and at every login. `devlanes daemon uninstall` removes it.

A sandboxed CLI still has to connect to the daemon's Unix socket or, on Windows, named pipe. Whether the Codex sandbox allows that connection has not been tested yet. On Windows the daemon refuses clients with a restricted or AppContainer token, and Codex's elevated sandbox runs commands as a separate Windows user that the pipe does not admit, so expect to approve running `devlanes` commands outside the sandbox there.

## License

Apache-2.0
