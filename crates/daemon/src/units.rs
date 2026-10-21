//! One host per running unit: it owns the output pump, the screen model used for
//! attach snapshots, the log writer thread and the lifecycle state other parts watch.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use devlanes_core::{ExitOutcome, ProcessIdentity, StopPolicy, TerminalSize, Unit};
use devlanes_logs::{LogOptions, UnitLog};
use devlanes_proto::{Actor, LogLine, LogQuery, Page, RpcError};
use serde_json::json;
use tokio::sync::{broadcast, watch};

/// Daemon-side scrollback stays small; deep history comes from log segments.
const SCROLLBACK: usize = 200;
/// Bounds per-unit raw memory; 40 units must not cost hundreds of MiB.
const RAW_RING: usize = 1024 * 1024;

/// Lifecycle as exposed through `UnitSummary.state`.
#[derive(Debug, Clone, PartialEq)]
pub enum UnitState {
    Starting,
    Ready,
    /// Readiness failed or the process exited non-zero before/after becoming ready.
    Failed(Option<RpcError>),
    Stopping,
    /// Exited after an explicit stop, or a task/exec that finished with code 0.
    Exited(ExitOutcome),
}

impl UnitState {
    /// Stable lowercase words agents match on (cli-design 3.4).
    pub fn word(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed(_) => "failed",
            Self::Stopping => "stopping",
            Self::Exited(_) => "stopped",
        }
    }

    /// Terminal states never transition again for this run.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Exited(_)) || matches!(self, Self::Failed(_))
    }
}

/// Static facts about a run, fixed at spawn.
pub struct UnitInfo {
    pub name: String,
    pub lane: String,
    pub kind: &'static str,
    pub actor: Actor,
    pub started_at_ms: u64,
    pub log_dir: PathBuf,
}

/// Commands for the log thread; blocking file I/O never runs on the async executor.
enum LogCommand {
    Output(Bytes, u64),
    /// Answered once every earlier command was applied, so queries see all output.
    Sync(tokio::sync::oneshot::Sender<()>),
    Finish,
}

/// Shared handle; cloning is cheap and never affects the process.
#[derive(Clone)]
pub struct UnitHost {
    inner: Arc<Inner>,
}

struct Inner {
    info: UnitInfo,
    unit: Arc<dyn Unit>,
    identity: Option<ProcessIdentity>,
    state: watch::Sender<UnitState>,
    exit: watch::Sender<Option<ExitOutcome>>,
    /// The screen lock also orders re-broadcast, so an attacher's snapshot and its
    /// subscription never leave a gap or a duplicate.
    screen: StdMutex<vt100::Parser>,
    live: broadcast::Sender<Bytes>,
    log: Arc<StdMutex<UnitLog>>,
    log_tx: std::sync::mpsc::Sender<LogCommand>,
    /// Set when someone asked for a stop, so a signal exit reads as `stopped`, not `failed`.
    stop_requested: std::sync::atomic::AtomicBool,
    /// Flips once the pump saw the end of output, so the exit is published after it.
    pump_done: watch::Sender<bool>,
}

/// How long the exit waits for remaining output to reach the log after the leader died.
const PUMP_DRAIN: Duration = Duration::from_secs(1);

impl UnitHost {
    /// Starts pumping output immediately so a chatty process never blocks on a full PTY.
    pub fn start(
        info: UnitInfo,
        unit: Arc<dyn Unit>,
        identity: Option<ProcessIdentity>,
    ) -> Result<Self, RpcError> {
        let log = UnitLog::open(
            &info.log_dir,
            info.name.replace('#', "_"),
            LogOptions {
                raw_ring_bytes: RAW_RING,
                ..LogOptions::default()
            },
        )
        .map_err(|error| internal("log_open_failed", error))?;
        let log = Arc::new(StdMutex::new(log));
        let (log_tx, log_rx) = std::sync::mpsc::channel();
        spawn_log_thread(log.clone(), log_rx);
        let size = TerminalSize::default();
        let host = Self {
            inner: Arc::new(Inner {
                info,
                unit,
                identity,
                state: watch::channel(UnitState::Starting).0,
                exit: watch::channel(None).0,
                screen: StdMutex::new(vt100::Parser::new(
                    cells(size.rows),
                    cells(size.columns),
                    SCROLLBACK,
                )),
                live: broadcast::channel(1024).0,
                log,
                log_tx,
                stop_requested: std::sync::atomic::AtomicBool::new(false),
                pump_done: watch::channel(false).0,
            }),
        };
        host.spawn_pump();
        host.spawn_waiter();
        Ok(host)
    }

    /// Run facts for summaries.
    pub fn info(&self) -> &UnitInfo {
        &self.inner.info
    }

    /// The process identity recorded in `unit_runs`, if the runner reported one.
    pub fn identity(&self) -> Option<ProcessIdentity> {
        self.inner.identity
    }

    /// Current lifecycle state.
    pub fn state(&self) -> UnitState {
        self.inner.state.borrow().clone()
    }

