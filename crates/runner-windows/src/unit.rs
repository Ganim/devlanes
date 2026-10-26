//! Unit handle and the per-unit supervisor task that waits for, tracks and
//! stops it.

use std::{
    fmt, io,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    Error, ExitOutcome, LaunchSpec, OutputChunk, Power, ProcessIdentity, StopPolicy, StopSignal,
    TerminalSize, Unit, WindowsStop,
};
use devlanes_sys::{ListeningSocket, ProcessInfo};
use devlanes_sys_windows::{
    console::CtrlEvent,
    job::Job,
    power::{PowerAccess, PowerHandle, PowerThrottling, PriorityClass},
    proc::{Held, ProcessEntry},
    spawn::Child,
    window::close_windows,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task,
    time::{self, Instant, MissedTickBehavior},
};

use crate::{
    ConsoleHelper, WindowsRunner,
    output::{self, Slot},
    spawn::{self, is_console_host},
    terminal::Terminal,
};

/// Poll period while waiting for members to exit after the leader, and for a
/// batch file's prompt during a stop.
const EXIT_POLL: Duration = Duration::from_millis(50);
/// Ctrl-C as a key: a console with processed input (the default) turns it
/// into `CTRL_C_EVENT` for every process attached to it.
const CTRL_C: &[u8] = b"\x03";
/// The answer to `cmd.exe`'s "Terminate batch job (Y/N)?".
const TERMINATE_BATCH: &[u8] = b"Y\r";
/// Poll period while waiting for a terminated job to empty.
const SWEEP_POLL: Duration = Duration::from_millis(25);
/// Exit code of processes the runner ends by terminating the job.
const TERMINATED: u32 = 1;
/// Most reads of the job a freeze makes. A member can start a process between
/// two reads, so the freeze reads again until a read finds nothing new.
const FREEZE_PASSES: usize = 5;

/// Everything known about a finished unit. Shared by all waiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    /// The leader's exit code; `None` if it could not be read. Windows has no
    /// signals, so `signal` is always `None`; a process ended by a console
    /// event reports `STATUS_CONTROL_C_EXIT` (`0xC000013A`) as its code.
    pub outcome: ExitOutcome,
    /// Present when an explicit stop ended the unit.
    pub stop: Option<StopOutcome>,
    /// Processes still running after the leader that the final sweep ended.
    pub swept: Vec<ProcessInfo>,
    /// Processes still running after the sweep's timeout.
    pub escaped: Vec<ProcessInfo>,
    /// Always empty on Windows: nothing a unit started may outlive it.
    pub left_running: Vec<ProcessInfo>,
}

/// How an explicit stop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    /// The first requested stop signal.
    pub signal: StopSignal,
    /// The job was terminated because the timeout elapsed first.
    pub escalated: bool,
}

/// Concrete unit handle. Clones of the `Arc` share one supervisor; dropping
/// every handle leaves the processes running until they exit on their own.
pub struct WindowsUnit {
    inner: Arc<Inner>,
}

/// State shared between handles and the supervisor task.
struct Inner {
    identity: ProcessIdentity,
    pty: bool,
    /// The leader is Git's `sh.exe` standing in for `/bin/sh`.
    through_shell: bool,
    job: Job,
    /// Held so the leader's PID stays its own until the unit is dropped.
    leader: Arc<Child>,
    slot: Arc<Slot>,
    /// Created before the first read, for the first subscriber.
    first: Mutex<Option<broadcast::Receiver<OutputChunk>>>,
    /// The pseudo console of a `pty: true` unit until teardown closes it.
    terminal: Mutex<Option<Terminal>>,
    /// The pseudo console's size.
    size: Mutex<TerminalSize>,
    /// A stop's Ctrl-C or Ctrl-Break went out.
    asked: AtomicBool,
    /// `cmd.exe`'s "Terminate batch job (Y/N)?" was answered, which happens
    /// once per unit.
    answered: AtomicBool,
    /// Live members other than the console host, as of the last refresh.
    members: Mutex<Vec<ProcessInfo>>,
    /// Members seen at a refresh that had not exited then, each held open:
    /// a terminated process leaves the job's list and count a moment before
    /// it has fully exited, and only its handle tells when it has.
    held: Mutex<Vec<(ProcessInfo, Held)>>,
    /// The unit's power level and the handles that hold it.
    power: Mutex<PowerState>,
    /// A stop began; from then on the unit stays at `Power::Normal`.
    stopping: AtomicBool,
    leader_exited: AtomicBool,
    /// Set when the supervisor ended without publishing a real report.
    crashed: AtomicBool,
    stops: mpsc::UnboundedSender<StopRequest>,
    exit: watch::Sender<Option<ExitReport>>,
    timing: Timing,
    helper: Option<ConsoleHelper>,
}

/// Supervisor tuning copied from the runner.
#[derive(Debug, Clone, Copy)]
struct Timing {
    track_interval: Duration,
    drain_timeout: Duration,
    sweep_timeout: Duration,
}

impl Timing {
    /// Upper bound for a stop beyond its policy timeout: the sweep, both output
    /// drains and slack for a loaded machine.
    fn stop_margin(&self) -> Duration {
        self.sweep_timeout + self.drain_timeout * 2 + Duration::from_secs(5)
    }
}

