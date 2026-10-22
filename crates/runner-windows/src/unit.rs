//! Unit handle and the per-unit supervisor task that waits on, tracks and stops it.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    io::{self, Read},
    os::windows::io::AsHandle,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    Error, ExitOutcome, ExitSweep, LaunchSpec, OutputChunk, OutputStream, ProcessIdentity,
    StderrMode, StopPolicy, TerminalSize, Unit,
};
use devlanes_sys::windows::{
    job::Job,
    proc::terminate_verified,
    spawn::{Child, Spawn, Stdio, spawn},
};
use devlanes_sys::{DescendantTracker, ListeningSocket, ProcessInfo};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task,
    time::{self, Instant, MissedTickBehavior},
};

use crate::{WindowsRunner, command};

/// Read size per call; large enough for bursty build output.
const CHUNK: usize = 16 * 1024;
/// Poll period between rounds of the final sweep.
const SWEEP_POLL: Duration = Duration::from_millis(25);
/// Exit code given to processes the runner terminates, like a failed command.
const TERMINATED: u32 = 1;

/// Everything known about a finished unit. Shared by all waiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    /// The leader's exit code; `None` when the supervisor ended without one.
    pub outcome: ExitOutcome,
    /// Present when an explicit stop ended the unit.
    pub stop: Option<StopOutcome>,
    /// Leftover processes terminated by the final sweep after the leader exited.
    pub swept: Vec<ProcessInfo>,
    /// Tracked processes still alive at the end, after the sweep gave up.
    pub escaped: Vec<ProcessInfo>,
    /// Descendants outside the unit's job, deliberately left running by
    /// [`ExitSweep::KeepDaemonized`] after a natural exit.
    pub left_running: Vec<ProcessInfo>,
}

/// How an explicit stop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    /// The processes were terminated rather than asked to exit. Always true until
    /// console control events are sent (graceful stop comes with the lifecycle work).
    pub escalated: bool,
}

/// Concrete unit handle. Clones of the `Arc` share one supervisor; dropping every
/// handle leaves the processes running until they exit on their own.
pub struct WindowsUnit {
    inner: Arc<Inner>,
}

/// State shared between handles, the supervisor task and the reader threads.
struct Inner {
    identity: ProcessIdentity,
    /// Every process the leader starts joins it unless it breaks away.
    job: Job,
    /// Keeps the leader's PID from being reused while the unit is supervised.
    child: Arc<Child>,
    /// The source subscribers clone from, also used by the reader threads. Set to
    /// `None` once the report is published, which closes every receiver even when
    /// a reader is still blocked on a pipe some leftover process holds.
    sender: Arc<Mutex<Option<broadcast::Sender<OutputChunk>>>>,
    /// The receiver created before the first read, for the first subscriber.
    first: Mutex<Option<broadcast::Receiver<OutputChunk>>>,
    tracker: Mutex<DescendantTracker>,
    leader_exited: AtomicBool,
    stops: mpsc::UnboundedSender<StopPolicy>,
    exit: watch::Sender<Option<ExitReport>>,
    timing: Timing,
}

