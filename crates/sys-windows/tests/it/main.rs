//! Live checks of the Win32 wrappers against this test process, scratch
//! directories and registry keys, and short-lived children.
#![cfg(windows)]

mod cert;
mod conpty;
mod console;
mod file;
mod job;
mod net;
mod pipe;
mod proc;
mod registry;
mod security;
mod spawn;
mod token;
mod usage;

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use devlanes_sys_windows::job::{Job, JobOptions};
use devlanes_sys_windows::spawn::{self as win_spawn, CreationFlags, Spawn, Stdio, Stream};

/// How long a test waits for a child or a condition before failing.
const PATIENCE: Duration = Duration::from_secs(20);

/// `cmd.exe` from System32.
fn cmd() -> io::Result<PathBuf> {
    Ok(devlanes_sys_windows::file::system_directory()?.join("cmd.exe"))
}

/// A job that ends its members when the test drops it, pass or fail.
fn test_job() -> io::Result<Job> {
    Job::create(JobOptions {
        kill_on_close: true,
        die_on_unhandled_exception: true,
    })
}

/// Starts `cmd.exe /d /c <script>` in `job` with NUL streams.
fn spawn_cmd(script: &str, job: &Job) -> io::Result<win_spawn::Child> {
    let program = cmd()?;
    let line = format!(r#"cmd.exe /d /c "{script}""#);
    win_spawn::spawn(&Spawn {
        program: &program,
        command_line: OsStr::new(&line),
        environment: None,
        cwd: None,
        stdio: Stdio::Handles {
            stdin: Stream::Null,
            stdout: Stream::Null,
            stderr: Stream::Null,
        },
        inherit: &[],
        job: Some(job),
        flags: CreationFlags {
            no_window: true,
            ..CreationFlags::default()
        },
    })
}

/// A child that lives about 30 s unless ended, in `job`.
fn long_child(job: &Job) -> io::Result<win_spawn::Child> {
    spawn_cmd("ping -n 30 127.0.0.1 >NUL", job)
}

/// Polls `condition` until it holds or [`PATIENCE`] passes.
fn eventually(mut condition: impl FnMut() -> io::Result<bool>) -> io::Result<bool> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition()? {
            return Ok(true);
        }
        sleep(Duration::from_millis(20));
    }
    Ok(false)
}

/// Reads `reader` to its end on a thread and returns the text, or fails after
/// [`PATIENCE`].
fn read_all(mut reader: impl Read + Send + 'static) -> io::Result<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = sender.send(reader.read_to_string(&mut text).map(|_| text));
    });
    receiver
        .recv_timeout(PATIENCE)
        .map_err(|_| io::Error::other("output did not end"))?
}