/// A stop request sent to the supervisor.
#[derive(Debug, Clone, Copy)]
struct StopRequest {
    signal: StopSignal,
    timeout: Duration,
}

/// Stop progress owned by the supervisor.
#[derive(Debug, Clone, Copy)]
struct Stopping {
    signal: StopSignal,
    deadline: Instant,
    escalated: bool,
}

/// Result of the final sweep.
#[derive(Debug, Default)]
struct Sweep {
    swept: Vec<ProcessInfo>,
    escaped: Vec<ProcessInfo>,
}

/// What the unit did to its members' scheduling, through handles it keeps
/// open so each change is undone once, on the same process.
#[derive(Debug, Default)]
struct PowerState {
    /// Members this unit suspended, held until the thaw.
    frozen: Vec<PowerHandle>,
    /// Members this unit put in efficiency mode.
    throttled: Vec<Throttled>,
    /// The unit is at `Power::Frozen`.
    is_frozen: bool,
    /// Efficiency mode is on, also under a freeze.
    is_throttled: bool,
}

/// A member in efficiency mode.
#[derive(Debug)]
struct Throttled {
    handle: PowerHandle,
    /// Its class and power throttling before; `None` for a member that
    /// started while the unit was throttled, which inherited the idle class
    /// from its parent and gets `NORMAL_PRIORITY_CLASS` with Windows deciding
    /// its throttling.
    before: Option<(PriorityClass, PowerThrottling)>,
}

impl PowerState {
    /// Whether the unit holds nothing: no suspension, no efficiency mode and
    /// no handle left from a failed undo.
    fn is_normal(&self) -> bool {
        !self.is_frozen && !self.is_throttled && self.frozen.is_empty() && self.throttled.is_empty()
    }

    /// Whether `process` is behind one of `handles`.
    fn has<'a>(mut handles: impl Iterator<Item = &'a PowerHandle>, process: &ProcessInfo) -> bool {
        handles.any(|h| h.pid() == process.pid && h.start_time() == process.start_time)
    }

    /// Suspends the members of `live` this unit has not suspended yet and
    /// returns how many it suspended. Blocking.
    fn suspend_new(&mut self, live: &[ProcessInfo]) -> io::Result<usize> {
        let mut added = 0;
        for process in live {
            if Self::has(self.frozen.iter(), process) {
                continue;
            }
            let Some(mut handle) =
                PowerHandle::open(process.pid, process.start_time, PowerAccess::Suspend)?
            else {
                continue;
            };
            handle.suspend()?;
            self.frozen.push(handle);
            added += 1;
        }
        Ok(added)
    }

    /// Resumes every member this unit suspended, each through the handle that
    /// suspended it. Every handle is tried. Handles that fail stay, with the
    /// unit still frozen if it was, so the next `Power::Normal` tries them
    /// again; the first error is returned. Blocking.
    fn resume(&mut self) -> io::Result<()> {
        let mut first = None;
        self.frozen.retain_mut(|handle| match handle.resume() {
            Ok(()) => false,
            Err(error) => {
                first.get_or_insert(error);
                true
            }
        });
        match first {
            None => {
                self.is_frozen = false;
                Ok(())
            }
            Some(error) => Err(error),
        }
    }

    /// Puts the members of `live` this unit has not throttled yet in
    /// efficiency mode: the idle priority class and EcoQoS on top of the
    /// throttling they chose. With `record`, each member's class and
    /// throttling are kept to restore. Blocking.
    fn throttle_new(&mut self, live: &[ProcessInfo], record: bool) -> io::Result<()> {
        for process in live {
            if Self::has(self.throttled.iter().map(|t| &t.handle), process) {
                continue;
            }
            let Some(handle) =
                PowerHandle::open(process.pid, process.start_time, PowerAccess::Throttle)?
            else {
                continue;
            };
            let throttling = handle.power_throttling()?;
            let before = if record {
                Some((handle.priority_class()?, throttling))
            } else {
                None
            };
            // Kept before the change, so a failure halfway is still undone.
            self.throttled.push(Throttled { handle, before });
            let handle = &self.throttled[self.throttled.len() - 1].handle;
            handle.set_priority_class(PriorityClass::IDLE)?;
            handle.set_power_throttling(throttling.with_eco_qos())?;
        }
        Ok(())
    }

    /// Takes every throttled member out of efficiency mode, back to the class
    /// and throttling it had. Every member is tried. Members that fail stay,
    /// with the unit still throttled if it was, so the next `Power::Normal`
    /// tries them again; the first error is returned. Blocking.
    fn unthrottle(&mut self) -> io::Result<()> {
        let mut first = None;
        self.throttled.retain(|Throttled { handle, before }| {
            if handle.exited() {
                return false;
            }
            let (class, throttling) =
                before.unwrap_or((PriorityClass::NORMAL, PowerThrottling::DEFAULT));
            let restored = handle
                .set_priority_class(class)
                .and_then(|()| handle.set_power_throttling(throttling));
            match restored {
                Ok(()) => false,
                Err(error) => {
                    first.get_or_insert(error);
                    true
                }
            }
        });
        match first {
            None => {
                self.is_throttled = false;
                Ok(())
            }
            Some(error) => Err(error),
        }
    }

    /// Back to `Power::Normal`: resumed and out of efficiency mode. Blocking.
    fn normal(&mut self) -> io::Result<()> {
        let resumed = self.resume();
        let unthrottled = self.unthrottle();
        resumed.and(unthrottled)
    }

    /// Closes the handles of members that exited, so their PIDs are not
    /// pinned for as long as the unit stays throttled.
    fn prune(&mut self) {
        self.frozen.retain(|handle| !handle.exited());
        self.throttled.retain(|t| !t.handle.exited());
    }
}

