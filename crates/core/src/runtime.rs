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

/// What happens to processes still running after a unit's leader exits on its own.
/// An explicit stop always kills every tracked process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitSweep {
    /// Kill every tracked leftover; for services and ad-hoc commands.
    #[default]
    KillAll,
    /// Kill leftovers that still belong to the unit, but leave descendants that
    /// detached from it (their own session on Unix) running and report them as
    /// left running. For setup tasks that start daemons (`pg_ctl start`,
    /// `redis-server --daemonize yes`).
    KeepDaemonized,
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

/// OS identity used to reject PID reuse during reconciliation and signalling.
/// `start_time` is the platform's stable process birth token, not wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// The unit's group token, as `devlanes_sys::ProcessInfo::identity` fills
    /// it. On Unix it is the session ID, shared by the leader and every member
    /// that stayed in its session, and 0 when the OS did not report it. Windows
    /// has no group a process can be found or stopped by after a daemon crash,
    /// so it is the process's own PID: every job member then differs from the
    /// leader and is recorded on its own for orphan cleanup, which compares
    /// this field with the leader's.
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
}
