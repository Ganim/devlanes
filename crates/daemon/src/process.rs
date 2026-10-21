//! Daemon process setup required by the client's auto-spawn contract.

use std::{fs::File, io::Write};

/// Leaves the spawning terminal's session so terminal hangups and job control never reach units.
pub fn detach() -> std::io::Result<()> {
    match rustix::process::setsid() {
        Ok(_) => Ok(()),
        // EPERM means this process already leads a session, e.g. under launchd or systemd.
        Err(rustix::io::Errno::PERM) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The spawning client reads exactly one line from stdout, then the pipe must close.
pub struct Readiness {
    enabled: bool,
}

impl Readiness {
    /// Only `DEVLANES_READY_FD=1` is supported; service managers start without the variable.
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var_os("DEVLANES_READY_FD").is_some_and(|fd| fd == "1"),
        }
    }

    /// Signals that the socket accepts requests, then releases the client's pipe.
    pub fn ready(self) -> std::io::Result<()> {
        self.finish(b"ready\n")
    }

    /// Losing the lock race is a success for the spawning client, which then connects.
    pub fn already_running(self) -> std::io::Result<()> {
        self.finish(b"already-running\n")
    }

    /// Redirecting stdout matters because a held pipe keeps the CLI's reader waiting forever.
    fn finish(self, line: &[u8]) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(line)?;
        stdout.flush()?;
        let null = File::options().write(true).open("/dev/null")?;
        rustix::stdio::dup2_stdout(&null)?;
        Ok(())
    }
}