/// Launches the leader, starts its output readers and waiter, then its supervisor.
pub(crate) async fn spawn(
    runner: &WindowsRunner,
    spec: LaunchSpec,
) -> Result<Arc<WindowsUnit>, Error> {
    let stderr = runner.stderr;
    let shell = runner.shell.clone();
    let mut launched = blocking(move || spawn::launch(spec, stderr, shell.as_deref())).await??;
    let (terminal, size) = match launched.terminal.take() {
        Some((terminal, size)) => (Some(terminal), size),
        None => (None, TerminalSize::default()),
    };
    let pty = terminal.is_some();
    let identity = ProcessIdentity {
        pid: launched.child.pid(),
        sid: 0,
        start_time: launched.child.start_time(),
    };
    let (sender, first) = broadcast::channel(runner.output_capacity);
    let slot = Arc::new(Slot::new(sender));
    let (done_tx, done_rx) = watch::channel(false);
    let leader = Arc::new(launched.child);
    let (exit_tx, exit_rx) = oneshot::channel();
    let waiting = Arc::clone(&leader);
    let sizing = terminal
        .as_ref()
        .map(|terminal| Arc::clone(&terminal.sizing));
    let started =
        output::start(launched.pipes, Arc::clone(&slot), done_tx, sizing).and_then(|()| {
            std::thread::Builder::new()
                .name("unit-wait".into())
                .spawn(move || {
                    let _ = exit_tx.send(waiting.wait());
                })
                .map(drop)
        });
    if let Err(error) = started {
        // Unread pipes would stall it and nothing would see it exit.
        let _ = launched.job.terminate(TERMINATED);
        let _ = blocking(move || drop(terminal)).await;
        return Err(error.into());
    }

    let (stops, requests) = mpsc::unbounded_channel();
    let inner = Arc::new(Inner {
        identity,
        pty,
        through_shell: launched.through_shell,
        job: launched.job,
        leader,
        slot,
        first: Mutex::new(Some(first)),
        terminal: Mutex::new(terminal),
        size: Mutex::new(size),
        asked: AtomicBool::new(false),
        answered: AtomicBool::new(false),
        members: Mutex::new(Vec::new()),
        held: Mutex::new(Vec::new()),
        power: Mutex::new(PowerState::default()),
        stopping: AtomicBool::new(false),
        leader_exited: AtomicBool::new(false),
        crashed: AtomicBool::new(false),
        stops,
        exit: watch::channel(None).0,
        timing: Timing {
            track_interval: runner.track_interval,
            drain_timeout: runner.drain_timeout,
            sweep_timeout: runner.sweep_timeout,
        },
        helper: runner.helper.clone(),
    });
    // The guard is an argument, not a local, so it is dropped (and publishes) even if
    // the task is cancelled before its first poll.
    let publish = PublishOnDrop(Arc::clone(&inner));
    tokio::spawn(supervise(
        Arc::clone(&inner),
        publish,
        requests,
        exit_rx,
        done_rx,
    ));
    Ok(Arc::new(WindowsUnit { inner }))
}

