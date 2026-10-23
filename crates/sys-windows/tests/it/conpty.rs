use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::windows::io::AsHandle;
use std::sync::mpsc;

use devlanes_sys_windows::conpty::PseudoConsole;
use devlanes_sys_windows::spawn::{CreationFlags, Spawn, Stdio, pipe, spawn};

use crate::{PATIENCE, cmd, read_all, test_job};

/// A child in a pseudo console reads typed input from it and writes to it,
/// and the output ends once the console is closed.
#[test]
fn child_runs_in_a_pseudo_console() -> io::Result<()> {
    let job = test_job()?;
    let (input_read, mut input_write) = pipe()?;
    let (output_read, output_write) = pipe()?;
    let console = PseudoConsole::create(100, 30, input_read.as_handle(), output_write.as_handle())?;
    // The console holds its own duplicates of these ends.
    drop((input_read, output_write));
    // Output only ends once the console closes, so it is read on a thread.
    let (sender, output) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read_all(output_read));
    });
    let program = cmd()?;
    let child = spawn(&Spawn {
        program: &program,
        // Delayed expansion (`!NAME!`), since `%NAME%` expands before `set /p` runs.
        command_line: OsStr::new(r#"cmd.exe /d /q /v:on /c "set /p NAME=& echo hello !NAME!""#),
        environment: None,
        cwd: None,
        stdio: Stdio::PseudoConsole(&console),
        inherit: &[],
        job: Some(&job),
        flags: CreationFlags::default(),
    })?;
    console.resize(120, 40)?;
    input_write.write_all(b"acme\r")?;
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(0));
    drop(console);
    let output = output
        .recv_timeout(PATIENCE)
        .map_err(|_| io::Error::other("no output"))??;
    assert!(output.contains("hello acme"), "{output:?}");
    Ok(())
}

/// Sizes a console cannot hold are refused before any call.
#[test]
fn sizes_are_checked() -> io::Result<()> {
    let (input_read, _input_write) = pipe()?;
    let (_output_read, output_write) = pipe()?;
    for (columns, rows) in [(0, 10), (10, 0), (40_000, 10)] {
        let error = PseudoConsole::create(
            columns,
            rows,
            input_read.as_handle(),
            output_write.as_handle(),
        )
        .err()
        .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::InvalidInput), "{columns}x{rows}");
    }
    Ok(())
}
