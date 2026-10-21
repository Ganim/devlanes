//! Unit handle and the per-unit supervisor task that reaps, tracks and stops it.

use std::{
    collections::BTreeMap,
    fmt, io,
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    Error, ExitOutcome, LaunchSpec, OutputChunk, ProcessIdentity, TerminalSize, Unit,
};
use devlanes_sys::{DescendantTracker, ListeningSocket, ProcessInfo, Signal, SignalOutcome};
use rustix::{fs::OFlags, io::Errno, process::WaitOptions};
use tokio::{
    io::unix::AsyncFd,
    signal::unix::{SignalKind, signal},
    sync::{broadcast, mpsc, watch},
    task,
    time::{self, Instant, MissedTickBehavior},
};

use crate::{PtyRunner, output, snapshot, spawn};

/// Poll period while waiting for session members to exit after the leader.
const EXIT_POLL: Duration = Duration::from_millis(50);
/// Poll period between SIGKILL rounds of the final sweep.
const SWEEP_POLL: Duration = Duration::from_millis(25);

/// Everything known about a finished unit. Shared by all waiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    /// Leader status; both fields are `None` if another reaper stole the status.
    pub outcome: ExitOutcome,
    /// Present when an explicit stop ended the unit.
    pub stop: Option<StopOutcome>,
    /// Leftover processes killed by the final sweep after the leader exited.
    pub swept: Vec<ProcessInfo>,
    /// Tracked processes still alive after the sweep's SIGKILL rounds.
    pub escaped: Vec<ProcessInfo>,
}

/// How an explicit stop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    /// The first requested signal number.
    pub signal: i32,
    /// SIGKILL was needed because the timeout elapsed first.
    pub escalated: bool,
}

/// Concrete unit handle. Clones of the `Arc` share one supervisor; dropping every
/// handle leaves the process running until it exits on its own.
pub struct PtyUnit {
    inner: Arc<Inner>,
}

/// State shared between handles and the supervisor task.
struct Inner {
    identity: ProcessIdentity,
    master: Option<Arc<AsyncFd<OwnedFd>>>,
    /// Keeps concurrent inputs from interleaving partial writes.
    input_lock: tokio::sync::Mutex<()>,
    output: Mutex<OutputState>,
    tracker: Mutex<DescendantTracker>,
    /// Set by the supervisor once waitpid returned; the leader's PID may then be recycled.
    leader_reaped: AtomicBool,
    stops: mpsc::UnboundedSender<StopRequest>,
    exit: watch::Sender<Option<ExitReport>>,
    timing: Timing,
}

/// Subscription source plus the receiver created before the first byte was read.
struct OutputState {
    /// Dropped at exit so receivers observe `Closed` once readers also finish.
    sender: Option<broadcast::Sender<OutputChunk>>,
    first: Option<broadcast::Receiver<OutputChunk>>,
}

/// Supervisor tuning copied from the runner.
#[derive(Debug, Clone, Copy)]
struct Timing {
    track_interval: Duration,
    drain_timeout: Duration,
    sweep_timeout: Duration,
}

/// A validated stop request sent to the supervisor.
#[derive(Debug, Clone, Copy)]
struct StopRequest {
    signal: Signal,
    timeout: Duration,
}

/// Stop progress owned by the supervisor.
#[derive(Debug, Clone, Copy)]
struct Stopping {
    signal: Signal,
    deadline: Instant,
    escalated: bool,
}