/// Owns the unit lifecycle: the leader's exit, membership refreshes, stop
/// escalation and the final drain and sweep. Stops run here so a cancelled
/// `stop()` caller cannot abandon an escalation halfway.
async fn supervise(
    inner: Arc<Inner>,
    _publish: PublishOnDrop,
    mut requests: mpsc::UnboundedReceiver<StopRequest>,
    mut exited: oneshot::Receiver<io::Result<u32>>,
    mut output_done: watch::Receiver<bool>,
) {
    let timing = inner.timing;
    let mut tick = time::interval(timing.track_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stopping: Option<Stopping> = None;

    let code = loop {
        let deadline = stopping.filter(|s| !s.escalated).map(|s| s.deadline);
        tokio::select! {
            result = &mut exited => break exit_code(result),
            _ = tick.tick() => {
                inner.track().await;
            }
            Some(request) = requests.recv() => {
                let deadline = Instant::now() + request.timeout;
                match &mut stopping {
                    // A repeated stop can only shorten the grace period; it
                    // sends no second event.
                    Some(current) => current.deadline = current.deadline.min(deadline),
                    None => {
                        inner.begin_stop(request.signal, deadline);
                        stopping = Some(Stopping { signal: request.signal, deadline, escalated: false });
                    }
                }
            }
            () = until(deadline) => {
                inner.terminate_job();
                if let Some(current) = &mut stopping {
                    current.escalated = true;
                }
            }
            () = time::sleep(EXIT_POLL), if deadline.is_some() && inner.awaits_batch_answer() => {
                inner.answer_batch_prompt().await;
            }
        }
    };
    inner.leader_exited.store(true, Ordering::Release);

    if let Some(current) = &mut stopping {
        // Members that outlive the leader get the rest of the grace period. The
        // console host stays until its last client is gone and is not waited for.
        while !current.escalated && !inner.live_members().await.is_empty() {
            if Instant::now() >= current.deadline {
                inner.terminate_job();
                current.escalated = true;
            } else {
                inner.answer_batch_prompt().await;
                time::sleep(EXIT_POLL).await;
            }
        }
    } else if !inner.pty {
        // Leftover processes holding a pipe would block end of file forever.
        // A pseudo console's output only ends once it is closed, below.
        let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    }
    let sweep = inner.sweep().await;
    // Only once its processes are gone: closing a console its processes still
    // use only sends them `CTRL_CLOSE_EVENT`, and their last output is lost.
    inner.close_terminal().await;
    // Collect the last output of processes the sweep just ended.
    let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    inner.finish(ExitReport {
        outcome: ExitOutcome { code, signal: None },
        stop: stopping.map(|s| StopOutcome {
            signal: s.signal,
            escalated: s.escalated,
        }),
        swept: sweep.swept,
        escaped: sweep.escaped,
        left_running: Vec::new(),
    });
}

/// The leader's exit code from the waiter thread, `None` when it was lost.
fn exit_code(result: Result<io::Result<u32>, oneshot::error::RecvError>) -> Option<i32> {
    match result {
        Ok(Ok(code)) => Some(code.cast_signed()),
        Ok(Err(error)) => {
            tracing::warn!(%error, "waiting for a unit leader failed");
            None
        }
        Err(_) => None,
    }
}

/// Publishes a status-less report if the supervisor ends early (panic or runtime
/// shutdown), so waiters on other runtimes are not left hanging, and marks the unit
/// so a later stop terminates its job directly.
struct PublishOnDrop(Arc<Inner>);

impl Drop for PublishOnDrop {
    fn drop(&mut self) {
        if self.0.exit.borrow().is_none() {
            self.0.crashed.store(true, Ordering::SeqCst);
            self.0.finish(ExitReport {
                outcome: self.0.leader_outcome(),
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

/// Runs blocking Win32 work off the async executor.
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

/// A job member as a process-table entry; Windows has no sessions.
fn info(entry: ProcessEntry) -> ProcessInfo {
    ProcessInfo {
        pid: entry.pid,
        ppid: entry.ppid,
        sid: None,
        start_time: entry.start_time,
        zombie: entry.exited,
        name: entry.name,
    }
}

impl Inner {
    /// Live members other than the console host, read from the job now, and
    /// remembered for [`Unit::subjects`]. Members seen before that have not
    /// fully exited count too, through their held handles. Blocking.
    fn refresh_blocking(&self) -> Result<Vec<ProcessInfo>, Error> {
        let entries = self.job.processes()?;
        let mut held = lock(&self.held);
        held.retain(|(_, handle)| !handle.exited());
        let mut unheld = Vec::new();
        for entry in entries {
            if entry.exited
                || is_console_host(&entry.name)
                || held
                    .iter()
                    .any(|(p, _)| p.pid == entry.pid && p.start_time == entry.start_time)
            {
                continue;
            }
            // One that refuses to be held (or exited just now) is listed while
            // the job lists it.
            match Held::open(entry.pid, entry.start_time) {
                Ok(Some(handle)) => held.push((info(entry), handle)),
                Ok(None) | Err(_) => unheld.push(info(entry)),
            }
        }
        let live: Vec<ProcessInfo> = held.iter().map(|(p, _)| p.clone()).chain(unheld).collect();
        drop(held);
        lock(&self.members).clone_from(&live);
        Ok(live)
    }

    /// [`Inner::refresh_blocking`] for supervisor paths, where a failed read
    /// must not abort cleanup: the last known members stand in.
    async fn live_members(self: &Arc<Self>) -> Vec<ProcessInfo> {
        let inner = Arc::clone(self);
        match blocking(move || inner.refresh_blocking()).await {
            Ok(Ok(live)) => live,
            Ok(Err(error)) | Err(error) => {
                tracing::warn!(%error, pid = self.identity.pid, "reading the unit's job failed");
                lock(&self.members).clone()
            }
        }
    }

    /// The supervisor's periodic refresh: reads the members and gives those
    /// that started since the last one the unit's power level.
    async fn track(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        let tracked = blocking(move || {
            let live = inner.refresh_blocking()?;
            inner.track_power_blocking(&live);
            Ok::<_, Error>(())
        })
        .await;
        if let Ok(Err(error)) | Err(error) = tracked {
            tracing::warn!(%error, pid = self.identity.pid, "reading the unit's job failed");
        }
    }

    /// Suspends or throttles members of `live` the unit has not reached yet,
    /// while it is frozen or throttled. A member that started while the unit
    /// was throttled gets `NORMAL_PRIORITY_CLASS` back later. Best effort: a
    /// member that refuses is tried again at the next refresh. Blocking.
    fn track_power_blocking(&self, live: &[ProcessInfo]) {
        let mut state = lock(&self.power);
        state.prune();
        let result = if state.is_frozen {
            state.suspend_new(live).map(drop)
        } else if state.is_throttled {
            state.throttle_new(live, false)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            tracing::debug!(%error, pid = self.identity.pid, "a new member kept its power level");
        }
    }

    /// Moves the unit to `power`; see [`WindowsUnit::set_power`]. Blocking.
    fn set_power_blocking(&self, power: Power) -> Result<(), Error> {
        let mut state = lock(&self.power);
        // Checked under the lock: a stop that begins after this point resets
        // whatever this call sets.
        if self.leader_exited.load(Ordering::Acquire) || self.exit.borrow().is_some() {
            return Err(Error::Exited);
        }
        if self.stopping.load(Ordering::Acquire) {
            return match power {
                Power::Normal => Ok(()),
                Power::Throttled | Power::Frozen => Err(Error::Exited),
            };
        }
        match power {
            Power::Normal => Ok(state.normal()?),
            Power::Frozen => self.freeze(&mut state),
            Power::Throttled => self.throttle(&mut state),
        }
    }

    /// Suspends every member, reading the job again until a read finds no
    /// member left to suspend or [`FREEZE_PASSES`] reads were made; the
    /// refresh catches anything started after that. Efficiency mode stays as
    /// it was. On failure every member this unit suspended is resumed, as far
    /// as [`PowerState::resume`] gets. Blocking.
    fn freeze(&self, state: &mut PowerState) -> Result<(), Error> {
        let mut passes = || -> Result<(), Error> {
            for _ in 0..FREEZE_PASSES {
                if state.suspend_new(&self.refresh_blocking()?)? == 0 {
                    break;
                }
            }
            Ok(())
        };
        match passes() {
            Ok(()) => {
                state.is_frozen = true;
                Ok(())
            }
            Err(error) => {
                let _ = state.resume();
                Err(error)
            }
        }
    }

    /// Resumes a frozen unit, then puts every member in efficiency mode. The
    /// first call after `Power::Normal` records each member's priority class
    /// to restore. When a member refuses, every member is taken out of
    /// efficiency mode again. Blocking.
    fn throttle(&self, state: &mut PowerState) -> Result<(), Error> {
        state.resume()?;
        let record = !state.is_throttled;
        let result = self
            .refresh_blocking()
            .and_then(|live| Ok(state.throttle_new(&live, record)?));
        match result {
            Ok(()) => {
                state.is_throttled = true;
                Ok(())
            }
            Err(error) => {
                let _ = state.unthrottle();
                Err(error)
            }
        }
    }

    /// Puts the unit back at `Power::Normal` for a stop, logging what fails.
    /// Blocking.
    fn normal_for_stop(&self) {
        if let Err(error) = lock(&self.power).normal() {
            tracing::warn!(%error, pid = self.identity.pid, "restoring the unit's power level failed");
        }
    }

    /// Starts a stop: terminates the job for SIGKILL, otherwise asks politely
    /// in the background so the supervisor keeps watching the deadline.
    /// A pseudo console gets Ctrl-C as the key, which also reaches programs
    /// that read the console raw, as a terminal's interrupt character would.
    /// A frozen or throttled unit is put back at `Power::Normal` first.
    fn begin_stop(self: &Arc<Self>, signal: StopSignal, deadline: Instant) {
        self.stopping.store(true, Ordering::Release);
        let stop = signal.windows_stop(self.through_shell);
        if stop == WindowsStop::Terminate {
            self.terminate_job();
        }
        let deadline = deadline.into_std();
        if self.holds_no_power() {
            return self.ask_to_stop(stop, deadline);
        }
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            // Suspended threads handle no console event, key or window
            // message, so the unit runs again before it is asked to stop.
            // Terminating the job works on suspended processes, so SIGKILL
            // did not wait for this.
            let waking = Arc::clone(&inner);
            let _ = blocking(move || waking.normal_for_stop()).await;
            inner.ask_to_stop(stop, deadline);
        });
    }

    /// Whether the unit holds no suspension or efficiency mode, without
    /// waiting: a power change in progress counts as holding one. The stop
    /// flag is set before this is read, so a change that starts later is
    /// refused.
    fn holds_no_power(&self) -> bool {
        match self.power.try_lock() {
            Ok(state) => state.is_normal(),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().is_normal(),
            Err(TryLockError::WouldBlock) => false,
        }
    }

    /// Sends the graceful part of a stop: Ctrl-C as a key to a pseudo
    /// console, or a console event and `WM_CLOSE` through a blocking task.
    /// Nothing for `WindowsStop::Terminate`.
    fn ask_to_stop(self: &Arc<Self>, stop: WindowsStop, deadline: std::time::Instant) {
        let event = match stop {
            WindowsStop::Terminate => return,
            WindowsStop::CtrlC if self.pty => {
                self.write_input(Bytes::from_static(CTRL_C), true);
                None
            }
            WindowsStop::CtrlC => Some(CtrlEvent::CtrlC),
            WindowsStop::CtrlBreak => Some(CtrlEvent::CtrlBreak),
        };
        let inner = Arc::clone(self);
        task::spawn_blocking(move || inner.ask_to_close(event, deadline));
    }

    /// Queues `bytes` for the pseudo console's input without waiting for the
    /// write; with `asking`, records once it is written that the stop's event
    /// went out. Nothing happens for pipe units or a closed console.
    fn write_input(self: &Arc<Self>, bytes: Bytes, asking: bool) {
        let Some(queue) = lock(&self.terminal).as_ref().map(|t| t.input.clone()) else {
            return;
        };
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let (reply, written) = oneshot::channel();
            if queue.send((bytes, reply)).await.is_ok()
                && matches!(written.await, Ok(Ok(())))
                && asking
            {
                inner.asked.store(true, Ordering::Release);
            }
        });
    }

    /// Whether a stop may still need to answer a batch file's prompt.
    fn awaits_batch_answer(&self) -> bool {
        self.pty && !self.answered.load(Ordering::Acquire)
    }

    /// Answers "Terminate batch job (Y/N)?" with `Y`: a `.cmd` shim
    /// (`npm.cmd`) asks that on the console after Ctrl-C or Ctrl-Break once
    /// what it ran has exited, and would otherwise hold the stop until its
    /// timeout. This is expected cleanup, so it is not an escalation. Done
    /// once, after the stop's event went out, when only `cmd.exe` processes
    /// are left. In a pipe unit `cmd.exe` reads end of input from `NUL` there
    /// and goes on by itself.
    async fn answer_batch_prompt(self: &Arc<Self>) {
        if !self.awaits_batch_answer() || !self.asked.load(Ordering::Acquire) {
            return;
        }
        let live = self.live_members().await;
        let shims_only =
            !live.is_empty() && live.iter().all(|p| p.name.eq_ignore_ascii_case("cmd.exe"));
        if shims_only && !self.answered.swap(true, Ordering::AcqRel) {
            tracing::debug!(pid = self.identity.pid, "answering a batch file's prompt");
            self.write_input(Bytes::from_static(TERMINATE_BATCH), false);
        }
    }

    /// Closes the pseudo console, which ends its output, off the executor.
    async fn close_terminal(&self) {
        let terminal = lock(&self.terminal).take();
        if terminal.is_some() {
            let _ = blocking(move || drop(terminal)).await;
        }
    }

    /// Posts `WM_CLOSE` to the members' visible windows and sends `event`, if
    /// any, once to the unit's console, through the leader while it runs, else
    /// through another live member. Waits for the helper until `deadline`.
    /// Blocking.
    fn ask_to_close(&self, event: Option<CtrlEvent>, deadline: std::time::Instant) {
        let live: Vec<ProcessEntry> = match self.job.processes() {
            Ok(entries) => entries
                .into_iter()
                .filter(|entry| !entry.exited && !is_console_host(&entry.name))
                .collect(),
            Err(error) => {
                tracing::warn!(%error, pid = self.identity.pid, "reading the unit's job failed");
                return;
            }
        };
        let targets: Vec<(u32, u64)> = live.iter().map(|p| (p.pid, p.start_time)).collect();
        match close_windows(&targets) {
            Ok(0) => {}
            Ok(closed) => {
                tracing::debug!(closed, pid = self.identity.pid, "asked windows to close")
            }
            Err(error) => tracing::debug!(%error, "closing windows failed"),
        }
        let (Some(event), Some(helper)) = (event, &self.helper) else {
            return;
        };
        let is_leader = |p: &&ProcessEntry| {
            p.pid == self.identity.pid && p.start_time == self.identity.start_time
        };
        let held = live
            .iter()
            .filter(is_leader)
            .chain(live.iter().filter(|p| !is_leader(p)))
            .find_map(|p| Held::open(p.pid, p.start_time).ok().flatten());
        let Some(target) = held else {
            tracing::debug!(
                pid = self.identity.pid,
                "no live process to send a console event to"
            );
            return;
        };
        match helper.send(&target, event, deadline) {
            Ok(()) => {
                self.asked.store(true, Ordering::Release);
                tracing::debug!(?event, pid = target.pid(), "console event sent");
            }
            Err(error) => {
                tracing::warn!(%error, ?event, pid = target.pid(), "console event not sent")
            }
        }
    }

    /// Ends every process in the job, the console host included.
    fn terminate_job(&self) {
        if let Err(error) = self.job.terminate(TERMINATED) {
            tracing::warn!(%error, pid = self.identity.pid, "terminating the unit's job failed");
        }
    }

    /// [`Inner::sweep_blocking`] off the executor.
    async fn sweep(self: &Arc<Self>) -> Sweep {
        let inner = Arc::clone(self);
        blocking(move || inner.sweep_blocking())
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, pid = self.identity.pid, "sweeping the unit's job failed");
                self.terminate_job();
                Sweep::default()
            })
    }

    /// Terminates the job and waits until every member that was running has
    /// exited and the job is empty, or the sweep times out. Members running
    /// before are `swept`; any still running at the timeout `escaped`.
    /// Blocking.
    fn sweep_blocking(&self) -> Sweep {
        if let Err(error) = self.refresh_blocking() {
            tracing::warn!(%error, pid = self.identity.pid, "reading the unit's job failed");
        }
        let held = std::mem::take(&mut *lock(&self.held));
        self.terminate_job();
        // Left over when the leader exited on its own while the unit was
        // throttled; from here on the handles would only pin PIDs.
        drop(std::mem::take(&mut *lock(&self.power)));
        let deadline = std::time::Instant::now() + self.timing.sweep_timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let mut sweep = Sweep::default();
        for (process, handle) in held {
            if handle.wait(remaining()).unwrap_or(false) {
                sweep.swept.push(process);
            } else {
                sweep.escaped.push(process);
            }
        }
        // Members that started after the list was read.
        while !matches!(self.job.active_count(), Ok(0)) && !remaining().is_zero() {
            std::thread::sleep(SWEEP_POLL);
        }
        sweep
    }

    /// The leader's outcome without waiting: its code once it exited.
    fn leader_outcome(&self) -> ExitOutcome {
        ExitOutcome {
            code: self.leader.exit_code().ok().flatten().map(u32::cast_signed),
            signal: None,
        }
    }

    /// Publishes the report and closes the subscription source.
    fn finish(&self, report: ExitReport) {
        self.slot.close();
        lock(&self.members).clear();
        lock(&self.held).clear();
        self.exit.send_replace(Some(report));
    }
}