/// Supervisor tuning copied from the runner.
#[derive(Debug, Clone, Copy)]
struct Timing {
    track_interval: Duration,
    drain_timeout: Duration,
    sweep_timeout: Duration,
    exit_sweep: ExitSweep,
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

/// What the blocking launch hands back to async code.
struct Launched {
    job: Job,
    child: Arc<Child>,
    pipes: Vec<(OutputStream, io::PipeReader)>,
}

/// Launches the leader, starts its output readers and its supervisor.
pub(crate) async fn spawn_unit(
    runner: &WindowsRunner,
    spec: LaunchSpec,
) -> Result<Arc<WindowsUnit>, Error> {
    let stderr = runner.stderr;
    let launched = blocking(move || launch(&spec, stderr)).await??;
    let child = launched.child;
    let identity = ProcessIdentity {
        pid: child.pid(),
        // No process group survives a daemon crash on Windows, so a process is its
        // own group: every member is recorded on its own for orphan cleanup.
        sid: child.pid(),
        start_time: child.start_time(),
    };

    let (sender, first) = broadcast::channel(runner.output_capacity);
    let sender = Arc::new(Mutex::new(Some(sender)));
    let output_done = start_readers(launched.pipes, Arc::clone(&sender))?;
    let exited = wait_for_exit(Arc::clone(&child));

    let (stops, requests) = mpsc::unbounded_channel();
    let inner = Arc::new(Inner {
        identity,
        job: launched.job,
        child,
        sender,
        first: Mutex::new(Some(first)),
        tracker: Mutex::new(DescendantTracker::new(identity)),
        leader_exited: AtomicBool::new(false),
        stops,
        exit: watch::channel(None).0,
        timing: Timing {
            track_interval: runner.track_interval,
            drain_timeout: runner.drain_timeout,
            sweep_timeout: runner.sweep_timeout,
            exit_sweep: runner.exit_sweep,
        },
    });
    // The guard is an argument, not a local, so it is dropped (and publishes) even if
    // the task is cancelled before its first poll.
    let publish = PublishOnDrop(Arc::clone(&inner));
    tokio::spawn(supervise(
        Arc::clone(&inner),
        publish,
        exited,
        requests,
        output_done,
    ));
    Ok(Arc::new(WindowsUnit { inner }))
}

/// Resolves the program, creates the job and pipes, and starts the leader inside
/// the job. Blocking.
fn launch(spec: &LaunchSpec, stderr: StderrMode) -> Result<Launched, Error> {
    let args: Vec<OsString> = spec.args.iter().map(OsString::from).collect();
    let command = command::build(OsStr::new(&spec.program), &args, &spec.env, &spec.cwd)
        .map_err(command_error)?;
    // Breakaway is allowed: some tools start helpers with CREATE_BREAKAWAY_FROM_JOB
    // and fail outright when refused. Those helpers are still tracked by parent.
    let job = Job::create(true)?;
    let (stdout_reader, stdout_writer) = io::pipe()?;
    let separate = match stderr {
        StderrMode::Separate => Some(io::pipe()?),
        StderrMode::Merged => None,
    };
    let stderr_handle = separate
        .as_ref()
        .map_or(stdout_writer.as_handle(), |(_, writer)| writer.as_handle());
    let child = spawn(&Spawn {
        program: &command.application,
        command_line: &command.command_line,
        environment: Some(&spec.env),
        cwd: &spec.cwd,
        stdout: Stdio::Handle(stdout_writer.as_handle()),
        stderr: Stdio::Handle(stderr_handle),
        job: Some(&job),
    })?;
    // Only the child holds the write ends from here on, so its exit (and that of
    // everything it handed them to) ends the readers.
    drop(stdout_writer);
    let mut pipes = vec![(OutputStream::Stdout, stdout_reader)];
    if let Some((reader, writer)) = separate {
        drop(writer);
        pipes.push((OutputStream::Stderr, reader));
    }
    Ok(Launched {
        job,
        child: Arc::new(child),
        pipes,
    })
}

/// Maps a resolution failure to the errors the daemon's spawn diagnostics read. A
/// missing program carries `ERROR_FILE_NOT_FOUND`, as `CreateProcessW` would
/// report it, so the diagnostics name the code like any other spawn failure.
fn command_error(error: command::CommandError) -> Error {
    /// `ERROR_FILE_NOT_FOUND`.
    const FILE_NOT_FOUND: i32 = 2;
    match &error {
        command::CommandError::NotFound { .. } => {
            Error::Io(io::Error::from_raw_os_error(FILE_NOT_FOUND))
        }
        _ => Error::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            error.to_string(),
        )),
    }
}

/// Starts one reader thread per pipe; the returned receiver turns true once every
/// pipe reached end of file.
///
/// Anonymous pipes cannot be read asynchronously, so each gets a thread. A thread
/// blocked on a pipe that a leftover process still holds ends when that process
/// exits; the unit no longer waits for it once its report is published.
fn start_readers(
    pipes: Vec<(OutputStream, io::PipeReader)>,
    sender: Arc<Mutex<Option<broadcast::Sender<OutputChunk>>>>,
) -> io::Result<watch::Receiver<bool>> {
    let (done_tx, done_rx) = watch::channel(false);
    let mut readers = Vec::new();
    for (stream, pipe) in pipes {
        let sender = Arc::clone(&sender);
        readers.push(
            std::thread::Builder::new()
                .name("devlanes-unit-output".into())
                .spawn(move || read_pipe(stream, pipe, &sender))?,
        );
    }
    std::thread::Builder::new()
        .name("devlanes-unit-output-done".into())
        .spawn(move || {
            for reader in readers {
                let _ = reader.join();
            }
            done_tx.send_replace(true);
        })?;
    Ok(done_rx)
}

