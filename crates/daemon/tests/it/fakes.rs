//! In-memory integrations: a scripted runner, a process table and an infra provider.
//!
//! Fake units understand a tiny script language in the unit's command
//! (`;`-separated): `echo <text>`, `printenv <KEY>`, `serve` (binds `$PORT` and stays
//! up), `tcp-echo` (echoes every connection on `$PORT`), `http <body>` (answers
//! every request on `$PORT` with `<body>`), `sleep` (stays up), `exit <code>`,
//! `fail` (prints `boom`, exits 1).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_core::{
    Error, ExitOutcome, LaunchSpec, OutputChunk, OutputStream, ProcessIdentity, StopPolicy,
    TerminalSize, Unit,
};
use devlanes_daemon::integrations::{
    InfraError, InfraPortNeed, InfraProvider, InfraSpec, InfraUp, ManagedUnit, PortScope,
    ProcessTable, UnitRunner,
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

/// Identities handed out by the fake runner; far above real PIDs.
static NEXT_PID: AtomicU32 = AtomicU32::new(4_000_000);

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
        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        let (sender, first) = broadcast::channel(256);
        let unit = Arc::new(FakeUnit {
            identity: ProcessIdentity {
                pid,
                sid: pid,
                start_time: 1,
            },
            sender: Mutex::new(Some(sender)),
            first: Mutex::new(Some(first)),
            exit: watch::channel(None).0,
            stop: watch::channel(false).0,
        });
        let runner = unit.clone();
        tokio::spawn(async move { runner.run(script, spec.env).await });
        Ok(unit)
    }
}

/// A scripted process.
pub struct FakeUnit {
    identity: ProcessIdentity,
    sender: Mutex<Option<broadcast::Sender<OutputChunk>>>,
    first: Mutex<Option<broadcast::Receiver<OutputChunk>>>,
    exit: watch::Sender<Option<ExitOutcome>>,
    stop: watch::Sender<bool>,
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
                "printenv" => self.print(env.get(rest).map_or("", String::as_str)),
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
        self.print(&String::from_utf8_lossy(&bytes));
        Ok(())
    }
    /// Accepts any size.
    async fn resize(&self, _: TerminalSize) -> Result<(), Error> {
        Ok(())
    }
    /// Ends the unit with the requested signal.
    async fn stop(&self, policy: StopPolicy) -> Result<(), Error> {
        self.stop.send_replace(true);
        self.end(ExitOutcome {
            code: None,
            signal: Some(policy.signal),
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

/// Infra provider recording calls; one per-lane port `db/5432`.
#[derive(Default, Clone)]
pub struct FakeInfra {
    pub calls: Journal,
    /// `StartedAt` reported for every container.
    pub started_at: Arc<Mutex<Option<String>>>,
    /// When set, `up` fails with this error.
    pub fail_up: Arc<Mutex<Option<InfraError>>>,
}

impl FakeInfra {
    /// Records a call.
    fn record(&self, call: String) {
        if let Ok(mut calls) = self.calls.0.lock() {
            calls.push(call);
        }
    }

    /// Namespace from the allocated port.
    fn namespace(spec: &InfraSpec) -> Value {
        let port = spec.ports.get("infra.db/5432").copied();
        json!({"db": {"port": port, "ports": {"5432": port}}})
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
        Ok(InfraUp {
            namespace: Self::namespace(spec),
            setup_env: BTreeMap::from([("COMPOSE_PROJECT_NAME".into(), "fake".into())]),
            report: json!({"services": ["db"]}),
        })
    }
    async fn namespace(&self, spec: &InfraSpec) -> Result<InfraUp, InfraError> {
        Ok(InfraUp {
            namespace: Self::namespace(spec),
            ..InfraUp::default()
        })
    }
    async fn status(&self, _: &InfraSpec) -> Result<Value, InfraError> {
        Ok(json!({"db": "healthy"}))
    }
    async fn down(&self, _: &InfraSpec) -> Result<Value, InfraError> {
        self.record("down".into());
        Ok(json!({"stopped": ["db"]}))
    }
    async fn remove_lane(&self, _: &str, lane: u32) -> Result<Value, InfraError> {
        self.record(format!("remove {lane}"));
        Ok(json!({"containers": []}))
    }
    async fn container_started_at(
        &self,
        _: &InfraSpec,
        _: &str,
    ) -> Result<Option<String>, InfraError> {
        Ok(self.started_at.lock().ok().and_then(|s| s.clone()))
    }
    async fn adopt(&self, _: &[String]) -> Result<Value, InfraError> {
        Ok(Value::Null)
    }
    async fn doctor(&self) -> Value {
        json!({"docker": {"reachable": true}})
    }
}