impl WindowsUnit {
    /// Leader identity recorded at spawn; persist it for reconciliation. `sid`
    /// is 0: Windows has no sessions.
    pub fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    /// Reports whether the unit runs in a pseudo console (`pty: true`).
    pub fn is_pty(&self) -> bool {
        self.inner.pty
    }

    /// Reads the job and returns its live processes, the leader included while
    /// it runs and the console host left out.
    pub async fn processes(&self) -> Result<Vec<ProcessInfo>, Error> {
        let inner = Arc::clone(&self.inner);
        blocking(move || inner.refresh_blocking()).await?
    }

    /// Lists TCP listeners of the unit's processes, for ready-check diagnostics.
    pub async fn listening_ports(&self) -> Result<Vec<ListeningSocket>, Error> {
        let pids: Vec<u32> = self.processes().await?.iter().map(|p| p.pid).collect();
        Ok(blocking(move || devlanes_sys::listening_tcp(&pids)).await??)
    }

    /// The pseudo console's size; `Unsupported` for pipe units, `Exited` once
    /// the unit finished.
    pub fn size(&self) -> Result<TerminalSize, Error> {
        if !self.inner.pty {
            return Err(Error::Unsupported("pipe units have no terminal".into()));
        }
        if self.inner.exit.borrow().is_some() {
            return Err(Error::Exited);
        }
        Ok(*lock(&self.inner.size))
    }

