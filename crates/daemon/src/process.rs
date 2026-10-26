//! Daemon process setup required by the client's auto-spawn contract.

#[cfg(unix)]
use std::fs::File;
use std::io::Write;

/// Gives a daemon started without a console (by the logon Run key, since the
/// binary's manifest asks Windows not to create one) a hidden console of its
/// own, before anything is spawned: every program it starts shares that
/// console, where without one each would open a window. A daemon the client
/// spawned already has one (`CREATE_NO_WINDOW`) and keeps it.
///
/// Then clears the ignore-Ctrl-C flag that `CREATE_NEW_PROCESS_GROUP` set when
/// the client spawned the daemon, so the units it starts inherit normal Ctrl-C
/// handling and a stop can reach them. The daemon's own console is hidden and
/// of its own, so no terminal's Ctrl-C reaches it either way.
#[cfg(windows)]
pub fn detach() -> std::io::Result<()> {
    use devlanes_sys_windows::console;
    console::alloc_console(console::AllocMode::NoWindow)?;
    console::set_ctrl_c_ignored(false)
}

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

/// The spawning client reads one answer line from stdout, then the pipe must
/// close. Step lines before the answer are optional: they are written only
/// when the client set `DEVLANES_READY_STEPS=1`, since a client without them
/// reads the first line as the answer.
pub struct Readiness {
    enabled: bool,
    /// The client reads step lines ([`READY_STEPS_ENV`]).
    steps: bool,
}

impl Readiness {
    /// Only `DEVLANES_READY_FD=1` is supported; service managers start without
    /// the variable. Step lines also need `DEVLANES_READY_STEPS=1`.
    pub fn from_env() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| value == "1");
        Self {
            enabled: set("DEVLANES_READY_FD"),
            steps: set(READY_STEPS_ENV),
        }
    }

    /// A client spawned this daemon on demand (it waits for the readiness line);
    /// a service manager starts it without one.
    pub fn spawned_by_client(&self) -> bool {
        self.enabled
    }

    /// Logs that the startup step `step` begins and, when the spawning client
    /// asked for steps, reports it as a `starting: <step>` line, which
    /// restarts the client's per-step wait. A client that stopped waiting is
    /// ignored: the daemon starts anyway.
    pub fn step(&self, step: &str) {
        tracing::debug!(step, "startup step");
        // A client that did not ask for steps takes the first line as its answer.
        if !self.enabled || !self.steps {
            return;
        }
        let line = format!("{STEP_PREFIX}{step}\n");
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = stdout
            .write_all(line.as_bytes())
            .and_then(|()| stdout.flush())
        {
            tracing::debug!(%error, step, "startup step not reported");
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
    /// Only the first line of `message` is sent, cut to the client's bound;
    /// one that starts like a step line is prefixed with `error: `.
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
            // still serves, and the client's next command connects to it. A
            // closed Windows pipe (ERROR_NO_DATA) maps to BrokenPipe too.
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                tracing::info!("the spawning client stopped waiting for readiness");
            }
            Err(error) => return Err(error),
        }
        #[cfg(unix)]
        {
            let null = File::options().write(true).open("/dev/null")?;
            rustix::stdio::dup2_stdout(&null)?;
        }
        // Points stdout at NUL and closes the pipe's handle, so the client's
        // reader sees end of file.
        #[cfg(windows)]
        devlanes_sys_windows::console::release_stdout()?;
        Ok(())
    }
}

/// Prefix of a startup step line; the spawning client reads it as
/// `devlanes_client::STARTUP_STEP`.
const STEP_PREFIX: &str = "starting: ";

/// Set to `1` by clients that read step lines; the client names it
/// `devlanes_client::READY_STEPS_ENV`.
const READY_STEPS_ENV: &str = "DEVLANES_READY_STEPS";

/// Longest failure line, newline excluded. The spawning client reads at most
/// `devlanes_client::READY_LINE_LIMIT` bytes, newline included, so a longer line
/// would arrive without its end.
const FAILURE_LINE_MAX: usize = 4095;

/// Put before a failure message that starts with [`STEP_PREFIX`].
const FAILURE_PREFIX: &str = "error: ";

/// The first line of `message`, cut to [`FAILURE_LINE_MAX`] bytes on a character
/// boundary, with its newline. A message that starts like a step line gets
/// [`FAILURE_PREFIX`], or the client would read it as a step and go on
/// waiting.
fn failure_line(message: &str) -> String {
    let first = message.lines().next().unwrap_or_default();
    let first = if first.starts_with(STEP_PREFIX) {
        format!("{FAILURE_PREFIX}{first}")
    } else {
        first.to_owned()
    };
    let mut line = first[..first.floor_char_boundary(FAILURE_LINE_MAX)].to_owned();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// A long reason keeps everything the client reads, and the cut never splits
    /// a character or exceeds the client's bound. Step lines use the client's
    /// prefix and opt-in variable.
    fn failure_lines_fit_the_client_bound() {
        assert_eq!(
            FAILURE_LINE_MAX + 1,
            usize::try_from(devlanes_client::READY_LINE_LIMIT).expect("fits")
        );
        assert_eq!(STEP_PREFIX, devlanes_client::STARTUP_STEP);
        assert_eq!(READY_STEPS_ENV, devlanes_client::READY_STEPS_ENV);
        assert_eq!(crate::STARTING_MARKER, devlanes_client::STARTING_MARKER);
        assert_eq!(failure_line("CODE: first\nsecond"), "CODE: first\n");
        let long = format!("CODE: {}", "x".repeat(1000));
        assert_eq!(failure_line(&long), format!("{long}\n"));
        let wide = "é".repeat(FAILURE_LINE_MAX);
        let line = failure_line(&wide);
        assert!(line.len() <= FAILURE_LINE_MAX + 1);
        assert!(line.trim_end().chars().all(|c| c == 'é'));
    }

    /// A failure that starts like a step line is prefixed, so the client takes
    /// it as the answer; the prefix counts toward the client's bound.
    #[test]
    fn failures_never_read_as_steps() {
        assert_eq!(
            failure_line("starting: proxy failed\nmore"),
            "error: starting: proxy failed\n"
        );
        assert_eq!(failure_line("CODE: starting: x"), "CODE: starting: x\n");
        let long = format!("{STEP_PREFIX}{}", "x".repeat(FAILURE_LINE_MAX));
        let line = failure_line(&long);
        assert_eq!(line.len(), FAILURE_LINE_MAX + 1);
        assert!(line.starts_with("error: starting: "));
    }
}
