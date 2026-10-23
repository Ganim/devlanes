//! Runner for daemon-owned units on Windows.
//!
//! Every unit starts inside its own job object, which its processes cannot
//! leave, so the job's member list is the unit's process tree; no process
//! snapshots are taken. The leader runs with a hidden console of its own
//! (`CREATE_NO_WINDOW`), `NUL` as stdin and pipes for its output, and is
//! waited on through its process handle. A `pty: true` leader runs in a pseudo
//! console (ConPTY) instead, whose output is the console host's rendering of
//! its screen ([`devlanes_core::Unit::renders_screen`]). All Win32 calls go
//! through `devlanes-sys-windows`.
//!
//! A `/bin/sh` program (what `run:` strings resolve to) runs as Git for
//! Windows' `sh.exe`, given with [`WindowsRunner::with_shell`].
//!
//! Graceful stops post `WM_CLOSE` to the unit's windows and send Ctrl-C or
//! Ctrl-Break to its console through a short-lived helper process, the
//! `devlanes __console-ctrl` entry point that [`console_ctrl_main`] implements.
//! [`stop_orphan`] stops a unit a previous daemon left running the same way,
//! from its recorded processes.
//!
//! Requires a tokio runtime with IO and time drivers enabled.
//!
//! ```no_run
//! # #[cfg(windows)] mod demo {
//! use devlanes_core::{LaunchSpec, Runner, StopPolicy, TerminalSize};
//! use devlanes_runner_windows::WindowsRunner;
//!
//! # async fn demo() -> Result<(), devlanes_core::Error> {
//! let unit = WindowsRunner::default()
//!     .spawn(LaunchSpec {
//!         program: "fixture-server".into(),
//!         args: vec!["--port".into(), "0".into()],
//!         cwd: r"C:\work".into(),
//!         env: [("PATH".into(), r"C:\tools".into())].into(),
//!         pty: false,
//!         size: TerminalSize::default(),
//!     })
//!     .await?;
//! let mut output = unit.output(); // the first subscriber sees output from launch
//! unit.stop(StopPolicy::default()).await?;
//! let exit = unit.wait().await?;
//! # let _ = (output.try_recv(), exit);
//! # Ok(())
//! # }
//! # }
//! ```

// Job objects and consoles are Windows; other platforms use devlanes-runner-pty.
#![cfg(windows)]
#![deny(unsafe_code)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use devlanes_core::{Error, LaunchSpec, Runner, Unit};

mod command;
mod ctrl;
mod foreground;
mod orphan;
mod output;
mod spawn;
mod terminal;
mod unit;

pub use ctrl::{ConsoleHelper, HELPER_ARG, console_ctrl_main};
pub use devlanes_core::{ExitSweep, StderrMode};
pub use devlanes_sys::{ListeningSocket, ProcessInfo};
pub use foreground::{shell_exit_code, spawn_foreground};
pub use orphan::{OrphanStop, stop_orphan};
pub use spawn::SpawnError;
pub use unit::{ExitReport, StopOutcome, WindowsUnit};

/// Spawns units in job objects with piped output. Cheap to clone; holds only
/// tuning values, the shell and the console helper's command.
#[derive(Debug, Clone)]
pub struct WindowsRunner {
    pub(crate) stderr: StderrMode,
    pub(crate) track_interval: Duration,
    pub(crate) output_capacity: usize,
    pub(crate) drain_timeout: Duration,
    pub(crate) sweep_timeout: Duration,
    pub(crate) shell: Option<PathBuf>,
    pub(crate) helper: Option<ConsoleHelper>,
}

impl Default for WindowsRunner {
    /// Separate stderr, 500 ms membership refreshes, 1024 buffered chunks per
    /// subscriber, no shell, and this executable as the console helper (none
    /// when its path cannot be read, which leaves stops to `WM_CLOSE` and the
    /// timeout).
    fn default() -> Self {
        Self {
            stderr: StderrMode::Separate,
            track_interval: Duration::from_millis(500),
            output_capacity: 1024,
            drain_timeout: Duration::from_millis(500),
            sweep_timeout: Duration::from_secs(1),
            shell: None,
            helper: ConsoleHelper::current_exe().ok(),
        }
    }
}

impl WindowsRunner {
    /// Accepted for the same API as `devlanes-runner-pty`. On Windows every
    /// sweep is [`ExitSweep::KillAll`]: a unit's processes cannot leave its
    /// job, so daemons a task starts are ended with it.
    pub fn with_exit_sweep(self, _sweep: ExitSweep) -> Self {
        self
    }

    /// Selects stderr handling for pipe-mode units.
    pub fn with_stderr(mut self, mode: StderrMode) -> Self {
        self.stderr = mode;
        self
    }

    /// Runs `/bin/sh` programs as `shell`, Git for Windows' `usr\bin\sh.exe`.
    /// Without one, such units fail with [`SpawnError::ShellMissing`].
    pub fn with_shell(mut self, shell: Option<PathBuf>) -> Self {
        self.shell = shell;
        self
    }

    /// Sends console control events through `helper` instead of this
    /// executable's `__console-ctrl` entry point.
    pub fn with_console_helper(mut self, helper: ConsoleHelper) -> Self {
        self.helper = Some(helper);
        self
    }

    /// Sets how often job membership is refreshed for subjects and stops.
    pub fn with_track_interval(mut self, interval: Duration) -> Self {
        self.track_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// Sets the per-subscriber chunk buffer; slower subscribers observe `Lagged`.
    pub fn with_output_capacity(mut self, capacity: usize) -> Self {
        self.output_capacity = capacity.max(1);
        self
    }

    /// Spawns a unit and returns the concrete handle with identity and report
    /// methods.
    ///
    /// Returns once the leader runs in its job. The environment is exactly
    /// `spec.env` (for the shell, plus the MSYS variables and Git's `usr\bin`
    /// at the end of `PATH`); nothing is inherited from the daemon. `program` is resolved against the `PATH` and `PATHEXT`
    /// in `spec.env`. Failures before the leader started leave nothing behind.
    ///
    /// Not cancellation safe: process creation runs on the blocking pool and
    /// finishes even if this future is dropped, leaving an unsupervised unit.
    pub async fn spawn_unit(&self, spec: LaunchSpec) -> Result<Arc<WindowsUnit>, Error> {
        unit::spawn(self, spec).await
    }
}

#[async_trait]
impl Runner for WindowsRunner {
    /// Spawns through [`WindowsRunner::spawn_unit`], with the same caveat.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn Unit>, Error> {
        Ok(self.spawn_unit(spec).await?)
    }
}
