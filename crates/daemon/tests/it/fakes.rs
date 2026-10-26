//! In-memory integrations: a scripted runner, a process table and an infra provider.
//!
//! Fake units understand a tiny script language in the unit's command
//! (`;`-separated): `echo <text>`, `printenv <KEY>`, `serve` (binds `$PORT` and stays
//! up), `tcp-echo` (echoes every connection on `$PORT`), `http <body>` (answers
//! every request on `$PORT` with `<body>`), `sleep` (stays up), `exit <code>`,
//! `fail` (prints `boom`, exits 1), `nap <ms>` (waits, then goes on),
//! `hold` (waits until the test calls [`FakeRunner::release`]),
//! `deaf` (stops reading input: writes block
//! until the unit stops), `daemonize <name>` and `escape <name>` (report a
//! process left running or escaped once the unit ends), `stubborn` (ignores
//! its first stop request; with [`FakeRunner::stubborn_identity`] set it also
//! carries that real identity, so an overdue stop finds it alive), `lag`
//! (the spawn returns 500 ms after the journal recorded it, like a slow
//! `fork`/`exec`, which widens the window before the daemon registers the unit),
//! `render` (the unit's output renders its terminal, as a Windows pseudo
//! console's does: see `Unit::renders_screen`).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    Error, ExitOutcome, LaunchSpec, OutputChunk, OutputStream, ProcessIdentity, StopPolicy,
    StopSignal, TerminalSize, Unit,
};
use devlanes_daemon::integrations::{
    ContainerLogChunk, ContainerLogOptions, ExitDetails, InfraContainer, InfraError, InfraEvent,
    InfraPortNeed, InfraProvider, InfraSpec, InfraUp, ManagedUnit, PortScope, ProcessTable,
    UnitRunner,
};
use devlanes_proto::{LeftProcess, PhaseContainer};
use futures_util::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

/// The Docker-style timestamp of the fake infra's log line `n`: one second apart
/// from 2026-01-01T00:00:00Z.
pub fn fake_timestamp(n: usize) -> String {
    format!(
        "2026-01-01T{:02}:{:02}:{:02}.000000000Z",
        n / 3600 % 24,
        n / 60 % 60,
        n % 60
    )
}

/// The Linux number a fake unit reports as its exit signal after a stop.
fn signal_number(signal: StopSignal) -> i32 {
    match signal {
        StopSignal::Hangup => 1,
        StopSignal::Interrupt => 2,
        StopSignal::Quit => 3,
        StopSignal::Kill => 9,
        StopSignal::User1 => 10,
        StopSignal::User2 => 12,
        StopSignal::Terminate => 15,
    }
}

/// Identities handed out by the fake runner; far above real PIDs.
static NEXT_PID: AtomicU32 = AtomicU32::new(4_000_000);

/// Contributor that records `<kind> <name>` for every unit planned, so a test can
/// see which units a resolution planned.
#[derive(Default, Clone)]
pub struct PlanJournal {
    pub planned: Journal,
}

#[async_trait]
impl devlanes_core::contrib::Contributor for PlanJournal {
    fn id(&self) -> &'static str {
        "plan-journal"
    }

    /// Records the unit; the draft is left as it is.
    async fn apply(
        &self,
        _: &mut devlanes_core::contrib::LaunchDraft,
        cx: &devlanes_core::contrib::ContribCx<'_>,
    ) -> Result<(), Error> {
        if let Ok(mut planned) = self.planned.0.lock() {
            planned.push(format!("{:?} {}", cx.kind, cx.name));
        }
        Ok(())
    }
}

/// Records every spawned script in order.
#[derive(Default, Clone)]
pub struct Journal(pub Arc<Mutex<Vec<String>>>);

