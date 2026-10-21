## Dev services (devlanes)

This repository runs its dev services with devlanes. A per-user daemon owns the processes; the `devlanes` CLI talks to it.

- Start this worktree's lane with `devlanes start`. It waits for ready or failure.
- Never background a process with `&` or `nohup`. Use `devlanes exec --name NAME -- COMMAND` instead.
- Lanes are `project@N` or `project@branch`; units are `LANE:UNIT`. Globs and `+tags` select several units.
- Inspect with `devlanes status`, `devlanes wait UNIT --timeout 180s`, `devlanes errors` and `devlanes why UNIT`.
- Read logs with `devlanes logs UNIT --tail 100` or `--grep RE`. Follow logs only with `--timeout`.
- Run tests with `devlanes with -- COMMAND`. It starts the lane, waits, runs COMMAND, stops what it started and exits with COMMAND's code.
- Clean up with `devlanes stop --mine`, which stops only what this session started.
- A command that needs confirmation exits 2. Rerun it with `--yes` only when the user asked for that action.
- Add `--json` for complete structured output.
- Inside the Codex sandbox the CLI does not start the daemon. If a command fails with `auto-spawn refused in sandbox`, ask the user to run `devlanes daemon install` outside the sandbox, then retry.
