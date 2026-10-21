//! One host per running unit: it owns the output pump, the screen model used for
//! attach snapshots, the log writer thread and the lifecycle state other parts watch.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use devlanes_core::{ExitOutcome, ProcessIdentity, StopPolicy, TerminalSize, Unit};
use devlanes_logs::{LogOptions, LogReader, UnitLog};
use devlanes_proto::{Actor, LogLine, LogQuery, Page, RpcError};
use serde_json::json;
use tokio::sync::{broadcast, watch};

/// Daemon-side scrollback stays small; deep history comes from log segments.
const SCROLLBACK: usize = 200;
/// Most output chunks fed to the screen model per blocking-pool trip.
const PUMP_BATCH: usize = 64;
/// Output chunks queued for the log thread; beyond this, chunks are dropped from
/// the log (and counted) rather than buffered without bound.
const LOG_QUEUE: usize = 1024;
/// Partial-line and file-buffer flush cadence, matching `devlanes_logs::PARTIAL_FLUSH_MS`.
const LOG_TICK: Duration = Duration::from_millis(devlanes_logs::PARTIAL_FLUSH_MS);

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
    pub retention: LogRetention,
}

/// Disk bounds from the user's `logs.retention`, applied to every unit's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRetention {
    /// Upper bound for one unit's segments; the maintenance pass also enforces it
    /// across all units.
    pub max_bytes: u64,
    pub max_age_ms: u64,
}

impl Default for LogRetention {
    /// The user config defaults: 7 days, 5 GiB.
    fn default() -> Self {
        Self::from_config(&devlanes_config::LogRetention::default())
    }
}

impl LogRetention {
    /// Converts `logs.retention`.
    pub fn from_config(config: &devlanes_config::LogRetention) -> Self {
        Self {
            max_bytes: config.max_size.0,
            max_age_ms: u64::from(config.days).saturating_mul(86_400_000),
        }
    }

    /// Log options for one unit with these bounds.
    fn options(self) -> LogOptions {
        LogOptions {
            retention_bytes: self.max_bytes,
            retention_age_ms: self.max_age_ms,
            ..LogOptions::default()
        }
    }
}

/// Commands for the log thread; blocking file I/O never runs on the async executor.
enum LogCommand {
    Output(Bytes, u64),
    /// Answered once every earlier command was applied, so queries see all output.
    Sync(tokio::sync::oneshot::Sender<()>),
    Finish,
}

/// State the async side shares with the log thread outside the bounded queue.
#[derive(Default)]
struct LogSignals {
    /// Output chunks dropped because the queue was full, since the run started.
    dropped: AtomicU64,
    /// Set by the exit; covers a `Finish` that found the queue full.
    finish: AtomicBool,
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
    log_tx: std::sync::mpsc::SyncSender<LogCommand>,
    log_signals: Arc<LogSignals>,
    /// Set when someone asked for a stop, so a signal exit reads as `stopped`, not `failed`.
    stop_requested: std::sync::atomic::AtomicBool,
    /// Flips once the pump saw the end of output, so the exit is published after it.
    pump_done: watch::Sender<bool>,
    /// First log sequence of this run; earlier segments in the directory are older runs.
    first_seq: u64,
    /// Flips once the log thread closed the log, so segment metadata is final.
    log_done: watch::Sender<bool>,
    /// Claimed by whoever publishes the exit first: the waiter, or a stop whose bound
    /// expired after the process was already gone.
    exit_claimed: std::sync::atomic::AtomicBool,
}

/// How long the exit waits for remaining output to reach the log after the leader died.
const PUMP_DRAIN: Duration = Duration::from_secs(1);
/// Time a stop may take beyond its policy timeout: the runner's SIGKILL sweep and
/// output drain, plus [`PUMP_DRAIN`], with room for a loaded machine.
const STOP_MARGIN: Duration = Duration::from_secs(10);
/// How long a stop waits for the run's log to close after the exit.
const LOG_CLOSE: Duration = Duration::from_secs(5);