impl Journal {
    /// Snapshot of the recorded scripts.
    pub fn entries(&self) -> Vec<String> {
        self.0.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

/// Runner whose units follow the script in their command.
#[derive(Default, Clone)]
pub struct FakeRunner {
    pub journal: Journal,
    /// `<unit pid> <columns>x<rows>` for every resize, in order.
    pub resizes: Journal,
    /// Identity given to `stubborn` units, e.g. the test process's own.
    pub stubborn_identity: Arc<Mutex<Option<ProcessIdentity>>>,
    /// Opened by [`FakeRunner::release`]; `hold` waits for it.
    pub gate: Gate,
}

/// What `hold` waits on: a test opens it once it has seen what it waits for, so
/// a unit stays busy exactly as long as the test needs instead of a fixed time.
#[derive(Clone)]
pub struct Gate(Arc<watch::Sender<bool>>);

impl Default for Gate {
    /// Closed.
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl FakeRunner {
    /// Lets every `hold` (now and later) go on.
    pub fn release(&self) {
        self.gate.0.send_replace(true);
    }
}

/// Extracts the script from `/bin/sh -c <script>` or argv.
fn script_of(spec: &LaunchSpec) -> String {
    if spec.program == "/bin/sh" && spec.args.first().is_some_and(|a| a == "-c") {
        return spec.args.get(1).cloned().unwrap_or_default();
    }
    std::iter::once(spec.program.clone())
        .chain(spec.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl UnitRunner for FakeRunner {
    /// Starts a scripted unit.
    async fn spawn(&self, spec: LaunchSpec) -> Result<Arc<dyn ManagedUnit>, Error> {
        let script = script_of(&spec);
        if let Ok(mut journal) = self.journal.0.lock() {
            journal.push(script.clone());
        }
        if script.split(';').any(|command| command.trim() == "lag") {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        let (sender, first) = broadcast::channel(256);
        let stubborn = script
            .split(';')
            .any(|command| command.trim() == "stubborn");
        let renders = script.split(';').any(|command| command.trim() == "render");
        let real = if stubborn {
            self.stubborn_identity
                .lock()
                .ok()
                .and_then(|identity| *identity)
        } else {
            None
        };
        let unit = Arc::new(FakeUnit {
            identity: real.unwrap_or(ProcessIdentity {
                pid,
                sid: pid,
                start_time: 1,
            }),
            stubborn: AtomicBool::new(stubborn),
            renders,
            sender: Mutex::new(Some(sender)),
            first: Mutex::new(Some(first)),
            exit: watch::channel(None).0,
            stop: watch::channel(false).0,
            deaf: AtomicBool::new(false),
            resizes: self.resizes.clone(),
            leftovers: Mutex::default(),
            gate: self.gate.0.subscribe(),
        });
        let runner = unit.clone();
        tokio::spawn(async move { runner.run(script, spec.env).await });
        Ok(unit)
    }
}

/// A scripted process.
pub struct FakeUnit {
    identity: ProcessIdentity,
    /// Set by `stubborn`: the next stop request is ignored.
    stubborn: AtomicBool,
    /// Set by `render`.
    renders: bool,
    sender: Mutex<Option<broadcast::Sender<OutputChunk>>>,
    first: Mutex<Option<broadcast::Receiver<OutputChunk>>>,
    exit: watch::Sender<Option<ExitOutcome>>,
    stop: watch::Sender<bool>,
    /// Set by `deaf`: input never completes, like a process that stopped reading.
    deaf: AtomicBool,
    resizes: Journal,
    /// `(left_running, escaped)` recorded by `daemonize` and `escape`.
    leftovers: Mutex<(Vec<LeftProcess>, Vec<LeftProcess>)>,
    /// The runner's [`Gate`], for `hold`.
    gate: watch::Receiver<bool>,
}

impl FakeUnit {
    /// Writes one output chunk.
    fn print(&self, text: &str) {
        if let Ok(sender) = self.sender.lock()
            && let Some(sender) = sender.as_ref()
        {
            let _ = sender.send(OutputChunk {
                stream: OutputStream::Terminal,
                bytes: Bytes::from(format!("{text}\r\n")),
            });
        }
    }

    /// Ends the unit: closes output (so the host's pump finishes) and publishes the exit.
    fn end(&self, outcome: ExitOutcome) {
        if self.exit.borrow().is_some() {
            return;
        }
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        // send_replace: `send` drops the value while nobody waits yet.
        self.exit.send_replace(Some(outcome));
    }

    /// Interprets the script.
    async fn run(self: Arc<Self>, script: String, env: BTreeMap<String, String>) {
        // Let the host subscribe before the first byte, like a real process start.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut stopped = self.stop.subscribe();
        for command in script.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            let (word, rest) = command.split_once(' ').unwrap_or((command, ""));
            match word {
                "echo" => self.print(rest),
                "nap" => {
                    let ms = rest.trim().parse().unwrap_or(0);
                    tokio::select! {
                        _ = stopped.wait_for(|s| *s) => return,
                        () = tokio::time::sleep(Duration::from_millis(ms)) => {}
                    }
                }
                "hold" => {
                    let mut gate = self.gate.clone();
                    tokio::select! {
                        _ = stopped.wait_for(|s| *s) => return,
                        _ = gate.wait_for(|open| *open) => {}
                    }
                }
                "deaf" => self.deaf.store(true, Ordering::SeqCst),
                // Read at spawn.
                "render" => {}
                "printenv" => self.print(env.get(rest).map_or("", String::as_str)),
                "daemonize" | "escape" => {
                    let process = LeftProcess {
                        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
                        command: rest.to_owned(),
                    };
                    if let Ok(mut leftovers) = self.leftovers.lock() {
                        if word == "daemonize" {
                            leftovers.0.push(process);
                        } else {
                            leftovers.1.push(process);
                        }
                    }
                }
                "exit" => {
                    let code = rest.trim().parse().unwrap_or(0);
                    self.end(ExitOutcome {
                        code: Some(code),
                        signal: None,
                    });
                    return;
                }
                "fail" => {
                    self.print("boom");
                    self.end(ExitOutcome {
                        code: Some(1),
                        signal: None,
                    });
                    return;
                }
                "serve" | "sleep" => {
                    let listener = match env.get("PORT").and_then(|p| p.parse::<u16>().ok()) {
                        Some(port) if word == "serve" => {
                            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                                .await
                                .ok();
                            self.print(&format!("listening on {port}"));
                            listener
                        }
                        _ => None,
                    };
                    let accept = async {
                        match &listener {
                            Some(listener) => loop {
                                let _ = listener.accept().await;
                            },
                            None => std::future::pending::<()>().await,
                        }
                    };
                    tokio::select! {
                        _ = stopped.wait_for(|s| *s) => {}
                        () = accept => {}
                    }
                    drop(listener);
                    return;
                }
                "tcp-echo" | "http" => {
                    let Some(port) = env.get("PORT").and_then(|p| p.parse::<u16>().ok()) else {
                        self.print("no PORT");
                        continue;
                    };
                    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await
                    else {
                        self.fail_bind(port);
                        return;
                    };
                    self.print(&format!("listening on {port}"));
                    let body = rest.to_owned();
                    let http = word == "http";
                    let accept = async {
                        loop {
                            let Ok((stream, _)) = listener.accept().await else {
                                continue;
                            };
                            let body = body.clone();
                            tokio::spawn(async move {
                                if http {
                                    answer_http(stream, &body).await;
                                } else {
                                    let (mut read, mut write) = stream.into_split();
                                    let _ = tokio::io::copy(&mut read, &mut write).await;
                                }
                            });
                        }
                    };
                    tokio::select! {
                        _ = stopped.wait_for(|s| *s) => {}
                        () = accept => {}
                    }
                    return;
                }
                other => self.print(&format!("unknown {other}")),
            }
        }
        self.end(ExitOutcome {
            code: Some(0),
            signal: None,
        });
    }
}

impl FakeUnit {
    /// Exits like a server whose port is taken.
    fn fail_bind(&self, port: u16) {
        self.print(&format!("port {port} in use"));
        self.end(ExitOutcome {
            code: Some(98),
            signal: None,
        });
    }
}

/// Answers one HTTP/1.1 request with `body` and closes.
async fn answer_http(mut stream: tokio::net::TcpStream, body: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buffer[..n]),
        }
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[async_trait]
impl Unit for FakeUnit {
    /// Echoes input back as output, like a terminal in cooked mode.
    async fn input(&self, bytes: Bytes) -> Result<(), Error> {
        if self.exit.borrow().is_some() {
            return Err(Error::Exited);
        }
        if self.deaf.load(Ordering::SeqCst) {
            // A full PTY buffer: the write returns only once the unit goes away.
            let _ = self.stop.subscribe().wait_for(|stopped| *stopped).await;
            return Err(Error::Exited);
        }
        self.print(&String::from_utf8_lossy(&bytes));
        Ok(())
    }
    /// Accepts any size and records it.
    async fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        if let Ok(mut resizes) = self.resizes.0.lock() {
            resizes.push(format!(
                "{} {}x{}",
                self.identity.pid, size.columns, size.rows
            ));
        }
        Ok(())
    }
    /// Ends the unit with the requested signal (`stubborn`: from the second request).
    async fn stop(&self, policy: StopPolicy) -> Result<(), Error> {
        if self.stubborn.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        self.stop.send_replace(true);
        self.end(ExitOutcome {
            code: None,
            signal: Some(signal_number(policy.signal)),
        });
        Ok(())
    }
    /// Waits for the exit.
    async fn wait(&self) -> Result<ExitOutcome, Error> {
        let mut exit = self.exit.subscribe();
        let outcome = exit
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::Exited)?;
        outcome.ok_or(Error::Exited)
    }
    /// The first call sees everything since spawn.
    fn output(&self) -> broadcast::Receiver<OutputChunk> {
        if let Ok(mut first) = self.first.lock()
            && let Some(first) = first.take()
        {
            return first;
        }
        match self
            .sender
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(broadcast::Sender::subscribe))
        {
            Some(receiver) => receiver,
            None => broadcast::channel(1).1,
        }
    }
    /// The leader only.
    async fn subjects(&self) -> Result<Vec<ProcessIdentity>, Error> {
        Ok(vec![self.identity])
    }
    /// Set by `render`.
    fn renders_screen(&self) -> bool {
        self.renders
    }
}

#[async_trait]
impl ManagedUnit for FakeUnit {
    /// Synthetic identity.
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.identity)
    }
    /// Nothing is inspected.
    async fn listening_ports(&self) -> Vec<u16> {
        Vec::new()
    }
    /// The outcome plus what `daemonize` and `escape` recorded.
    fn exit_details(&self) -> Option<ExitDetails> {
        let outcome = (*self.exit.borrow())?;
        let (left_running, escaped) = self.leftovers.lock().ok()?.clone();
        Some(ExitDetails {
            outcome,
            left_running,
            escaped,
        })
    }
}

