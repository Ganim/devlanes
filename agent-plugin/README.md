# devlanes agent plugin

Instructions that teach coding agents to run dev services through the devlanes CLI instead of backgrounding them in a shell.

| Path | Content |
|---|---|
| `skills/devlanes/SKILL.md` | The skill. Claude Code, the skills CLI and Codex all use this file. |
| `.claude-plugin/plugin.json` | Claude Code plugin manifest. |
| `hooks/hooks.json`, `scripts/block-background.sh` | Optional Claude Code `PreToolUse` hook that rejects backgrounded dev servers. |
| `codex/AGENTS.md` | Short form of the skill for an `AGENTS.md` file. |

`devlanes agent-guide` prints the same short form, from `AGENT_GUIDE` in `crates/cli/src/lib.rs`. When the skill changes, update `AGENT_GUIDE` and `codex/AGENTS.md` with it.

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

The plugin adds the `devlanes` skill and the hook. The hook runs before every Bash tool call. It blocks the call (exit 2) when all of these hold:

- the working directory or one of its parents contains `devlanes.yaml`;
- the command backgrounds a process with a lone `&` (as in `pnpm dev &`, `nohup ... &` or `... &>/dev/null &`), or the call sets `run_in_background`;
- the command looks long-running: `dev`, `start`, `serve`, `watch`, `preview`, `vite`, `next`, `wrangler`, `uvicorn`, `rails` and similar words.

Commands that call `devlanes` pass. The block message tells Claude to use `devlanes start` or `devlanes exec --name NAME -- COMMAND`. The script uses `jq` when it is installed and falls back to matching the raw JSON input. To turn the hook off, set `DEVLANES_BLOCK_BACKGROUND=0` in Claude Code's environment, for example under `env` in `~/.claude/settings.json`.

### Skills CLI

```sh
npx skills add devlanes-run/devlanes --skill devlanes
```

Add `-g` to install for every project, and `-a claude-code` or `-a codex` to pick the agent. This installs the skill only, without the hook.

The skill is not in one of the directories the skills CLI scans first (`skills/`, `.claude/skills/` and others at the repository root). The CLI then falls back to a recursive search, which finds `agent-plugin/skills/devlanes`. This was checked with skills 1.7.4 against a copy of this layout. If a later version stops finding it, install from the direct path:

```sh
npx skills add https://github.com/devlanes-run/devlanes/tree/main/agent-plugin/skills/devlanes
```

### Codex

Install the skill into `.agents/skills/`, where Codex reads skills:

```sh
npx skills add devlanes-run/devlanes --skill devlanes -a codex
```

Or copy `codex/AGENTS.md` into the repository's `AGENTS.md`, or into `~/.codex/AGENTS.md` for every repository.

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