impl UnitHost {
    /// Opens the log off the executor, then starts pumping output so a chatty process
    /// never blocks on a full PTY.
    pub async fn start(
        info: UnitInfo,
        unit: Arc<dyn Unit>,
        identity: Option<ProcessIdentity>,
    ) -> Result<Self, RpcError> {
        let log = open_log(
            info.log_dir.clone(),
            info.name.replace('#', "_"),
            info.retention.options(),
        )
        .await?;
        let first_seq = log.next_sequence();
        let log = Arc::new(StdMutex::new(log));
        let (log_tx, log_rx) = std::sync::mpsc::sync_channel(LOG_QUEUE);
        let log_signals = Arc::new(LogSignals::default());
        let log_done = watch::channel(false).0;
        spawn_log_thread(
            log.clone(),
            log_rx,
            log_signals.clone(),
            info.name.clone(),
            log_done.clone(),
        );
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
                log_signals,
                stop_requested: std::sync::atomic::AtomicBool::new(false),
                pump_done: watch::channel(false).0,
                first_seq,
                log_done,
                exit_claimed: std::sync::atomic::AtomicBool::new(false),
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

    /// After an explicit full stop, a run that had failed and exited reads as
    /// stopped; the failure stays in `unit_runs`. Returns whether it changed.
    pub fn acknowledge_failure(&self) -> bool {
        let Some(outcome) = self.exit_outcome() else {
            return false;
        };
        self.inner.state.send_if_modified(|state| {
            if !matches!(state, UnitState::Failed(_)) {
                return false;
            }
            *state = UnitState::Exited(outcome);
            true
        })
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
    ///
    /// Bounded: after the policy timeout plus a margin, a process that is gone gets an
    /// unknown exit published, and one that still runs is reported as `stop_timeout`.
    pub async fn stop(&self, policy: StopPolicy) -> Result<(), RpcError> {
        self.stop_within(policy, STOP_MARGIN).await
    }

    /// [`UnitHost::stop`] with an explicit margin, so tests need not wait the default.
    async fn stop_within(&self, policy: StopPolicy, margin: Duration) -> Result<(), RpcError> {
        if !self.is_alive() {
            self.wait_log_closed().await;
            return Ok(());
        }
        self.inner
            .stop_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.set_state(UnitState::Stopping);
        let stopped = tokio::time::timeout(policy.timeout.saturating_add(margin), async {
            self.inner
                .unit
                .stop(policy)
                .await
                .map_err(|error| internal("stop_failed", error))?;
            self.wait_exit().await;
            Ok(())
        })
        .await;
        let result = match stopped {
            Ok(result) => result,
            Err(_) => self.settle_overdue_stop().await,
        };
        if result.is_ok() {
            self.wait_log_closed().await;
        }
        result
    }

    /// Waits briefly for the log thread to close this run's segments. A restart opens
    /// the same directory right after `stop`, and opening while the old run still
    /// compresses its last segment fails with ENOENT.
    async fn wait_log_closed(&self) {
        let mut done = self.inner.log_done.subscribe();
        let _ = tokio::time::timeout(LOG_CLOSE, done.wait_for(|done| *done)).await;
    }

    /// Runs when a stop outlived its bound. If the leader is gone, its exit got lost
    /// between the runner and this host, so an unknown outcome is published to release
    /// every waiter. A leader that is still alive is reported instead of waited on.
    async fn settle_overdue_stop(&self) -> Result<(), RpcError> {
        let alive = match self.inner.identity {
            Some(identity) => tokio::task::spawn_blocking(move || devlanes_sys::matches(&identity))
                .await
                .ok()
                .and_then(Result::ok)
                // An unreadable process table must not declare a live process dead.
                .unwrap_or(true),
            // Without an identity nothing can be checked; releasing waiters is the
            // only way the stop can finish.
            None => false,
        };
        if alive {
            return Err(RpcError::new(
                "stop_timeout",
                json!({"unit": self.inner.info.name}),
                1,
            ));
        }
        tracing::warn!(unit = %self.inner.info.name, "exit not reported after stop; process is gone");
        self.inner.publish_exit(ExitOutcome {
            code: None,
            signal: None,
        });
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
        let log_tx = self.inner.log_tx.clone();
        // The queue is bounded, so a full one is waited on off the executor. A finished
        // log thread has nothing queued; its send failure is fine.
        let queued =
            tokio::task::spawn_blocking(move || log_tx.send(LogCommand::Sync(synced)).is_ok());
        let _ = tokio::time::timeout(PUMP_DRAIN, async {
            if matches!(queued.await, Ok(true)) {
                let _ = applied.await;
            }
        })
        .await;
        let log = self.inner.log.clone();
        tokio::task::spawn_blocking(move || {
            // The snapshot flushes under the lock; reading happens after releasing it,
            // so a long query never stalls the writer.
            let reader = log.lock().map_err(|_| poisoned())?.reader();
            reader
                .query(&query)
                .map_err(|error| internal("log_query_failed", error))
        })
        .await
        .map_err(|error| internal("log_task_failed", error))?
    }

    /// Segments written by this run, once its log is closed (waits up to `limit`
    /// after the exit). Earlier runs in the same directory are left out.
    pub async fn run_segments(&self, limit: Duration) -> Vec<devlanes_logs::SegmentMetadata> {
        let mut done = self.inner.log_done.subscribe();
        let _ = tokio::time::timeout(limit, done.wait_for(|done| *done)).await;
        let log = self.inner.log.clone();
        let first = self.inner.first_seq;
        tokio::task::spawn_blocking(move || {
            log.lock()
                .map(|log| {
                    log.segments()
                        .into_iter()
                        .filter(|segment| segment.first_seq >= first && segment.records > 0)
                        .collect()
                })
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
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
        use broadcast::error::{RecvError, TryRecvError};
        let inner = self.inner.clone();
        let mut output = inner.unit.output();
        tokio::spawn(async move {
            let mut closed = false;
            // Set after lost chunks: attached viewers then get a full redraw instead
            // of raw bytes that assume a screen they never received.
            let mut stale = false;
            while !closed {
                let mut batch = Vec::new();
                match output.recv().await {
                    Ok(chunk) => batch.push(chunk.bytes),
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(unit = %inner.info.name, skipped, "output pump lagged");
                        stale = true;
                    }
                    Err(RecvError::Closed) => closed = true,
                }
                // Whatever is already queued joins this batch, so a burst costs one
                // trip to the blocking pool instead of one per chunk.
                while !closed && batch.len() < PUMP_BATCH {
                    match output.try_recv() {
                        Ok(chunk) => batch.push(chunk.bytes),
                        Err(TryRecvError::Lagged(skipped)) => {
                            tracing::warn!(unit = %inner.info.name, skipped, "output pump lagged");
                            stale = true;
                        }
                        Err(TryRecvError::Closed) => closed = true,
                        Err(TryRecvError::Empty) => break,
                    }
                }
                if batch.is_empty() {
                    continue;
                }
                // vt100 parsing is CPU work proportional to output; keep it off the
                // executor. Awaiting each batch keeps screen and live order intact.
                let screen = inner.clone();
                let fed = batch.clone();
                let redraw = std::mem::take(&mut stale);
                let _ = tokio::task::spawn_blocking(move || screen.feed_screen(&fed, redraw)).await;
                for bytes in batch {
                    // Never wait on disk here: a stalled pump would block the PTY.
                    if let Err(std::sync::mpsc::TrySendError::Full(_)) =
                        inner.log_tx.try_send(LogCommand::Output(bytes, now_ms()))
                    {
                        inner.log_signals.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            // send_replace: the waiter may subscribe only after this flips.
            inner.pump_done.send_replace(true);
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
            inner.publish_exit(outcome);
        });
    }
}

impl Inner {
    /// Feeds output to the screen model and re-broadcasts it under the screen lock,
    /// so an attacher's snapshot and subscription never leave a gap. With `redraw`,
    /// viewers get one full redraw of the model instead of the raw bytes, because
    /// they missed the chunks lost before this batch. Blocking.
    fn feed_screen(&self, batch: &[Bytes], redraw: bool) {
        let Ok(mut screen) = self.screen.lock() else {
            return;
        };
        for bytes in batch {
            screen.process(bytes);
            if !redraw {
                let _ = self.live.send(bytes.clone());
            }
        }
        if redraw {
            let _ = self.live.send(snapshot_bytes(&screen));
        }
    }

    /// Classifies the run, then publishes the exit and closes the log, once per run.
    ///
    /// The exit uses `send_replace`: plain `send` discards the value while nobody is
    /// subscribed, and every later `wait_exit` (from `stop`, the lane monitor or
    /// attach) would then wait forever on a run that already ended.
    fn publish_exit(&self, outcome: ExitOutcome) {
        if self
            .exit_claimed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let requested = self
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
        self.state.send_if_modified(|state| {
            // A readiness failure already explains the run; keep it.
            if matches!(state, UnitState::Failed(Some(_))) && !requested {
                return false;
            }
            *state = next;
            true
        });
        self.exit.send_replace(Some(outcome));
        // The flag finishes the log once the queue drains if this send finds it full.
        self.log_signals.finish.store(true, Ordering::SeqCst);
        let _ = self.log_tx.try_send(LogCommand::Finish);
    }
}

/// Queries the logs of a unit that has no live host (earlier runs, other daemons).
/// Read-only: it recovers nothing and creates no files, so a unit starting in the
/// same directory is unaffected. `_retention` is unused since reads never prune.
pub async fn query_log_dir(
    dir: PathBuf,
    unit: String,
    query: LogQuery,
    _retention: LogRetention,
) -> Result<Page<LogLine>, RpcError> {
    tokio::task::spawn_blocking(move || {
        LogReader::open(&dir, unit.replace('#', "_"))
            .map_err(|error| internal("log_open_failed", error))?
            .query(&query)
            .map_err(|error| internal("log_query_failed", error))
    })
    .await
    .map_err(|error| internal("log_task_failed", error))?
}

/// Opens a run's log on a blocking thread. Right after a restart the previous run may
/// still be closing the directory and hold its writer lock, so that is retried briefly.
async fn open_log(dir: PathBuf, unit: String, options: LogOptions) -> Result<UnitLog, RpcError> {
    tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + LOG_CLOSE;
        loop {
            match UnitLog::open(&dir, unit.clone(), options.clone()) {
                Err(devlanes_logs::Error::Locked) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
    })
    .await
    .map_err(|error| internal("log_task_failed", error))?
    .map_err(|error| internal("log_open_failed", error))
}

/// One thread per unit keeps log I/O ordered and off the async runtime.
fn spawn_log_thread(
    log: Arc<StdMutex<UnitLog>>,
    commands: std::sync::mpsc::Receiver<LogCommand>,
    signals: Arc<LogSignals>,
    unit: String,
    done: watch::Sender<bool>,
) {
    use std::sync::mpsc::RecvTimeoutError;
    std::thread::Builder::new()
        .name("devlanes-log".into())
        .spawn(move || {
            // Dropped on every return path, so waiters never hang on a dead thread.
            let _done = DoneGuard(done);
            let mut last_tick = Instant::now();
            let mut reported_drops = 0;
            let mut warned = false;
            loop {
                // A deadline rather than an idle timeout, so partial lines and the file
                // buffer also flush while output keeps arriving.
                let command = commands.recv_timeout(LOG_TICK.saturating_sub(last_tick.elapsed()));
                let Ok(mut log) = log.lock() else { return };
                let finishing = matches!(
                    command,
                    Ok(LogCommand::Finish) | Err(RecvTimeoutError::Disconnected)
                ) || (matches!(command, Err(RecvTimeoutError::Timeout))
                    && signals.finish.load(Ordering::SeqCst));
                let dropped = signals.dropped.load(Ordering::Relaxed);
                if dropped > reported_drops && (finishing || last_tick.elapsed() >= LOG_TICK) {
                    tracing::warn!(unit, chunks = dropped - reported_drops, total = dropped, "log writer fell behind; output chunks were not logged");
                    reported_drops = dropped;
                }
                if finishing {
                    if let Err(error) = log.finish(now_ms()) {
                        tracing::warn!(unit, %error, "log finish failed");
                    }
                    return;
                }
                let mut result = match command {
                    Ok(LogCommand::Output(bytes, ts)) => log.ingest(&bytes, ts),
                    Ok(LogCommand::Sync(done)) => {
                        let _ = done.send(());
                        Ok(())
                    }
                    _ => Ok(()),
                };
                if last_tick.elapsed() >= LOG_TICK {
                    last_tick = Instant::now();
                    result = result.and_then(|()| log.flush_partial(now_ms(), false));
                }
                // A failed writer refuses everything after; one warning per run is enough.
                if let Err(error) = result
                    && !warned
                {
                    warned = true;
                    tracing::warn!(unit, %error, "log write failed; later output of this run is not logged");
                }
            }
        })
        .map(drop)
        .unwrap_or_else(|error| tracing::error!(%error, "log thread spawn failed"));
}

/// Signals the end of the log thread when dropped.
struct DoneGuard(watch::Sender<bool>);

impl Drop for DoneGuard {
    /// Marks the log closed.
    fn drop(&mut self) {
        // send_replace keeps the flag for `run_segments` callers that subscribe later.
        self.0.send_replace(true);
    }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use devlanes_core::{Error, OutputChunk, OutputStream};

    use super::*;

    /// Bound for each awaited step; a lost exit shows up as a timeout, not a hang.
    const STEP: Duration = Duration::from_secs(5);

    /// A unit that exits on its own after `natural`, or when stopped, whichever is first.
    /// `hang` makes `wait` never return, like a runner whose exit report got lost.
    struct ScriptedUnit {
        identity: ProcessIdentity,
        exit: watch::Sender<Option<ExitOutcome>>,
        output: StdMutex<Option<broadcast::Sender<OutputChunk>>>,
        first: StdMutex<Option<broadcast::Receiver<OutputChunk>>>,
        hang: AtomicBool,
    }

    impl ScriptedUnit {
        /// Starts the unit; `natural: None` runs until stopped.
        fn start(natural: Option<Duration>, identity: ProcessIdentity) -> Arc<Self> {
            let (sender, first) = broadcast::channel(4);
            let unit = Arc::new(Self {
                identity,
                exit: watch::channel(None).0,
                output: StdMutex::new(Some(sender)),
                first: StdMutex::new(Some(first)),
                hang: AtomicBool::new(false),
            });
            if let Some(delay) = natural {
                let ending = unit.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    ending.end(Some(0), None);
                });
            }
            unit
        }

        /// Closes output first, as a real PTY does, then reports the exit.
        fn end(&self, code: Option<i32>, signal: Option<i32>) {
            if let Ok(mut output) = self.output.lock() {
                output.take();
            }
            self.exit.send_if_modified(|exit| {
                if exit.is_some() {
                    return false;
                }
                *exit = Some(ExitOutcome { code, signal });
                true
            });
        }
    }

    #[async_trait]
    impl Unit for ScriptedUnit {
        async fn input(&self, _: Bytes) -> Result<(), Error> {
            Ok(())
        }
        async fn resize(&self, _: TerminalSize) -> Result<(), Error> {
            Ok(())
        }
        async fn stop(&self, policy: StopPolicy) -> Result<(), Error> {
            self.end(None, Some(policy.signal));
            Ok(())
        }
        async fn wait(&self) -> Result<ExitOutcome, Error> {
            if self.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            let mut exit = self.exit.subscribe();
            let outcome = exit
                .wait_for(Option::is_some)
                .await
                .map_err(|_| Error::Exited)?;
            outcome.ok_or(Error::Exited)
        }
        fn output(&self) -> broadcast::Receiver<OutputChunk> {
            if let Some(first) = self.first.lock().ok().and_then(|mut f| f.take()) {
                return first;
            }
            broadcast::channel(1).1
        }
        async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error> {
            Ok(vec![self.identity])
        }
    }

    /// A host for `unit` logging into its own directory under `dir`.
    async fn host(unit: Arc<ScriptedUnit>, dir: &tempfile::TempDir) -> UnitHost {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run = NEXT.fetch_add(1, Ordering::Relaxed);
        let info = UnitInfo {
            name: "web".into(),
            lane: "0".into(),
            kind: "service",
            actor: Actor::default(),
            started_at_ms: 0,
            log_dir: dir.path().join(run.to_string()),
            retention: LogRetention::default(),
        };
        let identity = Some(unit.identity);
        UnitHost::start(info, unit, identity)
            .await
            .expect("host starts")
    }

    /// An identity whose process is gone: a child that was spawned and reaped.
    fn gone_identity() -> ProcessIdentity {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let identity = devlanes_sys::identity(pid)
            .ok()
            .flatten()
            .expect("unreaped child is visible");
        child.wait().expect("reap");
        identity
    }

    /// Regression: the exit used to be published with `watch::send`, which drops the
    /// value when nobody is subscribed. A unit that ended before any `wait_exit` call
    /// then looked alive forever and `stop` never returned.
    #[tokio::test]
    async fn exit_published_without_subscribers_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = ScriptedUnit::start(Some(Duration::ZERO), gone_identity());
        let host = host(unit, &dir).await;
        let mut state = host.watch_state();
        tokio::time::timeout(STEP, state.wait_for(UnitState::is_terminal))
            .await
            .expect("state turns terminal")
            .expect("state sender alive");
        // Nobody watched the exit when it was published; it must still be readable.
        let outcome = tokio::time::timeout(STEP, host.wait_exit())
            .await
            .expect("exit is not lost");
        assert_eq!(outcome.code, Some(0));
        assert!(!host.is_alive());
        tokio::time::timeout(STEP, host.stop(StopPolicy::default()))
            .await
            .expect("stop of an exited unit returns")
            .expect("stop succeeds");
    }

    /// The log tick flushes partial lines and the file buffer, so a read-only query of
    /// a live unit's directory sees its output without taking the writer's lock, and a
    /// query of a missing directory creates nothing.
    #[tokio::test]
    async fn live_output_reaches_read_only_directory_queries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing");
        let query = |dir: PathBuf| {
            query_log_dir(
                dir,
                "web".into(),
                LogQuery::default(),
                LogRetention::default(),
            )
        };
        let page = query(missing.clone()).await.expect("query");
        assert!(page.items.is_empty());
        assert!(!missing.exists());

        let unit = ScriptedUnit::start(None, gone_identity_cached());
        let output = unit
            .output
            .lock()
            .expect("output lock")
            .clone()
            .expect("output open");
        let host = host(unit, &dir).await;
        output
            .send(OutputChunk {
                stream: OutputStream::Terminal,
                bytes: Bytes::from_static(b"hello\nprompt> "),
            })
            .expect("send");
        let deadline = Instant::now() + STEP;
        loop {
            let page = query(host.info().log_dir.clone())
                .await
                .expect("read-only query");
            let lines: Vec<_> = page.items.into_iter().map(|line| line.text).collect();
            if lines == ["hello", "prompt> "] {
                break;
            }
            assert!(Instant::now() < deadline, "{lines:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(host.tail(10).await, ["hello", "prompt> "]);
        tokio::time::timeout(STEP, host.stop(StopPolicy::default()))
            .await
            .expect("stop is bounded")
            .expect("stop succeeds");
    }

    /// Stops sent right as units exit on their own always finish with a known outcome.
    #[tokio::test]
    async fn stop_racing_natural_exit_always_finishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        for round in 0..200_u64 {
            let natural = Duration::from_micros((round % 20) * 50);
            let unit = ScriptedUnit::start(Some(natural), gone_identity_cached());
            let host = host(unit, &dir).await;
            tokio::time::sleep(Duration::from_micros((round % 7) * 70)).await;
            tokio::time::timeout(STEP, host.stop(StopPolicy::default()))
                .await
                .unwrap_or_else(|_| panic!("stop hung in round {round}"))
                .expect("stop succeeds");
            let outcome = host.exit_outcome().expect("exit known after stop");
            assert!(
                outcome.code.is_some() || outcome.signal.is_some(),
                "round {round}"
            );
            assert!(host.state().is_terminal(), "round {round}");
        }
    }

    /// Spawning a child per round is slow; the identity only needs to name nothing.
    fn gone_identity_cached() -> ProcessIdentity {
        static GONE: std::sync::OnceLock<ProcessIdentity> = std::sync::OnceLock::new();
        *GONE.get_or_init(gone_identity)
    }

    /// When the runner never reports an exit, stop still ends after its bound: a dead
    /// leader gets an unknown outcome, a live one is reported as `stop_timeout`.
    #[tokio::test]
    async fn overdue_stop_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = StopPolicy {
            signal: 15,
            timeout: Duration::ZERO,
        };
        let margin = Duration::from_millis(200);

        let unit = ScriptedUnit::start(None, gone_identity());
        unit.hang.store(true, Ordering::SeqCst);
        let gone = host(unit, &dir).await;
        tokio::time::timeout(STEP, gone.stop_within(policy, margin))
            .await
            .expect("bounded")
            .expect("gone process settles");
        assert_eq!(
            gone.exit_outcome(),
            Some(ExitOutcome {
                code: None,
                signal: None
            })
        );
        assert!(gone.state().is_terminal());
        tokio::time::timeout(STEP, gone.wait_exit())
            .await
            .expect("waiters released");

        let me = devlanes_sys::identity(std::process::id())
            .ok()
            .flatten()
            .expect("own identity");
        let unit = ScriptedUnit::start(None, me);
        unit.hang.store(true, Ordering::SeqCst);
        let alive = host(unit, &dir).await;
        let error = tokio::time::timeout(STEP, alive.stop_within(policy, margin))
            .await
            .expect("bounded")
            .expect_err("live process is reported");
        assert_eq!(error.code, "stop_timeout");
        assert!(alive.is_alive());
    }

    /// After the pump loses chunks, attached viewers get a full redraw of the screen
    /// model rather than raw bytes that assume output they never saw.
    #[tokio::test]
    async fn lagged_pump_sends_a_full_redraw() {
        let dir = tempfile::tempdir().expect("tempdir");
        let unit = ScriptedUnit::start(None, gone_identity_cached());
        let output = unit
            .output
            .lock()
            .expect("output lock")
            .clone()
            .expect("output open");
        let host = host(unit, &dir).await;
        let (_, mut live) = host.attach();
        // The scripted unit buffers 4 chunks; the pump has not run yet, so it lags.
        for line in 0..20 {
            let _ = output.send(OutputChunk {
                stream: OutputStream::Terminal,
                bytes: Bytes::from(format!("line{line}\r\n")),
            });
        }
        let redraw = tokio::time::timeout(STEP, live.recv())
            .await
            .expect("viewer gets output")
            .expect("live open");
        assert!(redraw.starts_with(b"\x1b[H\x1b[2J"), "{redraw:?}");
        assert!(String::from_utf8_lossy(&redraw).contains("line19"));
    }
}
