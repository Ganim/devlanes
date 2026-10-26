//! Process lifecycle contracts; concrete runners own all OS interaction.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{Error, StopSignal};

/// Complete launch input. Deliberately neither serializable nor debug-printable:
/// the environment can contain secrets and must stay in memory.
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub pty: bool,
    pub size: TerminalSize,
}

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl Default for TerminalSize {
    /// Uses the headless terminal size until an interactive viewer takes ownership.
    fn default() -> Self {
        Self {
            columns: 120,
            rows: 40,
        }
    }
}

impl TerminalSize {
    /// The size of a terminal whose output renders its screen (see
    /// [`Unit::renders_screen`]) while no interactive viewer owns it. Such a
    /// terminal breaks long lines at its width in its output and logs, so it
    /// is wide; viewers get its rows re-wrapped at their own size anyway.
    /// Runners start such units at this size when [`LaunchSpec::size`] is the
    /// default, and the daemon sizes them back to it when the last viewer leaves.
    pub const RENDERED_HEADLESS: Self = Self {
        columns: 300,
        rows: 40,
    };
}

/// Signal and escalation delay for an explicit stop request.
#[derive(Debug, Clone, Copy)]
pub struct StopPolicy {
    pub signal: StopSignal,
    pub timeout: Duration,
}

impl Default for StopPolicy {
    /// Requests SIGTERM followed by SIGKILL after ten seconds.
    fn default() -> Self {
        Self {
            signal: StopSignal::Terminate,
            timeout: Duration::from_secs(10),
        }
    }
}

/// How much CPU an idle unit may use. The daemon lowers it for lanes without
/// activity and raises it back on the next request, connection or command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Power {
    /// Runs as started.
    #[default]
    Normal,
    /// Runs at background priority: lower CPU priority, throttled I/O and, where
    /// the OS has them, efficiency cores. Still serves requests.
    Throttled,
    /// Suspended: no thread runs until the unit is set back to `Normal`. Memory,
    /// open sockets and PIDs stay, and the kernel keeps queuing new connections.
    Frozen,
}

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
    /// their own session running and list them in the runner's exit report. For
    /// setup tasks that start daemons (`pg_ctl start`, `redis-server --daemonize yes`).
    KeepDaemonized,
}

/// OS identity used to reject PID reuse during reconciliation and signalling.
/// `start_time` is the platform's stable process birth token, not wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// The Unix session; 0 when unknown, and always on Windows, which has none.
    pub sid: u32,
    pub start_time: u64,
}

/// Terminal result shared by every waiter, including waiters arriving after exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitOutcome {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Source of raw output; PTY output merges stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Terminal,
    Stdout,
    Stderr,
}

/// One bounded output chunk. Lagging consumers must recover from a snapshot/log.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub bytes: Bytes,
}

/// Why a Windows runner did not start a unit, before any process existed.
/// Runners return it as the source of an [`Error::Io`], so the daemon can
/// name the case in its `spawn_failed` error without depending on the runner.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SpawnRefusal {
    /// `shell_missing`, `cwd_too_long`, `not_found`, `unsafe_batch_argument`,
    /// `invalid_command` or `invalid_environment`.
    pub code: &'static str,
    pub message: String,
    /// With `not_found`: the `PATH` directories tried for a bare name, or the
    /// directory an explicit path names.
    pub searched: Vec<PathBuf>,
}

/// Launches daemon-owned processes without binding callers to a particular runner.
#[async_trait]
pub trait Runner: Send + Sync {
    /// Starts a unit in its own session and returns after its identity is available.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn Unit>, Error>;
}

/// A managed process and its tracked descendants; dropping handles must not stop it.
#[async_trait]
pub trait Unit: Send + Sync {
    /// Writes terminal input; pipe-only runners may return `Unsupported`.
    async fn input(&self, bytes: Bytes) -> Result<(), Error>;
    /// Resizes a PTY after the caller has selected the active interactive viewer.
    async fn resize(&self, size: TerminalSize) -> Result<(), Error>;
    /// Signals verified session members and tracked descendants, escalating on timeout.
    async fn stop(&self, policy: StopPolicy) -> Result<(), Error>;
    /// Waits idempotently for exit. Implementations must support concurrent waiters.
    async fn wait(&self) -> Result<ExitOutcome, Error>;
    /// Subscribes to future raw output without blocking process output draining.
    fn output(&self) -> broadcast::Receiver<OutputChunk>;
    /// Returns the leader and known descendants with PID-reuse-resistant identities.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error>;
    /// Moves the unit's process tree to `power`. `Normal` undoes both other levels.
    /// A stop must still work on a `Frozen` unit. Runners without the mechanism
    /// return `Unsupported`, and an exited unit returns `Exited`.
    async fn set_power(&self, power: Power) -> Result<(), Error> {
        let _ = power;
        Err(Error::Unsupported("power levels".into()))
    }
    /// Whether the output is a rendering of the unit's terminal rather than
    /// what its programs wrote: text placed with absolute cursor moves for the
    /// terminal's current size, as a console host that redraws its own screen
    /// sends. Such output only reads right in a grid of that size, so viewers
    /// get redraws of the screen model instead of the raw bytes. A resize
    /// takes effect where the output says so, not when it is asked for: the
    /// first bytes drawn at a new size are preceded by
    /// `ESC[8;<rows>;<columns>t`, whole within one chunk. False for every Unix
    /// runner.
    fn renders_screen(&self) -> bool {
        false
    }
}
