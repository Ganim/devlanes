//! Runner for daemon-owned units on Windows: each unit's leader starts inside its
//! own Job Object and is waited on through its process handle. The Win32 calls live in `devlanes-sys`; this crate has no
//! `unsafe`.
//!
//! Descendants are tracked from job membership plus periodic process snapshots by
//! parent, so stops and exit sweeps also reach processes that broke away from the
//! job. `pty: true` units run in a pseudo console (ConPTY), the others over pipes.
//!
//! [`command`] (resolving a program and building its command line) is pure and
//! compiled on every OS, so its rules are tested everywhere.

pub mod command;

#[cfg(windows)]
mod unit;

#[cfg(windows)]
pub use runner::WindowsRunner;
#[cfg(windows)]
pub use unit::{ExitReport, StopOutcome, WindowsUnit};

#[cfg(windows)]
mod runner {
    use std::{sync::Arc, time::Duration};

    use async_trait::async_trait;
    use devlanes_core::{Error, ExitSweep, LaunchSpec, Runner, StderrMode, Unit};

    use crate::unit::{WindowsUnit, spawn_unit};

    /// Spawns units in job objects with piped output. Cheap to clone; holds only
    /// tuning values.
    #[derive(Debug, Clone)]
    pub struct WindowsRunner {
        pub(crate) stderr: StderrMode,
        pub(crate) track_interval: Duration,
        pub(crate) output_capacity: usize,
        pub(crate) drain_timeout: Duration,
        pub(crate) sweep_timeout: Duration,
        pub(crate) exit_sweep: ExitSweep,
    }

    impl Default for WindowsRunner {
        /// Separate stderr, 500 ms descendant snapshots, 1024 buffered chunks per
        /// subscriber, leftovers ended after a natural exit.
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

    impl WindowsRunner {
        /// Selects the natural-exit sweep for units spawned by this runner.
        pub fn with_exit_sweep(mut self, sweep: ExitSweep) -> Self {
            self.exit_sweep = sweep;
            self
        }

        /// Selects stderr handling.
        pub fn with_stderr(mut self, mode: StderrMode) -> Self {
            self.stderr = mode;
            self
        }

        /// Sets how often descendants are snapshotted.
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
        /// methods. The environment is exactly `spec.env`; nothing is inherited from
        /// the daemon. `program` is resolved against the `PATH` and `PATHEXT` in
        /// `spec.env`.
        ///
        /// Not cancellation safe: process creation runs on the blocking pool and
        /// finishes even if this future is dropped, leaving an unsupervised unit.
        pub async fn spawn_unit(&self, spec: LaunchSpec) -> Result<Arc<WindowsUnit>, Error> {
            spawn_unit(self, spec).await
        }
    }

    #[async_trait]
    impl Runner for WindowsRunner {
        /// Spawns through [`WindowsRunner::spawn_unit`], with the same caveat.
        async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn Unit>, Error> {
            Ok(self.spawn_unit(spec).await?)
        }
    }
}
