use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::windows::io::AsHandle;

use devlanes_sys_windows::spawn::{
    CreationFlags, EnvError, Environment, Spawn, Stdio, Stream, pipe, spawn,
};

use crate::{PATIENCE, cmd, read_all, test_job};

/// Output reaches the given pipe from stdout and stderr, the environment is
/// exactly the one given, the working directory applies and the exit code
/// comes back.
#[test]
fn output_environment_cwd_and_exit_code() -> io::Result<()> {
    let job = test_job()?;
    let (reader, writer) = pipe()?;
    let program = cmd()?;
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let environment = Environment::new([
        (OsStr::new("SystemRoot"), root.as_os_str()),
        (OsStr::new("DEVLANES_PROBE"), OsStr::new("here")),
    ])
    .map_err(io::Error::other)?;
    let cwd = tempfile::tempdir()?;
    let child = spawn(&Spawn {
        program: &program,
        command_line: OsStr::new(
            r#"cmd.exe /d /c "echo %DEVLANES_PROBE% %HOME%& cd& echo err 1>&2& exit 3""#,
        ),
        environment: Some(&environment),
        cwd: Some(cwd.path()),
        stdio: Stdio::Handles {
            stdin: Stream::Null,
            stdout: Stream::Handle(writer.as_handle()),
            stderr: Stream::Handle(writer.as_handle()),
        },
        inherit: &[],
        job: Some(&job),
        flags: CreationFlags {
            no_window: true,
            ..CreationFlags::default()
        },
    })?;
    // Only the child's duplicate keeps the pipe open now.
    drop(writer);
    let output = read_all(reader)?;
    let lines: Vec<&str> = output.lines().map(str::trim_end).collect();
    let cwd = cwd.path().to_string_lossy().into_owned();
    assert_eq!(lines, ["here %HOME%", cwd.as_str(), "err"], "{output:?}");
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(3));
    assert!(child.start_time() > 0);
    Ok(())
}

/// Standard input comes from a pipe; closing this end ends the child's input.
#[test]
fn input_from_a_pipe() -> io::Result<()> {
    let job = test_job()?;
    let (input_read, mut input_write) = pipe()?;
    let (output_read, output_write) = pipe()?;
    let program = devlanes_sys_windows::file::system_directory()?.join("sort.exe");
    let child = spawn(&Spawn {
        program: &program,
        command_line: OsStr::new("sort.exe"),
        environment: None,
        cwd: None,
        stdio: Stdio::Handles {
            stdin: Stream::Handle(input_read.as_handle()),
            stdout: Stream::Handle(output_write.as_handle()),
            stderr: Stream::Null,
        },
        inherit: &[],
        job: Some(&job),
        flags: CreationFlags {
            no_window: true,
            ..CreationFlags::default()
        },
    })?;
    drop((input_read, output_write));
    input_write.write_all(b"pear\r\napple\r\n")?;
    drop(input_write);
    assert_eq!(read_all(output_read)?, "apple\r\npear\r\n");
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(0));
    Ok(())
}

/// A program that does not exist fails to start and leaves nothing in the job.
#[test]
fn missing_programs_fail() -> io::Result<()> {
    let job = test_job()?;
    let dir = tempfile::tempdir()?;
    let program = dir.path().join("missing.exe");
    let error = spawn(&Spawn {
        program: &program,
        command_line: OsStr::new("missing.exe"),
        environment: None,
        cwd: None,
        stdio: Stdio::Handles {
            stdin: Stream::Null,
            stdout: Stream::Null,
            stderr: Stream::Null,
        },
        inherit: &[],
        job: Some(&job),
        flags: CreationFlags::default(),
    })
    .err()
    .ok_or_else(|| io::Error::other("a missing program started"))?;
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    assert!(job.members()?.is_empty());
    Ok(())
}

/// Blocks are sorted by name ignoring case, as Windows expects.
#[test]
fn environment_blocks_sort_without_case() -> Result<(), EnvError> {
    let block = Environment::new([("b", "2"), ("A", "1"), ("_x", "3")])?;
    // `_` (0x5F) sorts after the uppercase letters.
    assert_eq!(
        String::from_utf16_lossy(block.as_units()),
        "A=1\0b=2\0_x=3\0\0"
    );
    let empty = Environment::new(std::iter::empty::<(&str, &str)>())?;
    assert_eq!(empty.as_units(), [0, 0]);
    Ok(())
}

/// Anything that would corrupt the block or make a variable ambiguous is
/// refused.
#[test]
fn environment_blocks_refuse_ambiguity() {
    let refused = |pairs: &[(&str, &str)]| Environment::new(pairs.iter().copied()).err();
    assert_eq!(refused(&[("", "x")]), Some(EnvError::EmptyName));
    assert_eq!(
        refused(&[("A=B", "x")]),
        Some(EnvError::Equals("A=B".into()))
    );
    assert_eq!(
        refused(&[("=C:", r"C:\")]),
        Some(EnvError::Equals("=C:".into()))
    );
    assert_eq!(
        refused(&[("A\0B", "x")]),
        Some(EnvError::Nul("A\0B".into()))
    );
    assert_eq!(
        refused(&[("A", "x\0EVIL=1")]),
        Some(EnvError::Nul("A".into()))
    );
    assert!(matches!(
        refused(&[("Path", "a"), ("PATH", "b")]),
        Some(EnvError::Duplicate(..))
    ));
    assert!(matches!(
        refused(&[("caf\u{e9}", "a"), ("CAF\u{c9}", "b")]),
        Some(EnvError::Duplicate(..))
    ));
}
