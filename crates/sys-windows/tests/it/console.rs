use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::io::AsHandle;
use std::sync::mpsc;

use devlanes_sys_windows::console::{
    AllocMode, Allocated, Attached, CtrlEvent, Keyboard, alloc_console, alloc_hidden_console,
    console_processes, has_window, keyboard, set_ctrl_c_ignored,
};
use devlanes_sys_windows::spawn::{
    self, CreationFlags, Environment, Spawn, Stdio, Stream, pipe, spawn,
};

use crate::{PATIENCE, read_all, test_job};

/// Set in the helper process of the tests below to `<pid>:c` or
/// `<pid>:break`: the process whose console it attaches to and the event.
const TARGET: &str = "DEVLANES_SYS_WINDOWS_TEST_CTRL_TARGET";

/// Set in the unit process of the tests below, which then waits to be ended.
const UNIT: &str = "DEVLANES_SYS_WINDOWS_TEST_CTRL_UNIT";

/// `STATUS_CONTROL_C_EXIT`: ended by a console control event.
const CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Not a test of its own: with [`TARGET`] set, this binary is the helper that
/// attaches to the target's console, checks both are on it, and sends the
/// event. It must survive its own event, so it ends with success.
#[test]
fn ctrl_helper_entry() -> io::Result<()> {
    let Some(target) = std::env::var_os(TARGET) else {
        return Ok(());
    };
    let (pid, event) = target
        .to_str()
        .and_then(|text| text.split_once(':'))
        .ok_or_else(|| io::Error::other("bad target"))?;
    let pid: u32 = pid.parse().map_err(io::Error::other)?;
    let event = if event == "c" {
        CtrlEvent::CtrlC
    } else {
        CtrlEvent::CtrlBreak
    };
    let attached = Attached::to(pid).map_err(io::Error::other)?;
    let processes = attached.processes()?;
    assert!(processes.contains(&pid), "{processes:?}");
    assert!(processes.contains(&std::process::id()), "{processes:?}");
    assert!(
        attached.generate(event, PATIENCE)?,
        "the event never arrived"
    );
    Ok(())
}

/// Not a test of its own: with [`UNIT`] set, this binary is a unit with the
/// default control handling. It says when it runs, then waits to be ended.
#[test]
fn ctrl_unit_entry() -> io::Result<()> {
    if std::env::var_os(UNIT).is_none() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    writeln!(stdout, "ready")?;
    stdout.flush()?;
    std::thread::sleep(PATIENCE * 2);
    Ok(())
}

/// Starts this binary as `entry` with `variable` set, in `job` with `flags`
/// and stdout and stderr on a pipe.
fn start(
    entry: &str,
    variable: (&str, String),
    job: &devlanes_sys_windows::job::Job,
    flags: CreationFlags,
) -> io::Result<(spawn::Child, io::PipeReader)> {
    let exe = std::env::current_exe()?;
    let line = format!("\"{}\" --exact console::{entry} --nocapture", exe.display());
    // The `=C:` drive variables cannot go into a block; the children need none.
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !name.to_string_lossy().starts_with('='))
        .collect();
    variables.push((variable.0.into(), variable.1.into()));
    let environment = Environment::new(variables).map_err(io::Error::other)?;
    let (reader, writer) = pipe()?;
    let child = spawn(&Spawn {
        program: &exe,
        command_line: OsStr::new(&line),
        environment: Some(&environment),
        cwd: None,
        stdio: Stdio::Handles {
            stdin: Stream::Null,
            stdout: Stream::Handle(writer.as_handle()),
            stderr: Stream::Handle(writer.as_handle()),
        },
        inherit: &[],
        job: Some(job),
        flags,
    })?;
    Ok((child, reader))
}

/// Starts a windowless unit, then a helper without a console that sends
/// `event` (`c` or `break`) to the unit's console; returns the unit's exit
/// code once the helper succeeded.
fn send_to_unit(event: &str) -> io::Result<Option<u32>> {
    let job = test_job()?;
    let no_window = CreationFlags {
        no_window: true,
        ..CreationFlags::default()
    };
    let (unit, output) = start("ctrl_unit_entry", (UNIT, "1".into()), &job, no_window)?;
    // Once the unit runs its test, its console is set up.
    let (ready, said) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if line.is_ok_and(|line| line.trim() == "ready") {
                let _ = ready.send(());
            }
        }
    });
    said.recv_timeout(PATIENCE)
        .map_err(|_| io::Error::other("the unit never ran"))?;
    let detached = CreationFlags {
        detached_process: true,
        ..CreationFlags::default()
    };
    let target = format!("{}:{event}", unit.pid());
    let (helper, output) = start("ctrl_helper_entry", (TARGET, target), &job, detached)?;
    let output = read_all(output)?;
    assert_eq!(helper.wait_timeout(PATIENCE)?, Some(0), "{output}");
    unit.wait_timeout(PATIENCE)
}