/// Process table with a fixed set of "alive" identities.
#[derive(Default, Clone)]
pub struct FakeProcesses {
    pub alive: Arc<Mutex<BTreeSet<u32>>>,
    pub killed: Arc<Mutex<Vec<u32>>>,
}

#[async_trait]
impl ProcessTable for FakeProcesses {
    /// Alive when the PID was registered.
    async fn alive(&self, identity: ProcessIdentity) -> bool {
        self.alive.lock().is_ok_and(|a| a.contains(&identity.pid))
    }
    /// Records the kill.
    async fn terminate(&self, identity: ProcessIdentity, _: Duration) {
        if let Ok(mut alive) = self.alive.lock() {
            alive.remove(&identity.pid);
        }
        if let Ok(mut killed) = self.killed.lock() {
            killed.push(identity.pid);
        }
    }
}

/// A line pushed to following log streams with its index in the log; `None`
/// ends them.
type LiveLine = Option<(usize, String)>;

/// Infra provider recording calls; one per-lane port `db/5432`.
#[derive(Default, Clone)]
pub struct FakeInfra {
    pub calls: Journal,
    /// `StartedAt` reported for every container.
    pub started_at: Arc<Mutex<Option<String>>>,
    /// When set, `up` fails with this error.
    pub fail_up: Arc<Mutex<Option<InfraError>>>,
    /// Lines every container's log holds. Line `n` is stamped [`fake_timestamp`]`(n)`.
    pub logs: Arc<Mutex<Vec<String>>>,
    /// `<docker> tail=<tail> follow=<follow>` for every `container_logs` call.
    pub log_requests: Journal,
    /// Replicas of the lane's `db` service; 0 and 1 both mean one container.
    pub db_replicas: Arc<Mutex<usize>>,
    /// New lines for following streams ([`FakeInfra::push_log`]); `None` ends
    /// them, as Docker does when the container stops.
    live: Arc<std::sync::OnceLock<broadcast::Sender<LiveLine>>>,
    /// Snapshots `progress` hands out in turn, the last one for good. While
    /// any is left, `up` waits until the last was handed out.
    pub progress: Arc<Mutex<std::collections::VecDeque<Vec<PhaseContainer>>>>,
    /// Fired when `progress` handed out the last snapshot.
    pub progress_read: Arc<tokio::sync::Notify>,
    /// Container events, sent with [`FakeInfra::emit`].
    events: Arc<std::sync::OnceLock<broadcast::Sender<InfraEvent>>>,
}