    /// Moves the unit's processes, the job's members other than the console
    /// host, to `power`.
    ///
    /// `Frozen` suspends every member (`NtSuspendProcess`), reading the job
    /// again until no new member shows up, at most 5 times, and keeps each
    /// process's handle open, so the thaw resumes exactly the processes it
    /// suspended and a reused PID is never touched. `Throttled` is efficiency
    /// mode: each member's priority class and power throttling are recorded,
    /// then it gets `IDLE_PRIORITY_CLASS` and EcoQoS; members that start
    /// later get the same at the supervisor's next membership refresh.
    /// `Normal` resumes and restores the recorded class and throttling
    /// (`NORMAL_PRIORITY_CLASS` and Windows' default throttling for members
    /// that started while throttled). Moving from `Frozen` to `Throttled`
    /// resumes first. Memory is never trimmed.
    ///
    /// A member that refuses fails the call: a failed freeze resumes what it
    /// suspended, and a failed throttle takes every member out of efficiency
    /// mode. When `Normal` cannot resume or restore a member, it returns the
    /// error and keeps that member's handle and the unit's level, so the next
    /// `Normal` tries again. A stop puts the unit back at `Normal` before it
    /// asks the unit to close, and from then on only `Normal` is accepted.
    /// `Exited` once the leader exited.
    pub async fn set_power(&self, power: Power) -> Result<(), Error> {
        if self.inner.exit.borrow().is_some() {
            return Err(Error::Exited);
        }
        let inner = Arc::clone(&self.inner);
        blocking(move || inner.set_power_blocking(power)).await?
    }