/// Launches the leader, wires output readers and starts its supervisor.
pub(crate) async fn spawn(runner: &PtyRunner, spec: LaunchSpec) -> Result<Arc<PtyUnit>, Error> {
    // Listening before fork guarantees the leader's SIGCHLD cannot be missed.
    let sigchld = signal(SignalKind::child())?;
    let nofile = devlanes_sys::child_nofile_limit();
    let stderr = runner.stderr;
    let launched = blocking(move || spawn::launch(spec, stderr, nofile)).await??;
    let identity = launched.identity;

    let (sender, first) = broadcast::channel(runner.output_capacity);
    let (done_tx, done_rx) = watch::channel(false);
    let wired = launched
        .master
        .map(async_master)
        .transpose()
        .and_then(|master| {
            let master = master.map(Arc::new);
            output::start(master.clone(), launched.pipes, sender.clone(), done_tx)?;
            Ok(master)
        });
    let master = match wired {
        Ok(master) => master,
        Err(error) => {
            let _ = blocking(move || spawn::abandon(identity.pid)).await;
            return Err(error.into());
        }
    };

    let (stops, requests) = mpsc::unbounded_channel();
    let inner = Arc::new(Inner {
        identity,
        master,
        input_lock: tokio::sync::Mutex::new(()),
        output: Mutex::new(OutputState {
            sender: Some(sender),
            first: Some(first),
        }),
        tracker: Mutex::new(DescendantTracker::new(identity)),
        leader_reaped: AtomicBool::new(false),
        stops,
        exit: watch::channel(None).0,
        timing: Timing {
            track_interval: runner.track_interval,
            drain_timeout: runner.drain_timeout,
            sweep_timeout: runner.sweep_timeout,
        },
    });
    tokio::spawn(supervise(Arc::clone(&inner), sigchld, requests, done_rx));
    Ok(Arc::new(PtyUnit { inner }))
}

/// Registers the master with the reactor in non-blocking mode.
#[allow(unsafe_code)]
fn async_master(fd: OwnedFd) -> io::Result<AsyncFd<OwnedFd>> {
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?;
    // SAFETY: an OwnedFd is open and owned exclusively, and its `as_raw_fd` always
    // returns the same descriptor until the AsyncFd drops it.
    unsafe { AsyncFd::register(fd) }.map_err(|error| error.into_parts().1)
}

/// Owns the unit lifecycle: reaping, periodic tracking, stop escalation and the
/// post-exit drain and sweep. Stops run here so a cancelled `stop()` caller cannot
/// abandon an escalation halfway.
async fn supervise(
    inner: Arc<Inner>,
    mut sigchld: tokio::signal::unix::Signal,
    mut requests: mpsc::UnboundedReceiver<StopRequest>,
    mut output_done: watch::Receiver<bool>,
) {
    let _publish = PublishOnDrop(Arc::clone(&inner));
    let timing = inner.timing;
    let mut tick = time::interval(timing.track_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stopping: Option<Stopping> = None;

    let outcome = loop {
        // Checked every wakeup: SIGCHLD coalesces and may belong to other children.
        if let Some(outcome) = reap(inner.identity.pid) {
            break outcome;
        }
        let deadline = stopping.filter(|s| !s.escalated).map(|s| s.deadline);
        tokio::select! {
            _ = sigchld.recv() => {}
            _ = tick.tick() => {
                inner.refresh_logged(timing.track_interval / 2).await;
            }
            Some(request) = requests.recv() => {
                let deadline = Instant::now() + request.timeout;
                match &mut stopping {
                    // A repeated stop can only shorten the grace period.
                    Some(current) => current.deadline = current.deadline.min(deadline),
                    None => {
                        inner.signal_all(request.signal).await;
                        stopping = Some(Stopping { signal: request.signal, deadline, escalated: false });
                    }
                }
            }
            () = until(deadline) => {
                inner.signal_all(Signal::KILL).await;
                if let Some(current) = &mut stopping {
                    current.escalated = true;
                }
            }
        }
    };
    inner.leader_reaped.store(true, Ordering::Release);

    if let Some(current) = &mut stopping {
        // Members that outlive the leader get the rest of the grace period.
        while !current.escalated && !inner.live_members().await.is_empty() {
            if Instant::now() >= current.deadline {
                inner.signal_all(Signal::KILL).await;
                current.escalated = true;
            } else {
                time::sleep(EXIT_POLL).await;
            }
        }
    } else {
        // Leftover processes holding the slave or pipe would block EOF forever.
        let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    }
    let (swept, escaped) = inner.sweep().await;
    // Collect the last output of processes the sweep just killed.
    let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    inner.finish(ExitReport {
        outcome,
        stop: stopping.map(|s| StopOutcome {
            signal: s.signal.as_raw(),
            escalated: s.escalated,
        }),
        swept,
        escaped,
    });
}

/// Publishes a status-less report if the supervisor ends early (panic or runtime
/// shutdown), so waiters on other runtimes are not left hanging.
struct PublishOnDrop(Arc<Inner>);

impl Drop for PublishOnDrop {
    fn drop(&mut self) {
        if self.0.exit.borrow().is_none() {
            self.0.finish(ExitReport {
                outcome: ExitOutcome {
                    code: None,
                    signal: None,
                },
                stop: None,
                swept: Vec::new(),
                escaped: Vec::new(),
            });
        }
    }
}

/// Collects the leader's status with `waitpid(pid, WNOHANG)`; never waits for other PIDs.
fn reap(pid: u32) -> Option<ExitOutcome> {
    let unknown = ExitOutcome {
        code: None,
        signal: None,
    };
    let Some(raw) = raw_pid(pid) else {
        return Some(unknown);
    };
    loop {
        match rustix::process::waitpid(Some(raw), WaitOptions::NOHANG) {
            Ok(None) => return None,
            Ok(Some((_, status))) => {
                return Some(ExitOutcome {
                    code: status.exit_status(),
                    signal: status.terminating_signal(),
                });
            }
            Err(Errno::INTR) => {}
            Err(error) => {
                // ECHILD: someone called waitpid(-1) or reaped this PID; the status is gone.
                tracing::warn!(%error, pid, "unit leader status lost");
                return Some(unknown);
            }
        }
    }
}

/// Sleeps until `deadline`, or forever when there is none.
async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Runs blocking process-table or fork work off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Error> {
    task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Integration(format!("blocking task failed: {error}")))
}

