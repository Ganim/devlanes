//! Stand-in for an external program in tests. A copy named after the tool reads
//! its behaviour from the `.json` file next to it; see
//! `devlanes_testing::fake_tool`.

use std::{fs::OpenOptions, io::Write, process::ExitCode, time::Duration};

use devlanes_testing::fake_tool::{Behaviour, expand, expand_json, sidecar};

/// Runs the behaviour; a fake that cannot read it exits 101 with the reason.
fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fake-tool: {error}");
            ExitCode::from(101)
        }
    }
}

/// Checks arguments, records them, sleeps, prints and returns the exit code.
fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let behaviour: Behaviour =
        serde_json::from_slice(&std::fs::read(sidecar(&std::env::current_exe()?))?)?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if behaviour
        .args
        .as_ref()
        .is_some_and(|expected| *expected != args)
    {
        eprintln!("bad args: {}", args.join(" "));
        return Ok(ExitCode::from(2));
    }
    if let Some(record) = &behaviour.record {
        let mut file = OpenOptions::new().create(true).append(true).open(record)?;
        writeln!(file, "{}", serde_json::to_string(&args)?)?;
    }
    if behaviour.sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(behaviour.sleep_ms));
    }
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?
        .to_string_lossy()
        .into_owned();
    let env = |name: &str| std::env::var(name).ok();
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(expand(&behaviour.stderr, &cwd, &env).as_bytes())?;
    stderr.flush()?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(expand(&behaviour.stdout, &cwd, &env).as_bytes())?;
    if let Some(json) = &behaviour.stdout_json {
        serde_json::to_writer(&mut stdout, &expand_json(json, &cwd, &env))?;
    }
    stdout.flush()?;
    Ok(ExitCode::from(behaviour.exit))
}