    /// Ends every process of the unit at once by terminating its job, without
    /// waiting; the supervisor then reports the exit as usual.
    pub fn terminate(&self) {
        self.inner.terminate_job();
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

    /// Stops the unit and returns its report, including escalation.
    ///
    /// SIGKILL terminates the job. Every other signal posts `WM_CLOSE` to the
    /// unit's visible windows and sends one console event (see
    /// [`StopSignal::windows_stop`]) to its console; once only the console
    /// host is left, or at `policy.timeout`, the job is terminated. A repeated
    /// stop only shortens the timeout. Cancelling this future does not cancel
    /// the stop. Stopping an exited unit returns its existing report. If the
    /// supervisor task died, the job is terminated directly instead.
    pub async fn stop_with_report(&self, policy: StopPolicy) -> Result<ExitReport, Error> {
        let request = StopRequest {
            signal: policy.signal,
            timeout: policy.timeout,
        };
        // A closed channel means the supervisor ended; only a crash leaves work undone.
        if self.inner.stops.send(request).is_err() && self.inner.crashed.load(Ordering::SeqCst) {
            return self.terminate_and_settle(policy.signal).await;
        }
        let bound = policy
            .timeout
            .saturating_add(self.inner.timing.stop_margin());
        match time::timeout(bound, self.wait_report()).await {
            Ok(report) => report,
            Err(_) => self.terminate_and_settle(policy.signal).await,
        }
    }

    /// For a stop the supervisor cannot finish (it died, or its report is
    /// overdue): terminates the job, waits for the leader and publishes a
    /// report unless a real one exists. A leader still running is an error.
    async fn terminate_and_settle(&self, signal: StopSignal) -> Result<ExitReport, Error> {
        tracing::warn!(
            pid = self.inner.identity.pid,
            "unit supervisor did not finish the stop; terminating its job"
        );
        self.inner.terminate_job();
        let leader = Arc::clone(&self.inner.leader);
        let wait = self.inner.timing.sweep_timeout;
        let code = blocking(move || leader.wait_timeout(wait)).await??;
        self.inner.close_terminal().await;
        let Some(code) = code else {
            return Err(Error::Integration(format!(
                "unit {} still running after stop",
                self.inner.identity.pid
            )));
        };
        let report = ExitReport {
            outcome: ExitOutcome {
                code: Some(code.cast_signed()),
                signal: None,
            },
            stop: Some(StopOutcome {
                signal,
                escalated: signal != StopSignal::Kill,
            }),
            swept: Vec::new(),
            escaped: Vec::new(),
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
        self.inner.slot.close();
        self.wait_report().await
    }
}

impl fmt::Debug for WindowsUnit {
    /// Shows identity and state only; the spec and environment are never retained.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsUnit")
            .field("identity", &self.inner.identity)
            .field("pty", &self.inner.pty)
            .field("exited", &self.inner.exit.borrow().is_some())
            .finish()
    }
}

#[async_trait]
impl Unit for WindowsUnit {
    /// Writes all bytes to the pseudo console's input, in call order. Waits
    /// while the console does not read, so a program that never reads its
    /// input can stall this call; a caller that gives up still gets its bytes
    /// written, in their place. Pipe units have `NUL` as stdin and take none.
    async fn input(&self, bytes: Bytes) -> Result<(), Error> {
        if self.inner.exit.borrow().is_some() {
            return Err(Error::Exited);
        }
        if !self.inner.pty {
            return Err(Error::Unsupported(
                "pipe units take no terminal input".into(),
            ));
        }
        let queue = lock(&self.inner.terminal)
            .as_ref()
            .map(|terminal| terminal.input.clone())
            .ok_or(Error::Exited)?;
        let (reply, written) = oneshot::channel();
        queue
            .send((bytes, reply))
            .await
            .map_err(|_| Error::Exited)?;
        match written.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => Err(Error::Exited),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(Error::Exited),
        }
    }

