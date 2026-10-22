//! In-process engine tests over temporary homes and real temporary git repositories,
//! with fake runner, infra and process table. Every test has a timeout and stops
//! what it started.

mod attach;
mod containers;
mod engine;
mod failures;
mod fakes;
mod folders;
mod hosts;
mod lanes;
mod leftovers;
mod lifecycle;
mod local_sync;
mod metrics;
mod orphans;
mod pacing;
mod profiles;
// The PTY runner is Unix-only; Windows units run through `devlanes-runner-windows`.
#[cfg(unix)]
mod pty;
mod remote;
mod resolver;
mod retention;
mod runs;
mod server;
mod shutdown;
mod stop_in_start;
mod stream;
mod update;
mod urls;
mod watch;

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

/// Creates a canonical temporary directory with a short path (`/tmp` on Unix,
/// where the system one can be too long for a socket; `%TEMP%` on Windows).
pub fn temp() -> Temp {
    let parent = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    #[cfg(windows)]
    sweep_stale(&parent);
    let dir = tempfile::Builder::new()
        .prefix("dle-")
        .tempdir_in(parent)
        .expect("tempdir");
    let path = dunce::canonicalize(dir.path()).expect("canonical");
    Temp { _dir: dir, path }
}

/// Removes `dle-*` directories in `parent` older than any test runs. Windows
/// cannot delete open files, and an engine's background tasks (or the first
/// engine a crash test leaves behind) keep `state.db` and logs open until the
/// test process exits, so some directories outlive their test.
#[cfg(windows)]
fn sweep_stale(parent: &Path) {
    const STALE: Duration = Duration::from_secs(15 * 60);
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let ours = entry.file_name().to_string_lossy().starts_with("dle-");
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if ours && old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The daemon endpoint for a server whose home is `home`: the socket
/// `run/d.sock` in it on Unix, a pipe named after the directory on Windows.
pub fn endpoint(home: &Path) -> PathBuf {
    if cfg!(windows) {
        let name = home
            .strip_prefix(std::env::temp_dir())
            .unwrap_or(home)
            .display()
            .to_string()
            .replace(['\\', '/', ':'], "-");
        PathBuf::from(format!(r"\\.\pipe\devlanes-test-{name}"))
    } else {
        home.join("run/d.sock")
    }
}

/// Runs git with test-local identity, no global config and no hooks.
pub fn git(dir: &Path, args: &[&str]) -> String {
    // An empty hooks directory and config file work on every OS, unlike `/dev/null`.
    let hooks = tempfile::Builder::new()
        .prefix("dle-hooks-")
        .tempdir()
        .expect("hooks dir");
    let global_config = tempfile::Builder::new()
        .prefix("dle-gitconfig-")
        .tempfile()
        .expect("empty git config")
        .into_temp_path();
    let mut hooks_path = std::ffi::OsString::from("core.hooksPath=");
    hooks_path.push(hooks.path());
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-c")
        .arg(hooks_path)
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
    /// Logs directory instead of `<home>/logs`. A simulated crash leaves the old
    /// in-process engine holding its log writers, which a real crash would release.
    pub logs: Option<PathBuf>,
    /// Name constraints of the CA (`None`: unconstrained, as with mkcert).
    pub ca_permitted: Option<Vec<String>>,
    /// Metrics collectors; none keeps the sampler off.
    pub metrics: Vec<Arc<dyn devlanes_metrics::MetricsSource>>,
    /// Port holder lookups; none identifies no holder.
    pub ports: Option<Arc<dyn devlanes_daemon::integrations::PortInspector>>,
    /// Machine CPU and memory for `metrics.system`.
    pub system: Option<Arc<dyn devlanes_core::gate::PressureSource>>,
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
    integ.ca_permitted = setup.ca_permitted;
    integ.metrics = setup.metrics;
    integ.system_pressure = setup.system;
    if let Some(ports) = setup.ports {
        integ.ports = ports;
    }
    std::fs::create_dir_all(home).expect("home");
    let store = Store::open(home.join("state.db")).await.expect("store");
    let mut options = EngineOptions::new(home, home.join("d.sock"));
    options.watch = setup.watch;
    options.start_timeout = Duration::from_secs(10);
    if let Some(logs) = setup.logs {
        options.logs = logs;
    }
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
#[cfg(unix)]
pub fn client_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), "/tmp".into()),
    ])
}

/// A client environment with just enough to run: Windows programs need
/// `SystemRoot`, and `PATH` is the test's own so real units find their tools.
#[cfg(windows)]
pub fn client_env() -> BTreeMap<String, String> {
    let var = |name| std::env::var(name).unwrap_or_default();
    BTreeMap::from([
        ("PATH".into(), var("PATH")),
        ("SystemRoot".into(), var("SystemRoot")),
        (
            "USERPROFILE".into(),
            std::env::temp_dir().display().to_string(),
        ),
    ])
}

/// Request context for `cwd`, with env and a human actor. Inside a test's
/// [`temp`] directory, the home (`HOME`, `USERPROFILE` on Windows) is that
/// directory's `user-home`, so whatever a unit or a `~` path writes there (a
/// service's `sync` folder) goes away with the test.
pub fn cx(cwd: &Path) -> Context {
    let mut env = client_env();
    if let Some(root) = cwd.ancestors().find(|dir| {
        dir.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("dle-"))
    }) {
        env.insert(
            devlanes_core::paths::HOME_VAR.into(),
            root.join("user-home").display().to_string(),
        );
    }
    Context {
        cwd: cwd.to_path_buf(),
        env: Some(env),
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

    /// Stops everything (test-owned cleanup), as a signal would: no `lane.stop` tasks.
    pub async fn close(&self) {
        let _ = self.engine.shutdown(signal_shutdown()).await;
    }
}

/// The shutdown SIGTERM asks for.
pub fn signal_shutdown() -> devlanes_daemon::Shutdown {
    devlanes_daemon::Shutdown::new(devlanes_daemon::ShutdownReason::Signal)
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
