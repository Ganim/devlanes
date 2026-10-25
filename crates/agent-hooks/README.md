# devlanes-agent-hooks

The `PreToolUse` hook of the agent plugin in `agent-plugin/`: it denies shell commands that start a dev server in the background inside a devlanes project, and tells the agent to use `devlanes start` or `devlanes exec` instead. The binary runs it for the hidden `devlanes __agent-hook` command, which the plugin's hooks for Claude Code and Codex call; it only reads stdin and writes stdout.

## Usage

```sh
echo '{"cwd":"/src/acme","tool_name":"Bash","tool_input":{"command":"pnpm dev &"}}' \
  | devlanes __agent-hook --agent claude-code
```

```rust
use std::path::Path;
use devlanes_agent_hooks::{Agent, Verdict, decide};

let payload = r#"{"cwd":"/src/acme","tool_name":"Bash","tool_input":{"command":"pnpm dev &"}}"#;
let verdict = decide(payload, Agent::Codex, cfg!(windows), Path::new("."), None);
assert!(matches!(verdict, Verdict::Allow | Verdict::Block));
```

## API

- `run`: the whole hook for the arguments after `__agent-hook`; prints `deny_output()` for a blocked call and always returns 0.
- `decide`: the verdict for one payload from an `Agent`, on Windows or not.
- `Agent::shell`: which `Shell` runs a tool. Claude Code's `Bash` tool is Bash (Git Bash on Windows) and its `PowerShell` tool is PowerShell; Codex names every shell command `Bash` and runs it in PowerShell on Windows.
- `Shell::blocks`: the Bash and PowerShell readers.
- `ToolCall::parse`: the payload fields the hook reads, the same for both agents.

## Rules

A command is blocked when all of these hold:

- it is detached: a lone `&` after it (or after the commands joined to it), `run_in_background`, `setsid -f`, `tmux new -d` or `screen -dm` in Bash, and `Start-Process` without `-Wait`, `Start-Job`, `Start-ThreadJob` or `cmd /c start` without `/wait` in PowerShell;
- it looks like a dev server: a `dev`, `start`, `serve`, `watch`, `--watch`, `preview` or `up` argument, a `server.js`-style script, or a tool such as `vite`, `next`, `wrangler` or `uvicorn` without `build`, `test`, `lint` or `check`;
- it is not devlanes itself, such as `devlanes logs api -f &`;
- the directory it runs in, after any `cd` before it, or one of its parents has `devlanes.yaml`.

`DEVLANES_BLOCK_BACKGROUND=0` in the agent's environment allows every call, and so do arguments other than `--agent claude-code` or `--agent codex`, which come from a newer plugin.

## License

Apache-2.0