impl FakeInfra {
    /// Sends a container event to the engine.
    pub fn emit(&self, event: InfraEvent) {
        let _ = self.sender().send(event);
    }

    /// The event channel, made on first use.
    fn sender(&self) -> &broadcast::Sender<InfraEvent> {
        self.events.get_or_init(|| broadcast::channel(16).0)
    }

    /// The live log channel, made on first use.
    fn live(&self) -> &broadcast::Sender<LiveLine> {
        self.live.get_or_init(|| broadcast::channel(64).0)
    }

    /// Appends a line to every container's log and sends it to following streams.
    pub fn push_log(&self, line: &str) {
        let Ok(mut logs) = self.logs.lock() else {
            return;
        };
        logs.push(line.to_owned());
        let _ = self.live().send(Some((logs.len() - 1, line.to_owned())));
    }

    /// Ends every following stream, like a container that stopped.
    pub fn end_logs(&self) {
        let _ = self.live().send(None);
    }
    /// Records a call.
    fn record(&self, call: String) {
        if let Ok(mut calls) = self.calls.0.lock() {
            calls.push(call);
        }
    }

    /// Namespace from the allocated port.
    fn namespace(spec: &InfraSpec) -> Value {
        let port = spec.ports.get("infra.db/5432").copied();
        json!({"db": {"port": port, "ports": {"5432": port}}, "cache": {"port": 6379, "ports": {"6379": 6379}}})
    }

