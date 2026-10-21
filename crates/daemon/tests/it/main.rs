//! In-process engine tests over temporary homes and real temporary git repositories,
//! with fake runner, infra and process table. Every test has a timeout and stops
//! what it started.

mod engine;
mod failures;
mod fakes;
mod hosts;
mod pacing;
mod profiles;
mod pty;
mod remote;
mod resolver;
mod retention;
mod server;
mod urls;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use devlanes_daemon::{Engine, EngineOptions, Handler, Session, integrations::Integrations};
use devlanes_ipc::Frame;
use devlanes_proto::{Actor, ActorKind, Context, Control, Event, Method, Request, RpcError};
use devlanes_state::Store;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use fakes::{FakeInfra, FakeProcesses, FakeRunner};

/// Upper bound for every test body.
pub const LIMIT: Duration = Duration::from_secs(60);

/// A temporary directory with its canonical path (macOS `/var` is a symlink).
pub struct Temp {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

/// Creates a canonical temporary directory with a short path.
pub fn temp() -> Temp {
    let dir = tempfile::Builder::new()
        .prefix("dle-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let path = dir.path().canonicalize().expect("canonical");
    Temp { _dir: dir, path }
}

/// Runs git with test-local identity and no global config.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository on `main` with `devlanes.yaml` committed.
pub fn repo(parent: &Path, name: &str, config: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(&path).expect("mkdir");
    git(&path, &["init", "-q", "-b", "main"]);
    std::fs::write(path.join("devlanes.yaml"), config).expect("config");
    git(&path, &["add", "devlanes.yaml"]);
    git(&path, &["commit", "-q", "-m", "init"]);
    path
}

/// Adds a worktree for a new branch.
pub fn worktree(repo: &Path, path: &Path, branch: &str) {
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            &path.display().to_string(),
        ],
    );
}

/// Test engine plus the fakes behind it.
pub struct Harness {
    pub engine: Engine,
    pub runner: FakeRunner,
    pub infra: FakeInfra,
    pub processes: FakeProcesses,
    pub home: PathBuf,
    next_id: AtomicU64,
}

/// Options for [`harness`].
#[derive(Default)]
pub struct Setup {
    pub watch: bool,
    pub infra: bool,
    pub processes: Option<FakeProcesses>,
    pub gate: Option<Arc<dyn devlanes_core::gate::StartGate>>,
    /// User config; the DNS server (when configured) then binds an ephemeral port.
    pub user: Option<devlanes_config::UserConfig>,
    /// Route sink, e.g. one reporting a bound HTTPS port.
    pub routes: Option<Arc<dyn devlanes_daemon::integrations::RouteSink>>,
}

/// Opens an engine over `home` (reusing its state.db when present).
pub async fn harness(home: &Path, setup: Setup) -> Harness {
    let runner = FakeRunner::default();
    let infra = FakeInfra::default();
    let processes = setup.processes.unwrap_or_default();
    let mut integ = Integrations::with_runner(Arc::new(runner.clone()));
    integ.processes = Arc::new(processes.clone());
    integ.start_gate = setup.gate.clone();
    if setup.infra {
        integ.infra = Arc::new(infra.clone());
    }
    if let Some(routes) = setup.routes {
        integ.routes = routes;
    }
    std::fs::create_dir_all(home).expect("home");
    let store = Store::open(home.join("state.db")).await.expect("store");
    let mut options = EngineOptions::new(home, home.join("d.sock"));
    options.watch = setup.watch;
    options.start_timeout = Duration::from_secs(10);
    if let Some(user) = setup.user {
        options.user = user;
        options.dns_listen = "127.0.0.1:0".parse().expect("address");
    }
    let engine = Engine::new(options, store, integ, CancellationToken::new());
    engine.reconcile().await.expect("reconcile");
    Harness {
        engine,
        runner,
        infra,
        processes,
        home: home.to_path_buf(),
        next_id: AtomicU64::new(1),
    }
}

/// A client environment with just enough to run.
pub fn client_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), "/tmp".into()),
    ])
}

/// Request context for `cwd`, with env and a human actor.
pub fn cx(cwd: &Path) -> Context {
    Context {
        cwd: cwd.to_path_buf(),
        env: Some(client_env()),
        actor: Actor::default(),
        ..Context::default()
    }
}

/// An agent actor with a session id.
pub fn agent(session: &str) -> Actor {
    Actor {
        kind: ActorKind::Agent,
        agent_id: Some("test".into()),
        session_id: Some(session.into()),
    }
}

impl Harness {
    /// Sends one request on a fresh local session.
    pub async fn call(
        &self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<Value, RpcError> {
        let (session, _frames) = Session::local(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.call_on(method, params, context, &session).await
    }

    /// Sends one request on `session`.
    pub async fn call_on(
        &self,
        method: Method,
        params: Value,
        context: Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let request = Request {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            method,
            params,
            context,
        };
        self.engine.handle(request, session.clone()).await
    }

    /// A local session and the frames sent to it.
    pub fn session(&self) -> (Session, mpsc::Receiver<Frame>) {
        Session::local(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Stops everything (test-owned cleanup).
    pub async fn close(&self) {
        let _ = self.engine.shutdown().await;
    }
}

/// Events of a frame stream until it is empty.
pub fn drain_events(frames: &mut mpsc::Receiver<Frame>) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        if let Frame::Control(Control::Event(event)) = frame {
            events.push(event);
        }
    }
    events
}

/// Polls `check` until it returns `Some` or the timeout passes.
pub async fn eventually<T, F, Fut>(limit: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(value) = check().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