/// A helper without a console attaches to a windowless unit's console and
/// ends it with Ctrl-Break, surviving the event itself.
#[test]
fn ctrl_break_reaches_a_console() -> io::Result<()> {
    assert_eq!(send_to_unit("break")?, Some(CONTROL_C_EXIT));
    Ok(())
}

/// Set in the process of [`alloc_entry`], which then checks
/// [`alloc_console`] and exits with the first failed check's number.
const ALLOC: &str = "DEVLANES_SYS_WINDOWS_TEST_ALLOC";

/// Not a test of its own: with [`ALLOC`] set to `new`, this binary was
/// started without a console and gets a hidden one; with `existing`, it had
/// one already and keeps it.
#[test]
fn alloc_entry() {
    let Some(expected) = std::env::var_os(ALLOC) else {
        return;
    };
    let first = if expected == "new" {
        Allocated::New
    } else {
        Allocated::Existing
    };
    let checks = [
        alloc_console(AllocMode::NoWindow).ok() == Some(first),
        alloc_console(AllocMode::NoWindow).ok() == Some(Allocated::Existing),
        // Every stream is NUL, so only a missing window makes keys unreachable.
        keyboard() == Keyboard::Unreachable,
        console_processes().is_ok_and(|list| list.contains(&std::process::id())),
    ];
    if let Some(failed) = checks.iter().position(|passed| !passed) {
        std::process::exit(10 + i32::try_from(failed).unwrap_or(0));
    }
}

/// A process started without a console (as the Run key starts a program
/// whose manifest asks for none) gets a windowless one; a process with a
/// hidden console keeps it.
#[test]
fn alloc_console_gives_a_hidden_console() -> io::Result<()> {
    let job = test_job()?;
    for (expected, flags) in [
        (
            "new",
            CreationFlags {
                detached_process: true,
                ..CreationFlags::default()
            },
        ),
        (
            "existing",
            CreationFlags {
                no_window: true,
                ..CreationFlags::default()
            },
        ),
    ] {
        let (child, output) = start("alloc_entry", (ALLOC, expected.into()), &job, flags)?;
        let output = read_all(output)?;
        assert_eq!(
            child.wait_timeout(PATIENCE)?,
            Some(0),
            "{expected}: {output}"
        );
    }
    Ok(())
}

/// Set in the process of [`alloc_hidden_entry`].
const ALLOC_HIDDEN: &str = "DEVLANES_SYS_WINDOWS_TEST_ALLOC_HIDDEN";

/// Not a test of its own: with [`ALLOC_HIDDEN`] set, this binary was started
/// without a console and with stdout on a pipe. It gets a console without a
/// window and says `kept` on stdout, or exits with the first failed check's
/// number.
#[test]
fn alloc_hidden_entry() -> io::Result<()> {
    if std::env::var_os(ALLOC_HIDDEN).is_none() {
        return Ok(());
    }
    let checks = [
        alloc_hidden_console().ok() == Some(Allocated::New),
        !has_window(),
        alloc_hidden_console().ok() == Some(Allocated::Existing),
    ];
    if let Some(failed) = checks.iter().position(|passed| !passed) {
        std::process::exit(10 + i32::try_from(failed).unwrap_or(0));
    }
    let mut stdout = io::stdout();
    writeln!(stdout, "kept")?;
    stdout.flush()
}

/// A process started without a console and with stdout on a pipe, as from a
/// detached parent, gets a console without a window and still writes to the
/// pipe.
#[test]
fn alloc_hidden_console_keeps_redirected_streams() -> io::Result<()> {
    let job = test_job()?;
    let detached = CreationFlags {
        detached_process: true,
        ..CreationFlags::default()
    };
    let (child, output) = start(
        "alloc_hidden_entry",
        (ALLOC_HIDDEN, "1".into()),
        &job,
        detached,
    )?;
    let output = read_all(output)?;
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(0), "{output}");
    assert!(output.lines().any(|line| line.trim() == "kept"), "{output}");
    Ok(())
}

/// Ctrl-C reaches a unit that does not ignore it. The unit inherits the
/// ignore flag from this process, which whatever started the tests may have
/// set, so it is cleared first.
#[test]
fn ctrl_c_reaches_a_console() -> io::Result<()> {
    set_ctrl_c_ignored(false)?;
    assert_eq!(send_to_unit("c")?, Some(CONTROL_C_EXIT));
    Ok(())
}
