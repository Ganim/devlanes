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
use devlanes_sys::{
    DescendantTracker, ListeningSocket, OpenFile, ProcessInfo, Signal, SignalOutcome,
};
use rustix::{fs::OFlags, io::Errno, process::WaitOptions};
use tokio::{
    io::unix::AsyncFd,
    signal::unix::{SignalKind, signal},
    sync::{broadcast, mpsc, watch},
    task::{self, AbortHandle},
    time::{self, Instant, MissedTickBehavior},
};

use crate::{ExitSweep, PtyRunner, output, snapshot, spawn};

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
    /// Processes still alive at the end: tracked ones that survived SIGKILL, and
    /// untracked ones still holding the unit's terminal or output pipe.
    pub escaped: Vec<ProcessInfo>,
    /// Daemonized descendants deliberately left running by
    /// [`ExitSweep::KeepDaemonized`] after a natural exit.
    pub left_running: Vec<ProcessInfo>,
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
    pty: bool,
    /// Dropped once the report is published, so a process that still holds the slave
    /// cannot keep the PTY pair alive.
    master: Mutex<Option<Arc<AsyncFd<OwnedFd>>>>,
    /// Reader tasks, aborted after the final drain.
    readers: Mutex<Vec<AbortHandle>>,
    /// The slave or pipe write ends the leader got, to find untracked holders.
    held: Vec<OpenFile>,
    /// Keeps concurrent inputs from interleaving partial writes.
    input_lock: tokio::sync::Mutex<()>,
    output: Mutex<OutputState>,
    tracker: Mutex<DescendantTracker>,
    /// The leader's status once waitpid returned it; the PID may then be recycled.
    reaped: Mutex<Option<ExitOutcome>>,
    /// Mirrors `reaped.is_some()` for lock-free reads.
    leader_reaped: AtomicBool,
    /// Set when the supervisor ended without publishing a real report.
    crashed: AtomicBool,
    stops: mpsc::UnboundedSender<StopRequest>,
    exit: watch::Sender<Option<ExitReport>>,
    timing: Timing,
    /// Lets tests kill the supervisor to exercise the crash fallback.
    #[cfg(test)]
    supervisor: Mutex<Option<AbortHandle>>,
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
    exit_sweep: ExitSweep,
}