/// Converts a kernel PID for rustix; 0 is never a unit.
fn raw_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}

/// Locks a std mutex, ignoring poisoning: the guarded data stays consistent per call.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Updates the tracker from a snapshot at most `max_age` old and returns all tracked
    /// processes. Blocking.
    fn refresh_blocking(&self, max_age: Duration) -> Result<Vec<ProcessInfo>, Error> {
        let snapshot = snapshot::processes(max_age)?;
        let mut tracker = lock(&self.tracker);
        tracker.update(&snapshot);
        Ok(tracker.processes().cloned().collect())
    }

    /// Fresh tracked set from async code.
    async fn refresh(self: &Arc<Self>, max_age: Duration) -> Result<Vec<ProcessInfo>, Error> {
        let inner = Arc::clone(self);
        blocking(move || inner.refresh_blocking(max_age)).await?
    }

    /// Refresh for supervisor paths, where a failed scan must not abort cleanup.
    async fn refresh_logged(self: &Arc<Self>, max_age: Duration) -> Vec<ProcessInfo> {
        self.refresh(max_age).await.unwrap_or_else(|error| {
            tracing::warn!(%error, pid = self.identity.pid, "process snapshot failed");
            lock(&self.tracker).processes().cloned().collect()
        })
    }

    /// Live tracked processes after a fresh scan.
    async fn live_members(self: &Arc<Self>) -> Vec<ProcessInfo> {
        let mut processes = self.refresh_logged(Duration::ZERO).await;
        processes.retain(|p| !p.zombie);
        processes
    }

    /// Signals the session's process group and every tracked process outside it.
    async fn signal_all(self: &Arc<Self>, signal: Signal) {
        let inner = Arc::clone(self);
        let _ = blocking(move || inner.signal_all_blocking(signal)).await;
    }

    /// Blocking body of [`Inner::signal_all`]. Only the supervisor calls it, so
    /// `leader_reaped` cannot change underneath.
    fn signal_all_blocking(&self, signal: Signal) {
        let processes = self
            .refresh_blocking(Duration::ZERO)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "process snapshot failed before signalling");
                lock(&self.tracker).processes().cloned().collect()
            });
        let leader = self.identity.pid;
        // Until reaped, the leader's PID and therefore its group ID cannot be recycled.
        let group_alive = !self.leader_reaped.load(Ordering::Acquire);
        if group_alive && let Some(pid) = raw_pid(leader) {
            let _ = rustix::process::kill_process_group(pid, signal);
        }
        for process in processes.iter().filter(|p| !p.zombie) {
            // Group members already got the signal; a second delivery trips the
            // "press again to force quit" handlers of many dev servers.
            if group_alive && process.pgid == leader {
                continue;
            }
            if let Err(error) = devlanes_sys::signal_verified(&process.identity(), signal) {
                tracing::debug!(%error, pid = process.pid, "signal failed");
            }
        }
    }

    /// SIGKILLs every live tracked process until none remain or the sweep times out.
    /// Returns the killed processes and the survivors.
    async fn sweep(self: &Arc<Self>) -> (Vec<ProcessInfo>, Vec<ProcessInfo>) {
        let deadline = Instant::now() + self.timing.sweep_timeout;
        let mut killed = BTreeMap::new();
        loop {
            let live = self.live_members().await;
            if live.is_empty() || Instant::now() >= deadline {
                for process in &live {
                    killed.remove(&process.pid);
                }
                return (killed.into_values().collect(), live);
            }
            let sent = blocking(move || {
                live.into_iter()
                    .filter(|p| {
                        matches!(
                            devlanes_sys::signal_verified(&p.identity(), Signal::KILL),
                            Ok(SignalOutcome::Sent)
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            for process in sent {
                killed.entry(process.pid).or_insert(process);
            }
            time::sleep(SWEEP_POLL).await;
        }
    }

    /// Publishes the report and closes the subscription source.
    fn finish(&self, report: ExitReport) {
        lock(&self.output).sender = None;
        self.exit.send_replace(Some(report));
    }
}

impl PtyUnit {
    /// Leader identity recorded right after spawn; persist it for reconciliation.
    pub fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    /// Reports whether the unit runs in a PTY rather than pipes.
    pub fn is_pty(&self) -> bool {
        self.inner.master.is_some()
    }

    /// Rescans and returns live tracked processes, the leader included while it runs.
    pub async fn processes(&self) -> Result<Vec<ProcessInfo>, Error> {
        let mut processes = self.inner.refresh(Duration::ZERO).await?;
        processes.retain(|p| !p.zombie);
        Ok(processes)
    }

    /// Rescans and returns live descendants, excluding the leader. Includes processes
    /// that left the session, as long as a snapshot saw them under a tracked parent.
    pub async fn descendants(&self) -> Result<Vec<ProcessInfo>, Error> {
        let leader = self.inner.identity.pid;
        let mut processes = self.processes().await?;
        processes.retain(|p| p.pid != leader);
        Ok(processes)
    }

    /// Lists TCP listeners of the tracked process tree, for ready-check diagnostics.
    pub async fn listening_ports(&self) -> Result<Vec<ListeningSocket>, Error> {
        let pids: Vec<u32> = self.processes().await?.iter().map(|p| p.pid).collect();
        Ok(blocking(move || devlanes_sys::listening_tcp(&pids)).await??)
    }

    /// Returns the current PTY size.
    pub fn size(&self) -> Result<TerminalSize, Error> {
        let size =
            rustix::termios::tcgetwinsize(self.master()?.get_ref()).map_err(io::Error::from)?;
        Ok(TerminalSize {
            columns: size.ws_col,
            rows: size.ws_row,
        })
    }

    /// Returns the report once the unit has fully finished, without waiting.
    pub fn exit_report(&self) -> Option<ExitReport> {
        self.inner.exit.borrow().clone()
    }

    /// Waits for the leader's exit, the output drain and the final sweep.
    pub async fn wait_report(&self) -> Result<ExitReport, Error> {
        let mut exit = self.inner.exit.subscribe();
        let report = exit
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::Integration("unit supervisor vanished".into()))?;
        report
            .clone()
            .ok_or_else(|| Error::Integration("unit report missing".into()))
    }

    /// Stops the unit and returns its report, including escalation and escaped processes.
    ///
    /// Sends `policy.signal` to the session's group and to tracked processes outside
    /// it, then SIGKILL after `policy.timeout`. Cancelling this future does not cancel
    /// the stop. Stopping an exited unit returns its existing report.
    pub async fn stop_with_report(
        &self,
        policy: devlanes_core::StopPolicy,
    ) -> Result<ExitReport, Error> {
        let signal = Signal::from_named_raw(policy.signal)
            .ok_or_else(|| Error::InvalidInput(format!("unknown signal {}", policy.signal)))?;
        // A closed channel cannot happen while `inner` is alive; the report covers late stops.
        let _ = self.inner.stops.send(StopRequest {
            signal,
            timeout: policy.timeout,
        });
        self.wait_report().await
    }

    /// The PTY master, or `Unsupported` for pipe units.
    fn master(&self) -> Result<&Arc<AsyncFd<OwnedFd>>, Error> {
        self.inner
            .master
            .as_ref()
            .ok_or_else(|| Error::Unsupported("pipe units have no terminal".into()))
    }
}

impl fmt::Debug for PtyUnit {
    /// Shows identity and state only; the spec and environment are never retained.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyUnit")
            .field("identity", &self.inner.identity)
            .field("pty", &self.is_pty())
            .field("exited", &self.inner.exit.borrow().is_some())
            .finish()
    }
}

#[async_trait]
impl Unit for PtyUnit {
    /// Writes all bytes to the PTY. Waits while the terminal's input buffer is full,
    /// so a program that never reads stdin can stall this call; callers may cancel it.
    async fn input(&self, bytes: Bytes) -> Result<(), Error> {
        let master = self.master()?;
        if self.inner.exit.borrow().is_some() {
            return Err(Error::Exited);
        }
        let _serial = self.inner.input_lock.lock().await;
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let mut guard = master.writable().await?;
            match guard.try_io(|fd| rustix::io::write(fd.get_ref(), rest).map_err(io::Error::from))
            {
                Ok(Ok(written)) => rest = &rest[written..],
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(error)) if error.raw_os_error() == Some(Errno::IO.raw_os_error()) => {
                    return Err(Error::Exited);
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(_would_block) => {}
            }
        }
        Ok(())
    }

    /// Applies the size to the PTY; the kernel delivers SIGWINCH to the foreground group.
    async fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        if size.columns == 0 || size.rows == 0 {
            return Err(Error::InvalidInput("terminal size must be non-zero".into()));
        }
        rustix::termios::tcsetwinsize(self.master()?.get_ref(), spawn::winsize(size))
            .map_err(io::Error::from)?;
        Ok(())
    }

    /// See [`PtyUnit::stop_with_report`].
    async fn stop(&self, policy: devlanes_core::StopPolicy) -> Result<(), Error> {
        self.stop_with_report(policy).await.map(|_| ())
    }

    /// See [`PtyUnit::wait_report`].
    async fn wait(&self) -> Result<ExitOutcome, Error> {
        Ok(self.wait_report().await?.outcome)
    }

    /// The first call returns a receiver created before the first read, so it sees
    /// all output since launch; later calls see output from the moment they subscribe.
    /// After exit, new receivers report `Closed` immediately.
    fn output(&self) -> broadcast::Receiver<OutputChunk> {
        let mut state = lock(&self.inner.output);
        if let Some(first) = state.first.take() {
            return first;
        }
        match &state.sender {
            Some(sender) => sender.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    /// Live leader and descendants with their current sessions.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error> {
        Ok(self
            .processes()
            .await?
            .iter()
            .map(ProcessInfo::identity)
            .collect())
    }
}
