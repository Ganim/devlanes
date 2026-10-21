//! `devlanes run TASK`: output and exit code of one run.

use devlanes_e2e::{Lab, Result};

use crate::BUDGET;

/// Each run prints its own number and exits with it plus two, so every run differs.
const COUNTER: &str = r#"project: fixture-tasks
tasks:
  count:
    run: |
      n=$(cat "$HOME/run-count" 2>/dev/null || echo 0)
      n=$((n + 1))
      echo "$n" > "$HOME/run-count"
      echo "task-run-$n"
      exit $((n + 2))
"#;

/// Three runs of one task: each prints only its own output (no lines of earlier runs)
/// and exits with that run's code.
#[test]
fn run_prints_only_this_run_and_keeps_the_exit_code() -> Result<()> {
    let lab = Lab::with_config(COUNTER, BUDGET)?;
    for run in 1..=3 {
        let mut command = lab.command(lab.repo(), &["run", "count"])?;
        command.env("DEVLANES_OUTPUT", "plain");
        let output = lab.run(command, devlanes_e2e::COMMAND_TIMEOUT)?;
        output.expect_code(run + 2)?;
        assert!(
            output.stdout.contains(&format!("task-run-{run}")),
            "run {run} output missing: {output}"
        );
        for earlier in 1..run {
            assert!(
                !output.stdout.contains(&format!("task-run-{earlier}")),
                "run {run} printed run {earlier}'s output: {output}"
            );
        }
    }
    Ok(())
}