impl Timing {
    /// Upper bound for a stop beyond its policy timeout: the sweep, both output
    /// drains and slack for scans on a loaded machine.
    fn stop_margin(&self) -> Duration {
        self.sweep_timeout + self.drain_timeout * 2 + Duration::from_secs(5)
    }
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

/// Result of the final sweep.
#[derive(Debug, Default)]
struct Sweep {
    swept: Vec<ProcessInfo>,
    escaped: Vec<ProcessInfo>,
    left_running: Vec<ProcessInfo>,
}

/// Outcome when the status is unknown.
const UNKNOWN: ExitOutcome = ExitOutcome {
    code: None,
    signal: None,
};

/// Launches the leader, wires output readers and starts its supervisor.
pub(crate) async fn spawn(runner: &PtyRunner, spec: LaunchSpec) -> Result<Arc<PtyUnit>, Error> {
    // Listening before fork guarantees the leader's SIGCHLD cannot be missed.
    let sigchld = signal(SignalKind::child())?;
    let nofile = devlanes_sys::child_nofile_limit();
    let stderr = runner.stderr;
    let pty = spec.pty;
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
            let readers = output::start(master.clone(), launched.pipes, sender.clone(), done_tx)?;
            Ok((master, readers))
        });
    let (master, readers) = match wired {
        Ok(wired) => wired,
        Err(error) => {
            let _ = blocking(move || spawn::abandon(identity.pid)).await;
            return Err(error.into());
        }
    };

    let (stops, requests) = mpsc::unbounded_channel();
    let inner = Arc::new(Inner {
        identity,
        pty,
        master: Mutex::new(master),
        readers: Mutex::new(readers),
        held: launched.held,
        input_lock: tokio::sync::Mutex::new(()),
        output: Mutex::new(OutputState {
            sender: Some(sender),
            first: Some(first),
        }),
        tracker: Mutex::new({
            let mut tracker = DescendantTracker::new(identity);
            // The leader is our unreaped child until the supervisor says otherwise.
            tracker.hold_root(true);
            tracker
        }),
        reaped: Mutex::new(None),
        leader_reaped: AtomicBool::new(false),
        crashed: AtomicBool::new(false),
        stops,
        exit: watch::channel(None).0,
        timing: Timing {
            track_interval: runner.track_interval,
            drain_timeout: runner.drain_timeout,
            sweep_timeout: runner.sweep_timeout,
            exit_sweep: runner.exit_sweep,
        },
        #[cfg(test)]
        supervisor: Mutex::new(None),
    });
    // The guard is an argument, not a local, so it is dropped (and publishes) even if
    // the task is cancelled before its first poll.
    let publish = PublishOnDrop(Arc::clone(&inner));
    let _supervisor = tokio::spawn(supervise(
        Arc::clone(&inner),
        publish,
        sigchld,
        requests,
        done_rx,
    ));
    #[cfg(test)]
    {
        *lock(&inner.supervisor) = Some(_supervisor.abort_handle());
    }
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
    _publish: PublishOnDrop,
    mut sigchld: tokio::signal::unix::Signal,
    mut requests: mpsc::UnboundedReceiver<StopRequest>,
    mut output_done: watch::Receiver<bool>,
) {
    let timing = inner.timing;
    let mut tick = time::interval(timing.track_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stopping: Option<Stopping> = None;

    let outcome = loop {
        // Checked every wakeup: SIGCHLD coalesces and may belong to other children.
        if exited_unreaped(inner.identity.pid) {
            // Last scan while the zombie leader still owns its PID and session ID, so
            // session members forked since the previous tick are adopted safely.
            inner.refresh_logged(Duration::ZERO).await;
        }
        if let Some(outcome) = inner.reap_leader() {
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
    let mut sweep = inner.sweep(stopping.is_some()).await;
    // Collect the last output of processes the sweep just killed.
    let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    if !*output_done.borrow() {
        // EOF never came: something outside the tracked tree still holds an end.
        let known: Vec<u32> = (sweep.swept.iter())
            .chain(&sweep.escaped)
            .chain(&sweep.left_running)
            .map(|p| p.pid)
            .collect();
        let holders = inner.untracked_holders(known).await;
        sweep.escaped.extend(holders);
    }
    inner.finish(ExitReport {
        outcome,
        stop: stopping.map(|s| StopOutcome {
            signal: s.signal.as_raw(),
            escalated: s.escalated,
        }),
        swept: sweep.swept,
        escaped: sweep.escaped,
        left_running: sweep.left_running,
    });
    inner.release_output();
}

/// Publishes a status-less report if the supervisor ends early (panic or runtime
/// shutdown), so waiters on other runtimes are not left hanging, and marks the unit
/// so a later stop signals its processes directly.
struct PublishOnDrop(Arc<Inner>);

impl Drop for PublishOnDrop {
    fn drop(&mut self) {
        if self.0.exit.borrow().is_none() {
            self.0.crashed.store(true, Ordering::SeqCst);
            self.0.finish(ExitReport {
                outcome: self.0.reaped_outcome().unwrap_or(UNKNOWN),
                stop: None,
                swept: Vec::new(),
                escaped: Vec::new(),
                left_running: Vec::new(),
            });
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

/// Reports whether the leader has exited, without reaping it (`WNOWAIT`).
fn exited_unreaped(pid: u32) -> bool {
    use rustix::process::{WaitId, WaitIdOptions};
    let Some(raw) = raw_pid(pid) else {
        return false;
    };
    matches!(
        rustix::process::waitid(
            WaitId::Pid(raw),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ),
        Ok(Some(_))
    )
}

/// One non-blocking waitpid for the leader; never waits for any other PID.
fn waitpid(pid: u32) -> Option<ExitOutcome> {
    let Some(raw) = raw_pid(pid) else {
        return Some(UNKNOWN);
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
                return Some(UNKNOWN);
            }
        }
    }
}

impl Inner {
    /// Collects the leader's status once and keeps it, so the supervisor and an
    /// overdue stop can both ask without either losing it.
    fn reap_leader(&self) -> Option<ExitOutcome> {
        let mut reaped = lock(&self.reaped);
        if let Some(outcome) = *reaped {
            return Some(outcome);
        }
        let outcome = waitpid(self.identity.pid)?;
        *reaped = Some(outcome);
        self.leader_reaped.store(true, Ordering::Release);
        // The PID is free now; its session ID may be reused by a stranger.
        lock(&self.tracker).hold_root(false);
        Some(outcome)
    }

    /// The stored status, without calling waitpid.
    fn reaped_outcome(&self) -> Option<ExitOutcome> {
        *lock(&self.reaped)
    }

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

    /// Live tracked processes after a fresh scan; concurrent units share scans.
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
        // A stopped process (Ctrl-Z, SIGSTOP) only acts on the signal once continued.
        let resume = signal != Signal::KILL && signal != Signal::CONT;
        if group_alive && let Some(pid) = raw_pid(leader) {
            let _ = rustix::process::kill_process_group(pid, signal);
            if resume {
                let _ = rustix::process::kill_process_group(pid, Signal::CONT);
            }
        }
        for process in processes.iter().filter(|p| !p.zombie) {
            // Group members already got the signal; a second delivery trips the
            // "press again to force quit" handlers of many dev servers.
            if group_alive && process.pgid == leader {
                continue;
            }
            let identity = process.identity();
            match devlanes_sys::signal_verified(&identity, signal) {
                Ok(SignalOutcome::Sent) if resume => {
                    let _ = devlanes_sys::signal_verified(&identity, Signal::CONT);
                }
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, pid = process.pid, "signal failed"),
            }
        }
    }

    /// SIGKILLs live tracked processes until none remain or the sweep times out.
    /// After a natural exit in [`ExitSweep::KeepDaemonized`] mode, descendants that
    /// left the unit's session are reported instead of killed.
    async fn sweep(self: &Arc<Self>, explicit_stop: bool) -> Sweep {
        let keep_daemonized = !explicit_stop && self.timing.exit_sweep == ExitSweep::KeepDaemonized;
        let session = self.identity.sid;
        let deadline = Instant::now() + self.timing.sweep_timeout;
        let mut killed = BTreeMap::new();
        loop {
            let (targets, kept): (Vec<_>, Vec<_>) = self
                .live_members()
                .await
                .into_iter()
                .partition(|p| !keep_daemonized || p.sid == Some(session));
            if targets.is_empty() || Instant::now() >= deadline {
                for process in &targets {
                    killed.remove(&process.pid);
                }
                return Sweep {
                    swept: killed.into_values().collect(),
                    escaped: targets,
                    left_running: kept,
                };
            }
            let sent = blocking(move || {
                targets
                    .into_iter()
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

    /// Finds live processes of this user, started after the leader and not already
    /// reported, that hold the unit's slave or output pipe open.
    async fn untracked_holders(self: &Arc<Self>, known: Vec<u32>) -> Vec<ProcessInfo> {
        if self.held.is_empty() {
            return Vec::new();
        }
        let held = self.held.clone();
        let started = self.identity.start_time;
        let found = blocking(move || -> Result<Vec<ProcessInfo>, devlanes_sys::Error> {
            let uid = devlanes_sys::current_uid();
            // The daemon holds the master and the pipe read ends itself.
            let me = std::process::id();
            let candidates: Vec<ProcessInfo> = snapshot::processes(Duration::ZERO)?
                .iter()
                .filter(|p| {
                    p.uid == uid
                        && !p.zombie
                        && p.pid != me
                        && p.start_time >= started
                        && !known.contains(&p.pid)
                })
                .cloned()
                .collect();
            let pids: Vec<u32> = candidates.iter().map(|p| p.pid).collect();
            let mut holding = Vec::new();
            for target in held {
                holding.extend(devlanes_sys::holders(target, &pids)?);
            }
            Ok(candidates
                .into_iter()
                .filter(|p| holding.contains(&p.pid))
                .collect())
        })
        .await;
        match found {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::warn!(%error, "searching output holders failed");
                Vec::new()
            }
            Err(error) => {
                tracing::warn!(%error, "searching output holders failed");
                Vec::new()
            }
        }
    }

    /// Publishes the report and closes the subscription source.
    fn finish(&self, report: ExitReport) {
        lock(&self.output).sender = None;
        self.exit.send_replace(Some(report));
    }

    /// Ends readers still waiting for EOF and closes the master, so nothing outlives
    /// the report except handles that callers still hold.
    fn release_output(&self) {
        for reader in lock(&self.readers).drain(..) {
            reader.abort();
        }
        lock(&self.master).take();
    }

    /// The PTY master while the unit runs; `Unsupported` for pipe units, `Exited`
    /// once released.
    fn master(&self) -> Result<Arc<AsyncFd<OwnedFd>>, Error> {
        if !self.pty {
            return Err(Error::Unsupported("pipe units have no terminal".into()));
        }
        lock(&self.master).clone().ok_or(Error::Exited)
    }

    /// Signals the live tracked set directly, for when the supervisor is gone.
    /// Returns the processes signalled. Blocking.
    fn signal_without_supervisor(&self, signal: Signal) -> Vec<ProcessInfo> {
        let live: Vec<ProcessInfo> = self
            .refresh_blocking(Duration::ZERO)
            .unwrap_or_else(|_| lock(&self.tracker).processes().cloned().collect())
            .into_iter()
            .filter(|p| !p.zombie)
            .collect();
        for process in &live {
            let _ = devlanes_sys::signal_verified(&process.identity(), signal);
        }
        live
    }
}

impl PtyUnit {
    /// Leader identity recorded right after spawn; persist it for reconciliation.
    pub fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    /// Identities in the tracked set as of the last snapshot, without scanning.
    /// Blocking-safe and cheap; callers that persist them can later sweep the
    /// session with [`devlanes_sys::session_members_known`] even after the leader
    /// is gone.
    pub fn tracked(&self) -> Vec<ProcessIdentity> {
        lock(&self.inner.tracker)
            .processes()
            .map(ProcessInfo::identity)
            .collect()
    }

    /// Reports whether the unit runs in a PTY rather than pipes.
    pub fn is_pty(&self) -> bool {
        self.inner.pty
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

    /// Returns the current PTY size; `Exited` once the unit's terminal was released.
    pub fn size(&self) -> Result<TerminalSize, Error> {
        let master = self.inner.master()?;
        let size = rustix::termios::tcgetwinsize(master.get_ref()).map_err(io::Error::from)?;
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
    /// Sends `policy.signal` (then SIGCONT, so stopped processes act on it) to the
    /// session's group and to tracked processes outside it, then SIGKILL after
    /// `policy.timeout`. Cancelling this future does not cancel the stop. Stopping an
    /// exited unit returns its existing report. If the supervisor task died, the
    /// tracked processes are signalled directly instead.
    pub async fn stop_with_report(
        &self,
        policy: devlanes_core::StopPolicy,
    ) -> Result<ExitReport, Error> {
        let signal = Signal::from_named_raw(policy.signal)
            .ok_or_else(|| Error::InvalidInput(format!("unknown signal {}", policy.signal)))?;
        let request = StopRequest {
            signal,
            timeout: policy.timeout,
        };
        // A closed channel means the supervisor ended; only a crash leaves work undone.
        if self.inner.stops.send(request).is_err() && self.inner.crashed.load(Ordering::SeqCst) {
            return self.stop_without_supervisor(request).await;
        }
        let bound = policy
            .timeout
            .saturating_add(self.inner.timing.stop_margin());
        match time::timeout(bound, self.wait_report()).await {
            Ok(report) => report,
            Err(_) => self.settle_overdue_stop(signal).await,
        }
    }

    /// Runs when a stop outlived the supervisor's own worst case. The leader is reaped
    /// here if it exited, so the placeholder carries its real status and no zombie is
    /// left behind; a leader still running is an error. The supervisor's full report
    /// replaces the placeholder if it arrives later.
    async fn settle_overdue_stop(&self, signal: Signal) -> Result<ExitReport, Error> {
        let Some(outcome) = self.inner.reap_leader() else {
            return Err(Error::Integration(format!(
                "unit {} still running after stop",
                self.inner.identity.pid
            )));
        };
        tracing::warn!(
            pid = self.inner.identity.pid,
            "unit report overdue after stop; leader has exited"
        );
        self.publish_placeholder(outcome, signal, true, Vec::new());
        self.wait_report().await
    }

    /// Stops the tracked set directly after the supervisor died: the signal, SIGKILL
    /// after the timeout, then a reap of the leader.
    async fn stop_without_supervisor(&self, request: StopRequest) -> Result<ExitReport, Error> {
        tracing::warn!(
            pid = self.inner.identity.pid,
            "unit supervisor is gone; signalling directly"
        );
        let deadline = Instant::now() + request.timeout;
        let give_up = deadline + self.inner.timing.sweep_timeout;
        let mut signal = request.signal;
        let mut escalated = false;
        let survivors = loop {
            let inner = Arc::clone(&self.inner);
            let live = blocking(move || inner.signal_without_supervisor(signal)).await?;
            // The leader is our child; reap it as soon as it exits so it is not
            // reported as live forever.
            self.inner.reap_leader();
            if live.is_empty() || (escalated && Instant::now() >= give_up) {
                break live;
            }
            if !escalated && Instant::now() >= deadline {
                signal = Signal::KILL;
                escalated = true;
            }
            time::sleep(EXIT_POLL).await;
        };
        let outcome = self.inner.reap_leader().unwrap_or(UNKNOWN);
        self.publish_placeholder(outcome, request.signal, escalated, survivors);
        self.inner.release_output();
        self.wait_report().await
    }

    /// Publishes a report built outside the supervisor unless a real one exists.
    fn publish_placeholder(
        &self,
        outcome: ExitOutcome,
        signal: Signal,
        escalated: bool,
        escaped: Vec<ProcessInfo>,
    ) {
        let report = ExitReport {
            outcome,
            stop: Some(StopOutcome {
                signal: signal.as_raw(),
                escalated,
            }),
            swept: Vec::new(),
            escaped,
            left_running: Vec::new(),
        };
        // The crash report is itself a placeholder; anything else is real.
        let crashed = self.inner.crashed.load(Ordering::SeqCst);
        self.inner.exit.send_if_modified(|exit| {
            if exit.is_some() && !crashed {
                return false;
            }
            *exit = Some(report);
            true
        });
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
        let master = self.inner.master()?;
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
        let master = self.inner.master()?;
        rustix::termios::tcsetwinsize(master.get_ref(), spawn::winsize(size))
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

    /// Live leader and descendants as of the supervisor's last snapshot. Never scans,
    /// so frequent callers such as the metrics sampler add no process-table walks.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error> {
        let mut subjects: Vec<ProcessIdentity> = lock(&self.inner.tracker)
            .live()
            .map(ProcessInfo::identity)
            .collect();
        // Before the first snapshot the tracker is empty; the leader is known anyway.
        if subjects.is_empty() && !self.inner.leader_reaped.load(Ordering::Acquire) {
            subjects.push(self.inner.identity);
        }
        Ok(subjects)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use devlanes_core::StopPolicy;

    use super::*;

    /// Upper bound for each test body.
    const LIMIT: Duration = Duration::from_secs(20);

    /// A pipe-mode `sh -c script` with a minimal environment.
    fn shell(script: &str) -> LaunchSpec {
        LaunchSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            pty: false,
            size: TerminalSize::default(),
        }
    }

    /// Kills the leader if a test fails before stopping it.
    struct Kill(ProcessIdentity);

    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = devlanes_sys::signal_verified(&self.0, Signal::KILL);
        }
    }

    /// With the supervisor gone, stop still signals the processes and reaps the leader
    /// instead of returning the crash placeholder as if the unit had ended.
    #[tokio::test]
    async fn stop_after_supervisor_death_signals_directly() {
        tokio::time::timeout(LIMIT, async {
            let unit = PtyRunner::default()
                .spawn_unit(shell("sleep 30"))
                .await
                .expect("spawn");
            let _kill = Kill(unit.identity());
            let supervisor = lock(&unit.inner.supervisor).take().expect("handle");
            supervisor.abort();
            let mut exit = unit.inner.exit.subscribe();
            exit.wait_for(Option::is_some).await.expect("crash report");
            assert!(devlanes_sys::matches(&unit.identity()).expect("lookup"));

            let report = unit
                .stop_with_report(StopPolicy {
                    signal: 15,
                    timeout: Duration::from_secs(2),
                })
                .await
                .expect("stop");
            assert_eq!(report.outcome.signal, Some(15));
            assert!(report.escaped.is_empty(), "{:?}", report.escaped);
            assert!(!devlanes_sys::matches(&unit.identity()).expect("lookup"));
            // Reaped by the fallback, so not even a zombie remains.
            assert_eq!(
                devlanes_sys::process(unit.identity().pid).expect("lookup"),
                None
            );
        })
        .await
        .expect("bounded");
    }

    /// An overdue stop reaps an exited leader itself, so the placeholder carries the
    /// real status and the supervisor's later report agrees with it.
    #[tokio::test]
    async fn overdue_stop_reaps_before_publishing() {
        tokio::time::timeout(LIMIT, async {
            let unit = PtyRunner::default()
                .spawn_unit(shell("exit 3"))
                .await
                .expect("spawn");
            // Wait for the exit; whichever side reaps first, the status must survive.
            while !exited_unreaped(unit.identity().pid) && unit.inner.reaped_outcome().is_none() {
                time::sleep(Duration::from_millis(5)).await;
            }
            let report = unit
                .settle_overdue_stop(Signal::TERM)
                .await
                .expect("settled");
            assert_eq!(report.outcome.code, Some(3));
            let last = unit.wait_report().await.expect("report");
            assert_eq!(last.outcome.code, Some(3));
        })
        .await
        .expect("bounded");
    }

    /// `subjects` reads the tracker and never triggers a process-table scan.
    #[tokio::test]
    async fn subjects_do_not_scan() {
        tokio::time::timeout(LIMIT, async {
            let unit = PtyRunner::default()
                .with_track_interval(Duration::from_secs(60))
                .spawn_unit(shell("sleep 30"))
                .await
                .expect("spawn");
            let _kill = Kill(unit.identity());
            // Let the supervisor's immediate first tick finish its scan.
            time::sleep(Duration::from_millis(200)).await;
            let before = snapshot::scans_started();
            for _ in 0..50 {
                let subjects = unit.subjects().await.expect("subjects");
                assert!(subjects.iter().any(|s| s.pid == unit.identity().pid));
            }
            // Other tests in this process may scan meanwhile, but not within a loop
            // this short.
            assert!(snapshot::scans_started() - before < 5);
        })
        .await
        .expect("bounded");
    }
}
