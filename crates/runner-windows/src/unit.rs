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
    StderrMode, StopPolicy, StopSignal, TerminalSize, Unit, WindowsStop,
};
use devlanes_sys::windows::{
    conpty::PseudoConsole,
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

use crate::ctrl::{ConsoleHelper, Request};
use crate::{WindowsRunner, command};

/// Read size per call; large enough for bursty build output.
const CHUNK: usize = 16 * 1024;
/// Poll period between rounds of the final sweep.
const SWEEP_POLL: Duration = Duration::from_millis(25);
/// Exit code given to processes the runner terminates, like a failed command.
const TERMINATED: u32 = 1;
/// How long a pseudo console's output must stay quiet before it is closed.
const OUTPUT_PAUSE: Duration = Duration::from_millis(100);
/// Poll period while a stop waits for the unit to exit.
const EXIT_POLL: Duration = Duration::from_millis(100);
/// Ctrl-C as a key, which a console in processed-input mode turns into
/// `CTRL_C_EVENT` for every process attached to it.
const CTRL_C: &[u8] = b"\x03";

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
    /// The unit runs in a pseudo console (`pty: true`).
    pty: bool,
    /// The pseudo console while the unit runs; taken (closed) once its processes
    /// are ended, which is what ends its output.
    terminal: Mutex<Option<Terminal>>,
    /// When the readers last published output, for closing the console only once
    /// it went quiet.
    last_output: Arc<Mutex<Instant>>,
    /// Sends console control events and lists console members; `None` disables
    /// graceful stops of pipe units.
    helper: Option<ConsoleHelper>,
    /// Processes seen attached to the unit's own console (PID and start time),
    /// for deciding what a task keeps after it exits. `None` until a listing
    /// succeeded; only recorded for pipe tasks with a helper.
    on_console: Mutex<Option<BTreeSet<(u32, u64)>>>,
    leader_exited: AtomicBool,
    /// Set when the supervisor ended without publishing a real report.
    crashed: AtomicBool,
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
    /// The pseudo console and the write end of its input, for `pty: true`.
    terminal: Option<Terminal>,
}

/// A unit's pseudo console. Closing it (dropping `console`) ends output and the
/// processes still attached to it.
struct Terminal {
    console: PseudoConsole,
    /// Feeds the writer thread. Dropping it ends the thread, which closes the
    /// console's input pipe.
    input: mpsc::Sender<Input>,
}

/// Bytes for the console's input, and where to report the write's result.
type Input = (Bytes, oneshot::Sender<io::Result<()>>);

/// Input chunks queued before callers wait; enough for typing and pastes.
const INPUT_QUEUE: usize = 64;

/// Starts the thread that writes queued input to the console in order. Writes
/// block while the console does not read, so they cannot run on the executor,
/// and one thread keeps them in order even when callers give up waiting: a write
/// already queued still happens, in its place.
fn input_writer(mut pipe: io::PipeWriter) -> io::Result<mpsc::Sender<Input>> {
    let (sender, mut queue) = mpsc::channel::<Input>(INPUT_QUEUE);
    std::thread::Builder::new()
        .name("devlanes-unit-input".into())
        .spawn(move || {
            while let Some((bytes, reply)) = queue.blocking_recv() {
                let _ = reply.send(std::io::Write::write_all(&mut pipe, &bytes));
            }
        })?;
    Ok(sender)
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
    let last_output = Arc::new(Mutex::new(Instant::now()));
    let readers = start_readers(
        launched.pipes,
        Arc::clone(&sender),
        Arc::clone(&last_output),
    );
    let output_done = match readers {
        Ok(output_done) => output_done,
        Err(error) => {
            // Nothing would supervise or ever stop the leader; end it now.
            let _ = launched.job.terminate(TERMINATED);
            return Err(error.into());
        }
    };
    let exited = wait_for_exit(Arc::clone(&child));

    let (stops, requests) = mpsc::unbounded_channel();
    let inner = Arc::new(Inner {
        identity,
        job: launched.job,
        child,
        sender,
        first: Mutex::new(Some(first)),
        tracker: Mutex::new(DescendantTracker::new(identity)),
        pty: launched.terminal.is_some(),
        terminal: Mutex::new(launched.terminal),
        last_output,
        helper: runner.helper.clone(),
        on_console: Mutex::new(None),
        leader_exited: AtomicBool::new(false),
        crashed: AtomicBool::new(false),
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
    let cwd = plain_drive_path(&spec.cwd);
    if spec.pty {
        return launch_in_console(spec, &command, job, &cwd);
    }
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
        cwd: &cwd,
        stdout: Stdio::Handle(stdout_writer.as_handle()),
        stderr: Stdio::Handle(stderr_handle),
        job: Some(&job),
        console: None,
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
        terminal: None,
    })
}

