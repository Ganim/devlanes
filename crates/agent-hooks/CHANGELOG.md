# devlanes-agent-hooks

## 0.4.0 — 2026-10-25

### Patch changes

- [07ba115](https://github.com/devlanes-run/devlanes/commit/07ba115e2da3825ad16d6b8d1f8b1e5879f919e4) Changed the agent plugin's hook, which blocks dev servers started in the background inside a devlanes project, to run in devlanes for Claude Code's Bash and PowerShell tools, and added it for Codex.
  
  The hook now needs devlanes of the plugin's version on the agent's `PATH`; with an older or missing devlanes every call passes. Codex runs it after you trust it in `/hooks`.

