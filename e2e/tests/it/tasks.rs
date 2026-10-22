//! `devlanes run TASK`: output and exit code of one run, and its stored log.

use devlanes_e2e::{COMMAND_TIMEOUT, Lab, Result};

use crate::BUDGET;

/// A terminal task: one colored line, then plain ones.
const PAINT: &str = r#"project: fixture-paint
tasks:
  paint:
    pty: true
    run: printf '\033[31mred-line\033[0m\nplain-line\nthird-line\n'
"#;

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

/// `logs --head` reads a terminal task's first lines. Their text has no escapes
/// (on Windows the console host adds its own), and colored output keeps only the
/// task's color, on the line that had it.
#[test]
fn logs_head_reads_the_first_lines_with_their_colors() -> Result<()> {
    let lab = Lab::with_config(PAINT, BUDGET)?;
    let mut command = lab.command(lab.repo(), &["run", "paint"])?;
    command.env("DEVLANES_OUTPUT", "plain");
    lab.run(command, COMMAND_TIMEOUT)?.expect_code(0)?;

    let logs = lab.json(lab.repo(), &["logs", "paint", "--head", "2", "--json"])?;
    let texts: Vec<&str> = logs["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| line["text"].as_str())
        .collect();
    assert_eq!(texts, ["red-line", "plain-line"], "{logs}");
    assert_eq!(logs["truncated"], true, "{logs}");

    let mut command = lab.command(lab.repo(), &["logs", "paint", "--head", "2"])?;
    command
        .env("DEVLANES_OUTPUT", "plain")
        .env("CLICOLOR_FORCE", "1");
    let output = lab.run(command, COMMAND_TIMEOUT)?;
    output.expect_code(0)?;
    let line = |text: &str| output.stdout.lines().find(|line| line.contains(text));
    assert!(
        line("red-line").is_some_and(|line| line.contains("\x1b[31m")),
        "{output}"
    );
    assert!(
        line("plain-line").is_some_and(|line| !line.contains('\x1b')),
        "{output}"
    );
    assert!(line("third-line").is_none(), "{output}");
    Ok(())
}
