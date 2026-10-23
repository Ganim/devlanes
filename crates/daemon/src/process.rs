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
    /// Only the first line of `message` is sent, cut to the client's bound.
    pub fn failed(self, message: &str) -> std::io::Result<()> {
        self.finish(failure_line(message).as_bytes())
    }

    /// Redirecting stdout matters because a held pipe keeps the CLI's reader waiting forever.
    fn finish(self, line: &[u8]) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut stdout = std::io::stdout().lock();
        match stdout.write_all(line).and_then(|()| stdout.flush()) {
            Ok(()) => {}
            // The client stopped waiting (its start timeout passed). This daemon
            // still serves, and the client's next command connects to it.
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                tracing::info!("the spawning client stopped waiting for readiness");
            }
            Err(error) => return Err(error),
        }
        let null = File::options().write(true).open("/dev/null")?;
        rustix::stdio::dup2_stdout(&null)?;
        Ok(())
    }
}

/// Longest failure line, newline excluded. The spawning client reads at most
/// `devlanes_client::READY_LINE_LIMIT` bytes, newline included, so a longer line
/// would arrive without its end.
const FAILURE_LINE_MAX: usize = 4095;

/// The first line of `message`, cut to [`FAILURE_LINE_MAX`] bytes on a character
/// boundary, with its newline.
fn failure_line(message: &str) -> String {
    let first = message.lines().next().unwrap_or_default();
    let mut line = first[..first.floor_char_boundary(FAILURE_LINE_MAX)].to_owned();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// A long reason keeps everything the client reads, and the cut never splits
    /// a character or exceeds the client's bound.
    fn failure_lines_fit_the_client_bound() {
        assert_eq!(
            FAILURE_LINE_MAX + 1,
            usize::try_from(devlanes_client::READY_LINE_LIMIT).expect("fits")
        );
        assert_eq!(failure_line("CODE: first\nsecond"), "CODE: first\n");
        let long = format!("CODE: {}", "x".repeat(1000));
        assert_eq!(failure_line(&long), format!("{long}\n"));
        let wide = "é".repeat(FAILURE_LINE_MAX);
        let line = failure_line(&wide);
        assert!(line.len() <= FAILURE_LINE_MAX + 1);
        assert!(line.trim_end().chars().all(|c| c == 'é'));
    }
}
