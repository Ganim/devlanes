---
cargo/devlanes: patch
cargo/devlanes-agent-hooks: patch
---

Changed the agent plugin's hook, which blocks dev servers started in the background inside a devlanes project, to run in devlanes for Claude Code's Bash and PowerShell tools, and added it for Codex.

The hook now needs devlanes of the plugin's version on the agent's `PATH`; with an older or missing devlanes every call passes. Codex runs it after you trust it in `/hooks`.
