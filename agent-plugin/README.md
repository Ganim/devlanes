# devlanes agent plugin

Instructions that teach coding agents to run dev services through the devlanes CLI instead of backgrounding them in a shell, to write `devlanes.yaml`, and to migrate other process-manager setups to it.

| Path | Content |
|---|---|
| `skills/devlanes/SKILL.md` | The skill: daily use. Claude Code, the skills CLI and Codex all use this file. |
| `skills/devlanes/references/configuration.md` | How to write a good `devlanes.yaml`; the skill tells agents to read it before editing the config. |
| `skills/devlanes/references/migration.md` | Mapping from mprocs, dekit, process-compose, Procfile runners, pm2, tmuxinator, tmuxp, package.json parallel scripts, mise and Docker Compose. |
| `.claude-plugin/plugin.json` | Claude Code plugin manifest. |
| `hooks/hooks.json`, `scripts/block-background.sh` | Optional Claude Code `PreToolUse` hook that rejects backgrounded dev servers; `scripts/test-block-background.sh` tests it. |
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

The plugin adds the `devlanes` skill and the hook. The hook runs before every Bash tool call. It blocks the call (exit 2) when all of these hold:

- a command is detached: it ends with a lone `&` outside quotes (`pnpm dev &`, `nohup ... &`, `... &>/dev/null &`, `... & disown`), runs under `setsid -f`, `tmux new -d` or `screen -dm`, or the call sets `run_in_background`;
- that command looks like a dev server: a `dev`, `start`, `serve`, `watch`, `preview` or `up` argument, a `server.js`-style script, or a tool such as `vite`, `next`, `wrangler` or `uvicorn` without `build` or `test`;
- the directory it runs in, after any `cd DIR &&` before it, or one of that directory's parents contains `devlanes.yaml`.

A detached `devlanes` command, such as `devlanes logs api -f &`, passes. The block message tells Claude to use `devlanes start` or `devlanes exec --name NAME -- COMMAND`. The script uses `jq` when it is installed and otherwise reads the `command` and `cwd` values from the JSON input with awk. To turn the hook off, set `DEVLANES_BLOCK_BACKGROUND=0` in Claude Code's environment, for example under `env` in `~/.claude/settings.json`.

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