/// Starts the leader attached to a new pseudo console of the spec's size. Its
/// output arrives as one terminal stream; input and resizes go to the console.
fn launch_in_console(
    spec: &LaunchSpec,
    command: &command::Command,
    job: Job,
    cwd: &std::path::Path,
) -> Result<Launched, Error> {
    let (input_reader, input_writer_pipe) = io::pipe()?;
    // Started before the leader, so a failure here leaves nothing running.
    let input = input_writer(input_writer_pipe)?;
    let (output_reader, output_writer) = io::pipe()?;
    let console = PseudoConsole::create(
        spec.size.columns,
        spec.size.rows,
        input_reader.as_handle(),
        output_writer.as_handle(),
    )?;
    let child = spawn(&Spawn {
        program: &command.application,
        command_line: &command.command_line,
        environment: Some(&spec.env),
        cwd,
        stdout: Stdio::Null,
        stderr: Stdio::Null,
        job: Some(&job),
        console: Some(&console),
    })?;
    // The console holds its own copies; ours would keep output from ever ending.
    drop((input_reader, output_writer));
    Ok(Launched {
        job,
        child: Arc::new(child),
        pipes: vec![(OutputStream::Terminal, output_reader)],
        terminal: Some(Terminal { console, input }),
    })
}

/// `path` without a `\\?\` prefix when it names a plain drive path. `cmd.exe`
/// cannot use a verbatim working directory and silently starts in the Windows
/// directory instead, which would run batch files and shell strings elsewhere.
fn plain_drive_path(path: &std::path::Path) -> std::path::PathBuf {
    // Paths that are not valid Unicode are left as they are rather than mangled.
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') && rest.len() < 260 => rest.into(),
        _ => path.to_owned(),
    }
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
    last_output: Arc<Mutex<Instant>>,
) -> io::Result<watch::Receiver<bool>> {
    let (done_tx, done_rx) = watch::channel(false);
    let mut readers = Vec::new();
    for (stream, pipe) in pipes {
        let sender = Arc::clone(&sender);
        let last_output = Arc::clone(&last_output);
        readers.push(
            std::thread::Builder::new()
                .name("devlanes-unit-output".into())
                .spawn(move || read_pipe(stream, pipe, &sender, &last_output))?,
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
    last_output: &Mutex<Instant>,
) {
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                *lock(last_output) = Instant::now();
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
        // The supervisor's tick still polls the handle, only less promptly.
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
    let mut stopping: Option<Stopping> = None;
    let mut waiter_alive = true;

    let outcome = loop {
        let asking = stopping.filter(|s| !s.escalated);
        tokio::select! {
            code = &mut exited, if waiter_alive => match code {
                Ok(Ok(code)) => break inner.leader_outcome(code),
                // The waiter failed or never started; the leader may still run,
                // so the tick's poll decides from here on.
                Ok(Err(error)) => {
                    tracing::warn!(%error, pid = inner.identity.pid, "waiting on the unit leader failed; polling instead");
                    waiter_alive = false;
                }
                Err(_) => waiter_alive = false,
            },
            _ = tick.tick() => {
                if let Ok(Some(code)) = inner.child.exit_code() {
                    break inner.leader_outcome(code);
                }
                inner.refresh_logged(timing.track_interval / 2).await;
                inner.record_console().await;
            }
            Some(policy) = requests.recv() => {
                let deadline = Instant::now() + policy.timeout;
                match &mut stopping {
                    // A repeated stop can only shorten the grace period, and after
                    // escalation every request terminates again: harmless if the
                    // first one ended everything, the only retry if it did not.
                    Some(current) => {
                        current.deadline = current.deadline.min(deadline);
                        if current.escalated {
                            inner.terminate_all().await;
                        }
                    }
                    None => {
                        let asked = inner.ask_to_stop(policy.signal).await;
                        if !asked {
                            inner.terminate_all().await;
                        }
                        stopping = Some(Stopping { deadline, escalated: !asked });
                    }
                }
            }
            () = until(asking.map(|s| s.deadline)) => {
                inner.terminate_all().await;
                if let Some(current) = &mut stopping {
                    current.escalated = true;
                }
            }
            () = time::sleep(EXIT_POLL), if asking.is_some() => {
                inner.end_waiting_shims().await;
            }
        }
    };
    // A stop that arrived as the leader exited still counts, so a task's kept
    // daemons are not spared from an explicit stop.
    while requests.try_recv().is_ok() {
        stopping.get_or_insert(Stopping {
            deadline: Instant::now(),
            escalated: false,
        });
    }
    if let Some(current) = &mut stopping {
        // Members that outlive the leader get the rest of the grace period.
        while !current.escalated && !inner.live_members().await.is_empty() {
            if Instant::now() >= current.deadline {
                inner.terminate_all().await;
                current.escalated = true;
            } else {
                inner.end_waiting_shims().await;
                time::sleep(EXIT_POLL).await;
            }
        }
    } else {
        // The last look at the console's members before deciding what a task
        // keeps; the leader is gone, but the others may still share it.
        inner.record_console().await;
    }
    let stopped = stopping.is_some();

    // A pseudo console's output only ends once it is closed, so waiting for it
    // first would always time out; pipes may end on their own.
    if !stopped && !inner.pty {
        // Leftover processes holding a pipe would block end of file forever.
        let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    }
    let sweep = inner.sweep(stopped).await;
    if inner.pty {
        // Some ConPTY versions drop what they still hold on close, so the last
        // frame is given until the output pauses (bounded by the drain timeout).
        let deadline = Instant::now() + timing.drain_timeout;
        while Instant::now() < deadline && lock(&inner.last_output).elapsed() < OUTPUT_PAUSE {
            time::sleep(OUTPUT_PAUSE / 4).await;
        }
    }
    // Closing the console ends its output and the processes still attached to
    // it, which is why it comes after the sweep.
    lock(&inner.terminal).take();
    // Collect the last output of processes the sweep just ended.
    let _ = time::timeout(timing.drain_timeout, output_done.wait_for(|done| *done)).await;
    inner.finish(ExitReport {
        outcome,
        stop: stopping.map(|s| StopOutcome {
            escalated: s.escalated,
        }),
        swept: sweep.swept,
        escaped: sweep.escaped,
        left_running: sweep.left_running,
    });
}

/// Stop progress owned by the supervisor.
#[derive(Debug, Clone, Copy)]
struct Stopping {
    /// When asking ends and terminating starts.
    deadline: Instant,
    /// The processes were terminated, not asked to stop.
    escalated: bool,
}

/// Sleeps until `deadline`, or forever when there is none.
async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Publishes a status-less report if the supervisor ends early (panic or runtime
/// shutdown), so waiters are not left hanging, and marks the unit so a later stop
/// terminates its processes directly.
struct PublishOnDrop(Arc<Inner>);

impl Drop for PublishOnDrop {
    fn drop(&mut self) {
        if self.0.exit.borrow().is_none() {
            self.0.crashed.store(true, Ordering::SeqCst);
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

impl Inner {
    /// Records that the leader exited with `code`.
    fn leader_outcome(&self, code: u32) -> ExitOutcome {
        self.leader_exited.store(true, Ordering::Release);
        exit_outcome(code)
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

    /// Asks the unit to stop the way `signal` maps on Windows: Ctrl-C as a key
    /// for a pseudo console, Ctrl-C or Ctrl-Break through the helper otherwise,
    /// plus `WM_CLOSE` to the members' visible windows. Returns whether anything
    /// was asked; `false` (SIGKILL, or no way to reach the unit) means terminate.
    async fn ask_to_stop(self: &Arc<Self>, signal: StopSignal) -> bool {
        let event = match signal.windows_stop() {
            WindowsStop::Terminate => return false,
            WindowsStop::CtrlC => Request::CtrlC,
            WindowsStop::CtrlBreak => Request::CtrlBreak,
        };
        let live = self.live_members().await;
        let mut asked = false;
        if event == Request::CtrlC && self.pty {
            let queue = lock(&self.terminal)
                .as_ref()
                .map(|terminal| terminal.input.clone());
            if let Some(queue) = queue {
                let (reply, _written) = oneshot::channel();
                asked = queue
                    .send((Bytes::from_static(CTRL_C), reply))
                    .await
                    .is_ok();
            }
        } else if let Some(helper) = self.helper.clone() {
            // The leader's console is the unit's; after it exited, any member
            // still running shares it unless it made its own.
            let leader_alive = matches!(self.child.exit_code(), Ok(None));
            let target = if leader_alive {
                Some(self.identity.pid)
            } else {
                live.first().map(|p| p.pid)
            };
            if let Some(pid) = target {
                match blocking(move || helper.send(pid, event)).await {
                    Ok(Ok(())) => asked = true,
                    Ok(Err(error)) => {
                        tracing::debug!(%error, pid, "console control event not sent")
                    }
                    Err(error) => tracing::debug!(%error, pid, "console control event not sent"),
                }
            }
        }
        let pids: Vec<u32> = live.iter().map(|p| p.pid).collect();
        let closed = blocking(move || devlanes_sys::windows::window::close_windows(&pids))
            .await
            .unwrap_or(0);
        asked || closed > 0
    }

    /// Ends `cmd.exe` processes left waiting at "Terminate batch job (Y/N)?" once
    /// everything they ran has exited: a `.cmd` shim (`npm.cmd`) asks that after
    /// Ctrl-C or Ctrl-Break and would otherwise hold the stop until its timeout.
    /// This is expected cleanup, so it does not count as escalation.
    async fn end_waiting_shims(self: &Arc<Self>) {
        let live = self.live_members().await;
        let shims_only =
            !live.is_empty() && live.iter().all(|p| p.name.eq_ignore_ascii_case("cmd.exe"));
        if shims_only {
            let _ = blocking(move || {
                for shim in &live {
                    let _ = terminate_verified(&shim.identity(), TERMINATED);
                }
            })
            .await;
        }
    }

    /// Records which processes are attached to the unit's console, for a pipe
    /// task (`ExitSweep::KeepDaemonized`) with a helper: after the task exits,
    /// those are its leftovers, and members with their own console or none are
    /// daemons it started. Asks through the leader, or after it exited through a
    /// recorded member that still runs.
    async fn record_console(self: &Arc<Self>) {
        if self.pty || self.timing.exit_sweep != ExitSweep::KeepDaemonized {
            return;
        }
        let Some(helper) = self.helper.clone() else {
            return;
        };
        let leader_alive = matches!(self.child.exit_code(), Ok(None));
        let tracked: Vec<ProcessInfo> = lock(&self.tracker).live().cloned().collect();
        let anchor = if leader_alive {
            Some(self.identity.pid)
        } else {
            let known = lock(&self.on_console).clone().unwrap_or_default();
            tracked
                .iter()
                .find(|p| known.contains(&(p.pid, p.start_time)))
                .map(|p| p.pid)
        };
        let Some(anchor) = anchor else {
            return;
        };
        let Ok(Ok(pids)) = blocking(move || helper.list(anchor)).await else {
            return;
        };
        let mut known = lock(&self.on_console);
        let set = known.get_or_insert_with(BTreeSet::new);
        set.extend(
            tracked
                .iter()
                .filter(|p| pids.contains(&p.pid))
                .map(|p| (p.pid, p.start_time)),
        );
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
            if let Err(error) = terminate_verified(&process.identity(), TERMINATED) {
                tracing::debug!(%error, pid = process.pid, "terminate failed");
            }
        }
    }

    /// Ends live tracked processes until none remain or the sweep times out.
    ///
    /// After a natural exit in [`ExitSweep::KeepDaemonized`] mode (decision Q9),
    /// processes still attached to the unit's console are leftovers and end, while
    /// those that made their own console, run without one, or broke away from the
    /// job are daemons the task started and are reported as left running. When the
    /// console's members were never listed (no helper), the job stands in for it:
    /// members end, processes outside it are kept.
    async fn sweep(self: &Arc<Self>, explicit_stop: bool) -> Sweep {
        // Processes kept outside the job are still attached to a pseudo console,
        // which ends them when it closes, so `pty: true` tasks keep nothing.
        let keep_daemonized =
            !explicit_stop && self.timing.exit_sweep == ExitSweep::KeepDaemonized && !self.pty;
        let on_console = lock(&self.on_console).clone().filter(|_| keep_daemonized);
        let deadline = Instant::now() + self.timing.sweep_timeout;
        let mut ended = BTreeMap::new();
        // Unless some job members are kept, the job holds only processes that must
        // end. Ending it first also reaches members started after the last
        // snapshot, which the loop below cannot see. Live members seen before are
        // reported as swept.
        if on_console.is_none() {
            let live = self.live_members().await;
            let inner = Arc::clone(self);
            let in_job = blocking(move || {
                let in_job: BTreeSet<u32> = inner
                    .job
                    .members()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                if let Err(error) = inner.job.terminate(TERMINATED) {
                    tracing::debug!(%error, "terminating the unit's job failed");
                }
                in_job
            })
            .await
            .unwrap_or_default();
            for process in live.into_iter().filter(|p| in_job.contains(&p.pid)) {
                ended.insert(process.pid, process);
            }
        }
        loop {
            let live = self.live_members().await;
            let in_job: BTreeSet<u32> =
                self.job.members().unwrap_or_default().into_iter().collect();
            let (targets, kept): (Vec<_>, Vec<_>) =
                live.into_iter()
                    .partition(|p| match (&on_console, keep_daemonized) {
                        (_, false) => true,
                        (Some(console), true) => console.contains(&(p.pid, p.start_time)),
                        (None, true) => in_job.contains(&p.pid),
                    });
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
            let precise = on_console.is_some();
            // Survivors show up in the next round and end as escaped at the deadline.
            let ended_now = blocking(move || {
                if precise {
                    // Kept members share the job, so only the targets end.
                    for process in &targets {
                        let _ = terminate_verified(&process.identity(), TERMINATED);
                    }
                } else if keep_daemonized {
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
    /// receiver even if a reader thread stays blocked on a held pipe. The first
    /// receiver is kept: a subscriber that comes late (the daemon opens the log
    /// first) still reads everything buffered before it sees the end.
    fn finish(&self, report: ExitReport) {
        lock(&self.sender).take();
        lock(&self.terminal).take();
        self.exit.send_replace(Some(report));
    }
}

impl WindowsUnit {
    /// Leader identity recorded right after spawn; persist it for reconciliation.
    pub fn identity(&self) -> ProcessIdentity {
        self.inner.identity
    }

    /// Ends every process in the unit's job at once, without waiting or a report.
    /// For cleanup where nothing can be awaited, such as a test's `Drop`; processes
    /// that left the job are not reached.
    pub fn kill_job(&self) {
        let _ = self.inner.job.terminate(TERMINATED);
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
        // A closed channel means the supervisor ended; only a crash leaves work undone.
        if self.inner.stops.send(policy).is_err() && self.inner.crashed.load(Ordering::SeqCst) {
            return self.stop_without_supervisor().await;
        }
        // The grace period, then the supervisor's own worst case: the sweep, both
        // output drains, and slack for scans on a loaded machine.
        let bound = policy.timeout
            + self.inner.timing.sweep_timeout
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

    /// Ends the tracked processes directly after the supervisor died, then
    /// replaces its placeholder with a report of this stop.
    async fn stop_without_supervisor(&self) -> Result<ExitReport, Error> {
        tracing::warn!(
            pid = self.inner.identity.pid,
            "unit supervisor is gone; terminating directly"
        );
        let deadline = Instant::now() + self.inner.timing.sweep_timeout;
        let survivors = loop {
            self.inner.terminate_all().await;
            let live = self.inner.live_members().await;
            if live.is_empty() || Instant::now() >= deadline {
                break live;
            }
            time::sleep(SWEEP_POLL).await;
        };
        // The supervisor would have closed the console after its sweep.
        lock(&self.inner.terminal).take();
        let outcome = match self.inner.child.exit_code() {
            Ok(Some(code)) => exit_outcome(code),
            _ => UNKNOWN,
        };
        self.inner.exit.send_replace(Some(ExitReport {
            outcome,
            stop: Some(StopOutcome { escalated: true }),
            swept: Vec::new(),
            escaped: survivors,
            left_running: Vec::new(),
        }));
        lock(&self.inner.sender).take();
        self.wait_report().await
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
    /// Writes all bytes to the pseudo console's input, in call order. Waits while
    /// the console does not read, so a program that never reads its input can
    /// stall this call; a caller that gives up still gets its bytes written, in
    /// their place. Pipe units have no terminal.
    async fn input(&self, bytes: Bytes) -> Result<(), Error> {
        if !self.inner.pty {
            return Err(Error::Unsupported("pipe units have no terminal".into()));
        }
        let queue = match lock(&self.inner.terminal).as_ref() {
            Some(terminal) => terminal.input.clone(),
            None => return Err(Error::Exited),
        };
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

    /// Resizes the pseudo console; the attached programs see the new size.
    async fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        if size.columns == 0 || size.rows == 0 {
            return Err(Error::InvalidInput("terminal size must be non-zero".into()));
        }
        if !self.inner.pty {
            return Err(Error::Unsupported("pipe units have no terminal".into()));
        }
        match lock(&self.inner.terminal).as_ref() {
            Some(terminal) => Ok(terminal.console.resize(size.columns, size.rows)?),
            None => Err(Error::Exited),
        }
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
            // Like the leader's, each identity's group is its own PID.
            .map(ProcessInfo::identity)
            .collect();
        // Before the first snapshot the tracker is empty; the leader is known anyway.
        if subjects.is_empty() && !self.inner.leader_exited.load(Ordering::Acquire) {
            subjects.push(self.inner.identity);
        }
        Ok(subjects)
    }
}