    /// The lane's `db` container (one per replica) and a shared `cache`, named
    /// like compose would.
    fn containers(&self, spec: &InfraSpec) -> Vec<InfraContainer> {
        let replicas = self.db_replicas.lock().map_or(1, |r| (*r).max(1));
        let mut containers: Vec<InfraContainer> = (1..=replicas)
            .map(|replica| InfraContainer {
                name: "db".into(),
                docker: format!("{}-lane{}-db-{replica}", spec.project, spec.lane),
                shared: false,
                state: Some("healthy".into()),
            })
            .collect();
        containers.push(InfraContainer {
            name: "cache".into(),
            docker: format!("{}-cache-1", spec.project),
            shared: true,
            state: None,
        });
        containers
    }
}

#[async_trait]
impl InfraProvider for FakeInfra {
    async fn port_needs(&self, _: &InfraSpec) -> Result<Vec<InfraPortNeed>, InfraError> {
        Ok(vec![InfraPortNeed {
            scope: PortScope::Lane,
            unit: "infra.db".into(),
            name: "5432".into(),
        }])
    }
    async fn up(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        self.record(format!("up {}", spec.lane));
        if let Some(error) = self.fail_up.lock().ok().and_then(|e| e.clone()) {
            return Err(error);
        }
        let scripted = self.progress.lock().is_ok_and(|script| !script.is_empty());
        if scripted {
            let read = self.progress_read.notified();
            let _ = tokio::time::timeout(Duration::from_secs(20), read).await;
        }
        Ok(InfraUp {
            namespace: Self::namespace(spec),
            setup_env: BTreeMap::from([("COMPOSE_PROJECT_NAME".into(), "fake".into())]),
            report: json!({"services": ["db"]}),
            containers: self.containers(spec),
        })
    }
    async fn namespace(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        Ok(InfraUp {
            namespace: Self::namespace(spec),
            // As Compose's provider: the same env `up` gives, nothing started.
            setup_env: BTreeMap::from([("COMPOSE_PROJECT_NAME".into(), "fake".into())]),
            containers: self.containers(spec),
            ..InfraUp::default()
        })
    }
    async fn status(&self, _: &InfraSpec) -> Result<Value, InfraError> {
        Ok(json!({"db": "healthy"}))
    }
    fn events(&self) -> Option<broadcast::Receiver<InfraEvent>> {
        Some(self.sender().subscribe())
    }
    async fn down(&self, _: &InfraSpec) -> Result<Value, InfraError> {
        self.record("down".into());
        Ok(json!({"stopped": ["db"]}))
    }
    async fn remove_lane(&self, _: &str, lane: u32) -> Result<Value, InfraError> {
        self.record(format!("remove {lane}"));
        Ok(json!({"containers": []}))
    }
    /// Every container started at the scripted time; none run without one.
    async fn containers_started_at(
        &self,
        _: &InfraSpec,
        containers: &[String],
    ) -> Result<BTreeMap<String, String>, InfraError> {
        let started = self.started_at.lock().ok().and_then(|s| s.clone());
        Ok(started
            .map(|at| {
                containers
                    .iter()
                    .map(|container| (container.clone(), at.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }
    /// The stored lines (the last `tail`), then live ones while following.
    async fn container_logs(
        &self,
        docker: &str,
        options: ContainerLogOptions,
    ) -> Result<BoxStream<'static, Result<ContainerLogChunk, InfraError>>, InfraError> {
        if let Ok(mut requests) = self.log_requests.0.lock() {
            requests.push(format!(
                "{docker} tail={:?} follow={}",
                options.tail, options.follow
            ));
        }
        let format = move |n: usize, line: &str| {
            let text = if options.timestamps {
                format!("{} {line}\n", fake_timestamp(n))
            } else {
                format!("{line}\n")
            };
            Ok(ContainerLogChunk {
                stderr: false,
                bytes: Bytes::from(text),
            })
        };
        // The receiver is made under the lock `push_log` takes, so no line is
        // missed or sent twice between the stored ones and the live ones.
        let (stored, receiver) = {
            let logs = self
                .logs
                .lock()
                .map_err(|_| InfraError::new("poisoned", "the fake's log lock is poisoned"))?;
            (logs.clone(), self.live().subscribe())
        };
        let start = options
            .tail
            .map_or(0, |tail| stored.len().saturating_sub(tail));
        let chunks: Vec<_> = stored
            .iter()
            .enumerate()
            .skip(start)
            .map(|(n, line)| format(n, line))
            .collect();
        let stored = futures_util::stream::iter(chunks);
        if !options.follow {
            return Ok(stored.boxed());
        }
        let live = futures_util::stream::unfold(receiver, move |mut receiver| async move {
            loop {
                match receiver.recv().await {
                    Ok(Some((n, line))) => return Some((format(n, &line), receiver)),
                    Ok(None) | Err(broadcast::error::RecvError::Closed) => return None,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        });
        Ok(stored.chain(live).boxed())
    }
    async fn adopt(&self, _: &[String]) -> Result<Value, InfraError> {
        Ok(Value::Null)
    }
    async fn doctor(&self) -> Value {
        json!({"docker": {"reachable": true}})
    }
    async fn progress(&self, _: &InfraSpec) -> Vec<PhaseContainer> {
        let Ok(mut script) = self.progress.lock() else {
            return Vec::new();
        };
        if script.len() > 1 {
            return script.pop_front().unwrap_or_default();
        }
        // `notify_one` keeps a permit, so an `up` not waiting yet still sees it.
        self.progress_read.notify_one();
        script.front().cloned().unwrap_or_default()
    }
}