    /// Resizes the pseudo console; its programs see the new size, and the
    /// console host repaints its window. The output marks where the new size
    /// begins (see [`Unit::renders_screen`]); a resize asked for while another
    /// is still being repainted follows that repaint. `Unsupported` for pipe
    /// units.
    async fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        if size.columns == 0 || size.rows == 0 {
            return Err(Error::InvalidInput("terminal size must be non-zero".into()));
        }
        if !self.inner.pty {
            return Err(Error::Unsupported("pipe units have no terminal".into()));
        }
        let sizing = lock(&self.inner.terminal)
            .as_ref()
            .map(|terminal| Arc::clone(&terminal.sizing))
            .ok_or(Error::Exited)?;
        // The console host answers through a pipe; off the executor.
        blocking(move || sizing.request(size)).await??;
        *lock(&self.inner.size) = size;
        Ok(())
    }

    /// See [`WindowsUnit::stop_with_report`].
    async fn stop(&self, policy: StopPolicy) -> Result<(), Error> {
        self.stop_with_report(policy).await.map(|_| ())
    }

    /// See [`WindowsUnit::set_power`].
    async fn set_power(&self, power: Power) -> Result<(), Error> {
        WindowsUnit::set_power(self, power).await
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
        self.inner.slot.subscribe()
    }

    /// Live members as of the supervisor's last refresh. Never reads the job, so
    /// frequent callers such as the metrics sampler add no work.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error> {
        let mut subjects: Vec<ProcessIdentity> = lock(&self.inner.members)
            .iter()
            .map(ProcessInfo::identity)
            .collect();
        // Before the first refresh the list is empty; the leader is known anyway.
        if subjects.is_empty() && !self.inner.leader_exited.load(Ordering::Acquire) {
            subjects.push(self.inner.identity);
        }
        Ok(subjects)
    }

    /// The console host behind a pseudo console sends its rendering of the
    /// screen, not what the programs wrote; the reader marks each new size.
    fn renders_screen(&self) -> bool {
        self.inner.pty
    }
}
