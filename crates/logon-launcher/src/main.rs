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
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use devlanes_client::Paths;

    /// Why the daemon was not started.
    #[derive(Debug, thiserror::Error)]
    pub(crate) enum LaunchError {
        /// The devlanes home could not be resolved.
        #[error("no devlanes home: {0}")]
        Home(#[from] devlanes_client::Error),
        /// `devlanes.exe` could not be started.
        #[error("cannot start {}: {source}", program.display())]
        Spawn {
            program: PathBuf,
            source: std::io::Error,
        },
    }

    /// Starts the daemon for `home` (the default home without one). A failure
    /// to start it is also appended to the home's `logs\daemon.log`, where
    /// `devlanes daemon logs` and support bundles show it: nobody sees this
    /// program's exit code at logon.
    pub(crate) fn start(home: Option<&OsStr>) -> Result<(), LaunchError> {
        let paths = match home {
            Some(home) => Paths::new(home)?,
            None => Paths::discover()?,
        };
        // The client prepares the home before its spawn too. A failure is the
        // daemon's to report; without the directory there is no log to write.
        let _ = paths.prepare();
        spawn(&paths).inspect_err(|error| record(&paths.logs, error))
    }

    /// Starts `devlanes.exe daemon run` from this executable's directory.
    ///
    /// The daemon gets the environment the CLI's own spawn gives it (an
    /// allowlist, `DEVLANES_HOME` set) and the home as its working directory,
    /// or this executable's directory while the home does not exist yet: a
    /// daemon running in a worktree would keep Windows from deleting it. The
    /// flags are the CLI's ([`devlanes_client::spawn_detached`]), so the daemon
    /// has no console and no window appears.
    fn spawn(paths: &Paths) -> Result<(), LaunchError> {
        let exe = std::env::current_exe().map_err(|source| LaunchError::Spawn {
            program: PathBuf::from("devlanes.exe"),
            source,
        })?;
        let devlanes = exe.with_file_name("devlanes.exe");
        let cwd = if paths.home.is_dir() {
            paths.home.as_path()
        } else {
            exe.parent().unwrap_or(Path::new("."))
        };
        let mut command = Command::new(&devlanes);
        command
            .args(["daemon", "run"])
            .env_clear()
            .envs(devlanes_client::daemon_environment())
            .env("DEVLANES_HOME", &paths.home)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // The daemon outlives this launcher; nothing waits for it.
        devlanes_client::spawn_detached(|flags| command.creation_flags(flags).spawn())
            .map(drop)
            .map_err(|source| LaunchError::Spawn {
                program: devlanes,
                source,
            })
    }

    /// Appends `error` to `logs\daemon.log` in the daemon's own line format.
    /// Best effort: without the file there is nowhere else to say it.
    fn record(logs: &Path, error: &LaunchError) {
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.join("daemon.log"))
        else {
            return;
        };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(%error, "the logon launcher could not start the daemon");
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A failed start is appended to the home's daemon log, after what is
        /// already there.
        #[test]
        fn failures_are_appended_to_the_daemon_log() -> std::io::Result<()> {
            let logs = tempfile::tempdir()?;
            std::fs::write(logs.path().join("daemon.log"), "earlier line\n")?;
            let error = LaunchError::Spawn {
                program: PathBuf::from(r"C:\missing\devlanes.exe"),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            };
            record(logs.path(), &error);
            let log = std::fs::read_to_string(logs.path().join("daemon.log"))?;
            let mut lines = log.lines();
            assert_eq!(lines.next(), Some("earlier line"));
            let line = lines.next().unwrap_or_default();
            assert!(line.contains("ERROR"), "{log}");
            assert!(
                line.contains(r"cannot start C:\missing\devlanes.exe"),
                "{log}"
            );
            assert!(line.contains("could not start the daemon"), "{log}");
            Ok(())
        }
    }
}