    /// Watches transitions; used by `wait`, `after: services` and event publishing.
    pub fn watch_state(&self) -> watch::Receiver<UnitState> {
        self.inner.state.subscribe()
    }

    /// Moves to a new state unless the run already reached a terminal state.
    pub fn set_state(&self, next: UnitState) {
        self.inner.state.send_if_modified(|state| {
            if state.is_terminal() || *state == next {
                return false;
            }
            *state = next;
            true
        });
    }

    /// The exit outcome once known; shared by every waiter.
    pub async fn wait_exit(&self) -> ExitOutcome {
        let mut exit = self.inner.exit.subscribe();
        loop {
            if let Some(outcome) = *exit.borrow_and_update() {
                return outcome;
            }
            if exit.changed().await.is_err() {
                return ExitOutcome {
                    code: None,
                    signal: None,
                };
            }
        }
    }

    /// The exit outcome once the leader has exited.
    pub fn exit_outcome(&self) -> Option<ExitOutcome> {
        *self.inner.exit.borrow()
    }

    /// True after an explicit stop was requested for this run.
    pub fn stop_was_requested(&self) -> bool {
        self.inner
            .stop_requested
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// True when both handles refer to the same run.
    pub fn same_run(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// True while the process (leader) has not exited.
    pub fn is_alive(&self) -> bool {
        self.inner.exit.borrow().is_none()
    }

    /// Explicit stop only; nothing in the daemon calls this implicitly (decision 16).
    pub async fn stop(&self, policy: StopPolicy) -> Result<(), RpcError> {
        if !self.is_alive() {
            return Ok(());
        }
        self.inner
            .stop_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.set_state(UnitState::Stopping);
        self.inner
            .unit
            .stop(policy)
            .await
            .map_err(|error| internal("stop_failed", error))?;
        self.wait_exit().await;
        Ok(())
    }

    /// Terminal input from an attached client or the TUI.
    pub async fn input(&self, bytes: Bytes) -> Result<(), RpcError> {
        self.inner
            .unit
            .input(bytes)
            .await
            .map_err(|error| internal("input_failed", error))
    }

    /// Resizes both the PTY and the screen model so snapshots match what the app drew.
    pub async fn resize(&self, size: TerminalSize) -> Result<(), RpcError> {
        if let Ok(mut screen) = self.inner.screen.lock() {
            screen
                .screen_mut()
                .set_size(cells(size.rows), cells(size.columns));
        }
        self.inner
            .unit
            .resize(size)
            .await
            .map_err(|error| internal("resize_failed", error))
    }

    /// Returns a full-screen redraw plus a receiver for everything after it.
    pub fn attach(&self) -> (Bytes, broadcast::Receiver<Bytes>) {
        match self.inner.screen.lock() {
            Ok(screen) => {
                let snapshot = snapshot_bytes(&screen);
                (snapshot, self.inner.live.subscribe())
            }
            // A poisoned lock only loses the snapshot; live output still flows.
            Err(_) => (Bytes::new(), self.inner.live.subscribe()),
        }
    }

    /// Fresh redraw for a viewer that lagged behind the live channel.
    pub fn redraw(&self) -> Bytes {
        self.inner
            .screen
            .lock()
            .map(|screen| snapshot_bytes(&screen))
            .unwrap_or_default()
    }

    /// Process tree members for metrics and diagnostics (and v2 window matching).
    pub async fn subjects(&self) -> Vec<ProcessIdentity> {
        self.inner.unit.subjects().await.unwrap_or_default()
    }

    /// Queries this unit's logs off the executor.
    pub async fn logs(&self, query: LogQuery) -> Result<Page<LogLine>, RpcError> {
        let (synced, applied) = tokio::sync::oneshot::channel();
        // A finished log thread has nothing queued; its send failure is fine.
        if self.inner.log_tx.send(LogCommand::Sync(synced)).is_ok() {
            let _ = tokio::time::timeout(PUMP_DRAIN, applied).await;
        }
        let log = self.inner.log.clone();
        tokio::task::spawn_blocking(move || {
            let mut log = log.lock().map_err(|_| poisoned())?;
            // Flush so the query sees lines written moments ago.
            log.flush()
                .map_err(|error| internal("log_flush_failed", error))?;
            log.query(&query)
                .map_err(|error| internal("log_query_failed", error))
        })
        .await
        .map_err(|error| internal("log_task_failed", error))?
    }

    /// The last lines, ANSI stripped, for failure reports.
    pub async fn tail(&self, lines: usize) -> Vec<String> {
        let query = LogQuery {
            tail: Some(lines),
            max_bytes: Some(32 * 1024),
            ..LogQuery::default()
        };
        self.logs(query)
            .await
            .map(|page| page.items.into_iter().map(|line| line.text).collect())
            .unwrap_or_default()
    }

    /// Subscribes to raw output (for `log` ready checks and followers).
    pub fn subscribe_output(&self) -> broadcast::Receiver<Bytes> {
        self.inner.live.subscribe()
    }

    /// Feeds the screen, re-broadcasts under the screen lock, then hands bytes to the log thread.
    fn spawn_pump(&self) {
        let inner = self.inner.clone();
        let mut output = inner.unit.output();
        tokio::spawn(async move {
            loop {
                match output.recv().await {
                    Ok(chunk) => {
                        if let Ok(mut screen) = inner.screen.lock() {
                            screen.process(&chunk.bytes);
                            let _ = inner.live.send(chunk.bytes.clone());
                        }
                        let _ = inner.log_tx.send(LogCommand::Output(chunk.bytes, now_ms()));
                    }
                    // Lost chunks are still in the runner's own buffers only if it keeps them;
                    // the screen may be stale until the next redraw, logs miss those bytes.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(unit = %inner.info.name, skipped, "output pump lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            let _ = inner.pump_done.send(true);
        });
    }

    /// Records the exit, classifies it and finishes the log.
    fn spawn_waiter(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = inner.unit.wait().await.unwrap_or(ExitOutcome {
                code: None,
                signal: None,
            });
            // Waiters read logs right after the exit; let the last output land first.
            let mut drained = inner.pump_done.subscribe();
            let _ = tokio::time::timeout(PUMP_DRAIN, drained.wait_for(|done| *done)).await;
            let requested = inner
                .stop_requested
                .load(std::sync::atomic::Ordering::SeqCst);
            let next = if requested || outcome.code == Some(0) {
                UnitState::Exited(outcome)
            } else {
                UnitState::Failed(Some(RpcError::new(
                    "exited",
                    json!({"code": outcome.code, "signal": outcome.signal}),
                    1,
                )))
            };
            inner.state.send_if_modified(|state| {
                // A readiness failure already explains the run; keep it.
                if matches!(state, UnitState::Failed(Some(_))) && !requested {
                    return false;
                }
                *state = next;
                true
            });
            let _ = inner.exit.send(Some(outcome));
            let _ = inner.log_tx.send(LogCommand::Finish);
        });
    }
}

/// Queries the logs of a unit that has no live host (earlier runs, other daemons).
/// Opening recovers and closes segments, so call it only while nothing writes there.
pub async fn query_log_dir(
    dir: PathBuf,
    unit: String,
    query: LogQuery,
) -> Result<Page<LogLine>, RpcError> {
    tokio::task::spawn_blocking(move || {
        if !dir.exists() {
            return Ok(Page::complete(Vec::new()));
        }
        let mut log = UnitLog::open(&dir, unit.replace('#', "_"), LogOptions::default())
            .map_err(|error| internal("log_open_failed", error))?;
        let page = log
            .query(&query)
            .map_err(|error| internal("log_query_failed", error));
        let _ = log.finish(now_ms());
        page
    })
    .await
    .map_err(|error| internal("log_task_failed", error))?
}

/// One thread per unit keeps log I/O ordered and off the async runtime.
fn spawn_log_thread(log: Arc<StdMutex<UnitLog>>, commands: std::sync::mpsc::Receiver<LogCommand>) {
    std::thread::Builder::new()
        .name("devlanes-log".into())
        .spawn(move || {
            loop {
                // The timeout drives the 200 ms partial-line flush even when output stops.
                let command = commands.recv_timeout(Duration::from_millis(200));
                let Ok(mut log) = log.lock() else { return };
                let result = match command {
                    Ok(LogCommand::Output(bytes, ts)) => log.ingest(&bytes, ts),
                    Ok(LogCommand::Sync(done)) => {
                        let _ = done.send(());
                        Ok(())
                    }
                    Ok(LogCommand::Finish)
                    | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        if let Err(error) = log.finish(now_ms()) {
                            tracing::warn!(%error, "log finish failed");
                        }
                        return;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let now = now_ms();
                        log.flush_partial(now, false)
                            .and_then(|()| log.maintain(now))
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "log write failed");
                }
            }
        })
        .map(drop)
        .unwrap_or_else(|error| tracing::error!(%error, "log thread spawn failed"));
}

/// Clear + home + current contents, so the viewer's terminal matches the model exactly.
fn snapshot_bytes(parser: &vt100::Parser) -> Bytes {
    let screen = parser.screen();
    let mut bytes = b"\x1b[H\x1b[2J".to_vec();
    bytes.extend_from_slice(screen.contents_formatted().as_bytes());
    bytes.extend_from_slice(screen.input_mode_formatted().as_bytes());
    Bytes::from(bytes)
}

/// vt100 rejects zero-sized screens; a client reporting 0 rows still gets one.
fn cells(value: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(value).unwrap_or(std::num::NonZeroU16::MIN)
}

/// Milliseconds since the epoch for log and summary timestamps.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Internal failures carry their message as data; the CLI words them.
pub fn internal(code: &str, error: impl std::fmt::Display) -> RpcError {
    RpcError::new(code, json!({"message": error.to_string()}), 1)
}

/// A panicked log writer poisons the lock; reads report it instead of panicking too.
fn poisoned() -> RpcError {
    RpcError::new("log_unavailable", serde_json::Value::Null, 1)
}
