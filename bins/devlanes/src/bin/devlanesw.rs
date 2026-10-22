//! `devlanesw.exe`: starts the devlanes daemon without a console window. It is
//! what the logon entry `devlanes daemon install` writes on Windows runs.
//!
//! Explorer starts the programs of the `Run` key with a visible console when they
//! are console programs, as `devlanes.exe` is. This one is a GUI program, so it
//! gets no console, and it starts `devlanes.exe daemon run` next to it without
//! one either, then exits. The `Run` key has no environment of its own, so the
//! devlanes home comes as `--home <path>`.
//!
//! On other systems it only explains that it is Windows-only: services start
//! `devlanes daemon run` directly there.

// Ignored outside Windows.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

/// `devlanesw [--home <path>]`: exit 0 once the daemon was started, 1 when it
/// could not be, 2 for other arguments. A daemon already running for the home
/// makes the new one exit quietly.
#[cfg(windows)]
fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let home = match (args.next(), args.next(), args.next()) {
        (None, _, _) => None,
        (Some(flag), Some(home), None) if flag == "--home" => Some(home),
        _ => return ExitCode::from(2),
    };
    match launcher::start(home.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

/// Explains that this launcher exists for Windows only.
#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("devlanesw is the Windows logon launcher; run `devlanes daemon run` instead");
    ExitCode::from(1)
}

#[cfg(windows)]
mod launcher {
    use std::ffi::OsStr;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use devlanes_sys::windows::process::{CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS};

    /// Starts `devlanes.exe daemon run` from this executable's directory, with
    /// `DEVLANES_HOME` set to `home` when given.
    ///
    /// `DETACHED_PROCESS` gives the daemon no console, so no window appears.
    /// Breaking away from a job matters when this runs from a terminal that kills
    /// its job on close; a job that forbids it refuses with access denied, and
    /// the daemon then starts inside it, as the CLI's own spawn does.
    pub(crate) fn start(home: Option<&OsStr>) -> std::io::Result<()> {
        let devlanes = std::env::current_exe()?.with_file_name("devlanes.exe");
        let mut command = Command::new(devlanes);
        command
            .args(["daemon", "run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(home) = home {
            command.env("DEVLANES_HOME", home);
        }
        command.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        let child = match command.spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                command.creation_flags(DETACHED_PROCESS);
                command.spawn()?
            }
            result => result?,
        };
        // The daemon outlives this launcher; nothing waits for it.
        drop(child);
        Ok(())
    }
}
