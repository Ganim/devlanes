//! Daemon process setup required by the client's auto-spawn contract.

#[cfg(unix)]
use std::fs::File;
use std::io::Write;

/// Leaves the spawning terminal's session so terminal hangups and job control never reach units.
#[cfg(unix)]
pub fn detach() -> std::io::Result<()> {
    match rustix::process::setsid() {
        Ok(_) => Ok(()),
        // EPERM means this process already leads a session, e.g. under launchd or systemd.
        Err(rustix::io::Errno::PERM) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Nothing to do: on Windows the spawning client starts the daemon without a console
/// (`DETACHED_PROCESS`), which is what keeps terminal events away from it.
#[cfg(windows)]
pub fn detach() -> std::io::Result<()> {
    Ok(())
}

/// Routes panics through `tracing`, so a crash leaves its message, location and
/// backtrace in `daemon.log` (the daemon has no terminal). The previous hook still
/// runs afterwards. Call once, after logging is initialized.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into());
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_default();
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%message, %location, %thread, %backtrace, "panic");
        previous(info);
    }));
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

    /// Reports a startup failure; the spawning client shows the line as the error.
    /// The client reads at most 128 bytes, so longer messages are cut.
    pub fn failed(self, message: &str) -> std::io::Result<()> {
        let mut line: String = message.lines().next().unwrap_or_default().to_owned();
        while line.len() > 126 {
            line.pop();
        }
        line.push('\n');
        self.finish(line.as_bytes())
    }

    /// Redirecting stdout matters because a held pipe keeps the CLI's reader waiting forever.
    fn finish(self, line: &[u8]) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(line)?;
        stdout.flush()?;
        release_stdout()
    }
}

/// Points stdout at `/dev/null`, closing the client's pipe.
#[cfg(unix)]
fn release_stdout() -> std::io::Result<()> {
    let null = File::options().write(true).open("/dev/null")?;
    rustix::stdio::dup2_stdout(&null)?;
    Ok(())
}

/// Keeps the pipe for now: the client stops reading at the newline, so it is not
/// blocked. Swapping the standard handle for `NUL` needs a Win32 call from `sys`.
#[cfg(windows)]
fn release_stdout() -> std::io::Result<()> {
    Ok(())
}