/// Reads one pipe until every writer closed it, publishing each chunk.
fn read_pipe(
    stream: OutputStream,
    mut pipe: io::PipeReader,
    sender: &Mutex<Option<broadcast::Sender<OutputChunk>>>,
) {
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                // Having no subscribers is not an error; lagging ones lose chunks
                // rather than stalling the unit on a full pipe.
                if let Some(sender) = lock(sender).as_ref() {
                    let _ = sender.send(OutputChunk {
                        stream,
                        bytes: Bytes::copy_from_slice(&buffer[..read]),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // ERROR_BROKEN_PIPE is how an anonymous pipe reports end of file.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return,
            Err(error) => {
                tracing::debug!(%error, "pipe read failed");
                return;
            }
        }
    }
}

/// Waits on the leader's process handle in a thread and delivers its exit code.
fn wait_for_exit(child: Arc<Child>) -> oneshot::Receiver<io::Result<u32>> {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("devlanes-unit-wait".into())
        .spawn(move || {
            let _ = sender.send(child.wait());
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot start the exit waiter; the unit is polled instead");
    }
    receiver
}

/// Owns the unit lifecycle: exit, periodic tracking, stops and the post-exit drain
/// and sweep. Stops run here so a cancelled `stop()` caller cannot abandon one.
async fn supervise(
    inner: Arc<Inner>,
    _publish: PublishOnDrop,
    mut exited: oneshot::Receiver<io::Result<u32>>,
    mut requests: mpsc::UnboundedReceiver<StopPolicy>,
    mut output_done: watch::Receiver<bool>,
) {
    let timing = inner.timing;
    let mut tick = time::interval(timing.track_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stopped = false;

    let outcome = loop {
        tokio::select! {
            code = &mut exited => break inner.leader_outcome(code.ok()),
            _ = tick.tick() => {
                // Without a waiter thread, the tick notices the exit too.
                if let Ok(Some(code)) = inner.child.exit_code() {
                    break inner.leader_outcome(Some(Ok(code)));
                }
                inner.refresh_logged(timing.track_interval / 2).await;
            }
            Some(_policy) = requests.recv() => {
                if !stopped {
                    inner.terminate_all().await;
                    stopped = true;
                }
            }
        }
    };

    if !stopped {
        // Leftover processes holding a pipe would block end of file forever.
        let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    }
    let sweep = inner.sweep(stopped).await;
    // Collect the last output of processes the sweep just ended.
    let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    inner.finish(ExitReport {
        outcome,
        stop: stopped.then_some(StopOutcome { escalated: true }),
        swept: sweep.swept,
        escaped: sweep.escaped,
        left_running: sweep.left_running,
    });
}

/// Publishes a status-less report if the supervisor ends early (panic or runtime
/// shutdown), so waiters are not left hanging.
struct PublishOnDrop(Arc<Inner>);

impl Drop for PublishOnDrop {
    fn drop(&mut self) {
        if self.0.exit.borrow().is_none() {
            let outcome = match self.0.child.exit_code() {
                Ok(Some(code)) => exit_outcome(code),
                _ => UNKNOWN,
            };
            self.0.finish(ExitReport {
                outcome,
                stop: None,
                swept: Vec::new(),
                escaped: Vec::new(),
                left_running: Vec::new(),
            });
        }
    }
}

/// Runs blocking process-table or process-creation work off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Error> {
    task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Integration(format!("blocking task failed: {error}")))
}

/// Locks a std mutex, ignoring poisoning: the guarded data stays consistent per call.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A Windows exit code as an outcome. Codes such as `0xC000013A` (Ctrl-C) are
/// kept as the signed value Windows tools show; there are no signals.
fn exit_outcome(code: u32) -> ExitOutcome {
    ExitOutcome {
        code: Some(code.cast_signed()),
        signal: None,
    }
}

/// The identity a tracked process is reported and recorded with: like the leader,
/// its own PID stands in for the group.
fn member_identity(process: &ProcessInfo) -> ProcessIdentity {
    ProcessIdentity {
        pid: process.pid,
        sid: process.pid,
        start_time: process.start_time,
    }
}

impl Inner {
    /// Records that the leader exited and turns its wait result into an outcome.
    fn leader_outcome(&self, code: Option<io::Result<u32>>) -> ExitOutcome {
        self.leader_exited.store(true, Ordering::Release);
        match code {
            Some(Ok(code)) => exit_outcome(code),
            Some(Err(error)) => {
                tracing::warn!(%error, pid = self.identity.pid, "unit leader status lost");
                UNKNOWN
            }
            // The waiter thread is gone; the handle still knows the code.
            None => match self.child.exit_code() {
                Ok(Some(code)) => exit_outcome(code),
                _ => UNKNOWN,
            },
        }
    }

    /// Updates the tracker from a snapshot at most `max_age` old plus the job's
    /// current members, and returns all tracked processes. Blocking.
    fn refresh_blocking(&self, max_age: Duration) -> Result<Vec<ProcessInfo>, Error> {
        let snapshot = devlanes_sys::shared_processes(max_age)?;
        // Read after the snapshot, so every member listed here is either in it or
        // started later and is found by the next refresh.
        let members: BTreeSet<u32> = self.job.members()?.into_iter().collect();
        let mut tracker = lock(&self.tracker);
        tracker.update(&snapshot);
        for process in snapshot.iter().filter(|p| members.contains(&p.pid)) {
            tracker.adopt(process);
        }
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

    /// Terminates the job and every tracked process that left it.
    async fn terminate_all(self: &Arc<Self>) {
        let targets = self.live_members().await;
        let inner = Arc::clone(self);
        let _ = blocking(move || inner.terminate_blocking(&targets)).await;
    }

    /// Ends the job's members, then each target by verified identity, which
    /// reaches processes that broke away. Blocking.
    fn terminate_blocking(&self, targets: &[ProcessInfo]) {
        if let Err(error) = self.job.terminate(TERMINATED) {
            tracing::debug!(%error, pid = self.identity.pid, "terminating the unit's job failed");
        }
        for process in targets {
            // Gone or a mismatch is fine: the job already ended it, or the PID now
            // names someone else and was left alone.
            if let Err(error) = terminate_verified(&member_identity(process), TERMINATED) {
                tracing::debug!(%error, pid = process.pid, "terminate failed");
            }
        }
    }

    /// Ends live tracked processes until none remain or the sweep times out. After
    /// a natural exit in [`ExitSweep::KeepDaemonized`] mode, descendants outside
    /// the job (started detached or with breakaway) are reported instead.
    async fn sweep(self: &Arc<Self>, explicit_stop: bool) -> Sweep {
        let keep_daemonized = !explicit_stop && self.timing.exit_sweep == ExitSweep::KeepDaemonized;
        let deadline = Instant::now() + self.timing.sweep_timeout;
        let mut ended = BTreeMap::new();
        loop {
            let live = self.live_members().await;
            let in_job: BTreeSet<u32> =
                self.job.members().unwrap_or_default().into_iter().collect();
            let (targets, kept): (Vec<_>, Vec<_>) = live
                .into_iter()
                .partition(|p| !keep_daemonized || in_job.contains(&p.pid));
            if targets.is_empty() || Instant::now() >= deadline {
                for process in &targets {
                    ended.remove(&process.pid);
                }
                return Sweep {
                    swept: ended.into_values().collect(),
                    escaped: targets,
                    left_running: kept,
                };
            }
            let inner = Arc::clone(self);
            // Survivors show up in the next round and end as escaped at the deadline.
            let ended_now = blocking(move || {
                if keep_daemonized {
                    // The job holds exactly the processes to end; kept ones are outside.
                    if let Err(error) = inner.job.terminate(TERMINATED) {
                        tracing::debug!(%error, "terminating the unit's job failed");
                    }
                } else {
                    inner.terminate_blocking(&targets);
                }
                targets
            })
            .await
            .unwrap_or_default();
            for process in ended_now {
                ended.entry(process.pid).or_insert(process);
            }
            time::sleep(SWEEP_POLL).await;
        }
    }

    /// Publishes the report and closes the subscription source, which ends every
    /// receiver even if a reader thread stays blocked on a held pipe.
    fn finish(&self, report: ExitReport) {
        lock(&self.sender).take();
        lock(&self.first).take();
        self.exit.send_replace(Some(report));
    }
}

impl WindowsUnit {
    /// Leader identity recorded right after spawn; persist it for reconciliation.
    pub fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    /// Rescans and returns live tracked processes, the leader included while it runs.
    pub async fn processes(&self) -> Result<Vec<ProcessInfo>, Error> {
        let mut processes = self.inner.refresh(Duration::ZERO).await?;
        processes.retain(|p| !p.zombie);
        Ok(processes)
    }

    /// Lists TCP listeners of the tracked process tree, for ready-check diagnostics.
    pub async fn listening_ports(&self) -> Result<Vec<ListeningSocket>, Error> {
        let pids: Vec<u32> = self.processes().await?.iter().map(|p| p.pid).collect();
        Ok(blocking(move || devlanes_sys::listening_tcp(&pids)).await??)
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

    /// Stops the unit and returns its report.
    ///
    /// Every process is terminated at once: the job's members, then tracked
    /// processes that left the job, each checked by identity first. Asking them to
    /// exit with Ctrl-C or Ctrl-Break first, honouring `policy`, comes with the
    /// lifecycle work. Cancelling this future does not cancel the stop; stopping an
    /// exited unit returns its existing report.
    pub async fn stop_with_report(&self, policy: StopPolicy) -> Result<ExitReport, Error> {
        if self.inner.stops.send(policy).is_err() && self.exit_report().is_none() {
            // The supervisor is gone without a report; end what is left directly.
            self.inner.terminate_all().await;
        }
        let bound = self.inner.timing.sweep_timeout
            + self.inner.timing.drain_timeout * 2
            + Duration::from_secs(5);
        time::timeout(bound, self.wait_report())
            .await
            .map_err(|_| {
                Error::Integration(format!(
                    "unit {} still running after stop",
                    self.inner.identity.pid
                ))
            })?
    }
}

impl fmt::Debug for WindowsUnit {
    /// Shows identity and state only; the spec and environment are never retained.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsUnit")
            .field("identity", &self.inner.identity)
            .field("exited", &self.inner.exit.borrow().is_some())
            .finish()
    }
}

#[async_trait]
impl Unit for WindowsUnit {
    /// Units run over pipes until the pseudo console lands, so there is no
    /// terminal to type into.
    async fn input(&self, _bytes: Bytes) -> Result<(), Error> {
        Err(Error::Unsupported(
            "units on Windows have no terminal yet".into(),
        ))
    }

    /// No terminal to resize; see [`WindowsUnit::input`].
    async fn resize(&self, _size: TerminalSize) -> Result<(), Error> {
        Err(Error::Unsupported(
            "units on Windows have no terminal yet".into(),
        ))
    }

    /// See [`WindowsUnit::stop_with_report`].
    async fn stop(&self, policy: StopPolicy) -> Result<(), Error> {
        self.stop_with_report(policy).await.map(|_| ())
    }

    /// See [`WindowsUnit::wait_report`].
    async fn wait(&self) -> Result<ExitOutcome, Error> {
        Ok(self.wait_report().await?.outcome)
    }

    /// The first call returns a receiver created before the first read, so it sees
    /// all output since launch; later calls see output from the moment they subscribe.
    /// After exit, new receivers report `Closed` immediately.
    fn output(&self) -> broadcast::Receiver<OutputChunk> {
        if let Some(first) = lock(&self.inner.first).take() {
            return first;
        }
        match lock(&self.inner.sender).as_ref() {
            Some(sender) => sender.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    /// Live leader and descendants as of the supervisor's last snapshot. Never scans,
    /// so frequent callers such as the metrics sampler add no process-table walks.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error> {
        let mut subjects: Vec<ProcessIdentity> = lock(&self.inner.tracker)
            .live()
            .map(member_identity)
            .collect();
        // Before the first snapshot the tracker is empty; the leader is known anyway.
        if subjects.is_empty() && !self.inner.leader_exited.load(Ordering::Acquire) {
            subjects.push(self.inner.identity);
        }
        Ok(subjects)
    }
}
