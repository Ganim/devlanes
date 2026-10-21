//! PTY and pipe runner for daemon-owned units.
//!
//! Every unit leads its own session (`setsid`, plus the PTY as controlling terminal),
//! is reaped by PID on SIGCHLD and never through `waitpid(-1)`, so `tokio::process`
//! helpers keep their exit statuses. Descendants are tracked from periodic process
//! snapshots so stop and exit sweeps also reach processes that left the session.
//!
//! Requires a tokio runtime with IO, time and signal drivers enabled.
//!
//! ```no_run
//! use devlanes_core::{LaunchSpec, Runner, StopPolicy, TerminalSize};
//! use devlanes_runner_pty::PtyRunner;
//!
//! # async fn demo() -> Result<(), devlanes_core::Error> {
//! let unit = PtyRunner::default()
//!     .spawn(LaunchSpec {
//!         program: "/bin/sh".into(),
//!         args: vec!["-c".into(), "echo hi; sleep 5".into()],
//!         cwd: "/".into(),
//!         env: [("PATH".into(), "/usr/bin:/bin".into())].into(),
//!         pty: true,
//!         size: TerminalSize::default(),
//!     })
//!     .await?;
//! let mut output = unit.output(); // the first subscriber sees output from launch
//! unit.stop(StopPolicy::default()).await?;
//! let exit = unit.wait().await?;
//! # let _ = (output.try_recv(), exit);
//! # Ok(())
//! # }
//! ```

// Unsafe code is limited to the pre-exec hook in `spawn` and master registration in
// `unit`; each block documents its invariants.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use devlanes_core::{Error, LaunchSpec, Runner, Unit};

mod output;
mod snapshot;
#[allow(unsafe_code)]
mod spawn;
mod unit;

pub use devlanes_sys::{ListeningSocket, ProcessInfo};
pub use unit::{ExitReport, PtyUnit, StopOutcome};

/// How pipe-mode units (`pty: false`) deliver stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StderrMode {
    /// Separate pipes; chunks are tagged `Stdout` or `Stderr`.
    #[default]
    Separate,
    /// One shared pipe preserving write order; every chunk is tagged `Stdout`.
    Merged,
}

/// What happens to processes still running after a unit's leader exits on its own.
/// An explicit stop always kills every tracked process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitSweep {
    /// SIGKILL every tracked leftover; for services and ad-hoc commands.
    #[default]
    KillAll,
    /// Kill leftovers inside the unit's session, but leave descendants that moved to
    /// their own session running and list them in [`ExitReport::left_running`]. For
    /// setup tasks that start daemons (`pg_ctl start`, `redis-server --daemonize yes`).
    KeepDaemonized,
}

/// Spawns units in PTYs or pipes. Cheap to clone; holds only tuning values.
#[derive(Debug, Clone)]
pub struct PtyRunner {
    stderr: StderrMode,
    track_interval: Duration,
    output_capacity: usize,
    drain_timeout: Duration,
    sweep_timeout: Duration,
    exit_sweep: ExitSweep,
}

impl Default for PtyRunner {
    /// Separate stderr, 500 ms descendant snapshots, 1024 buffered chunks per
    /// subscriber, leftovers killed after a natural exit.
    fn default() -> Self {
        Self {
            stderr: StderrMode::Separate,
            track_interval: Duration::from_millis(500),
            output_capacity: 1024,
            drain_timeout: Duration::from_millis(500),
            sweep_timeout: Duration::from_secs(1),
            exit_sweep: ExitSweep::KillAll,
        }
    }
}

impl PtyRunner {
    /// Selects the natural-exit sweep for units spawned by this runner.
    pub fn with_exit_sweep(mut self, sweep: ExitSweep) -> Self {
        self.exit_sweep = sweep;
        self
    }

    /// Selects stderr handling for pipe-mode units.
    pub fn with_stderr(mut self, mode: StderrMode) -> Self {
        self.stderr = mode;
        self
    }

    /// Sets how often descendants are snapshotted. Shorter intervals catch processes
    /// that fork and orphan quickly outside the session, at the cost of process-table scans.
    pub fn with_track_interval(mut self, interval: Duration) -> Self {
        self.track_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// Sets the per-subscriber chunk buffer; slower subscribers observe `Lagged`.
    pub fn with_output_capacity(mut self, capacity: usize) -> Self {
        self.output_capacity = capacity.max(1);
        self
    }

    /// Spawns a unit and returns the concrete handle with identity and report methods.
    ///
    /// Returns after the leader's identity was recorded. The environment is exactly
    /// `spec.env`; nothing is inherited from the daemon. `program` is resolved against
    /// the `PATH` in `spec.env`.
    ///
    /// Not cancellation safe: the fork runs on the blocking pool and finishes even if
    /// this future is dropped, and a leader started that way is never supervised or
    /// reaped. Await it to completion, or spawn it as its own task.
    pub async fn spawn_unit(&self, spec: LaunchSpec) -> Result<Arc<PtyUnit>, Error> {
        unit::spawn(self, spec).await
    }
}

#[async_trait]
impl Runner for PtyRunner {
    /// Spawns through [`PtyRunner::spawn_unit`], with the same cancellation caveat.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn Unit>, Error> {
        Ok(self.spawn_unit(spec).await?)
    }
}
