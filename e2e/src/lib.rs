//! Harness for end-to-end tests that run the real `devlanes` binary as separate processes.
//!
//! Each [`Lab`] owns a short `DEVLANES_HOME`, a Git repository copied from a fixture, a
//! private slice of the port pool and proxy ports, and a deadline. Dropping it stops the
//! daemon and kills only processes the test saw (identified by PID and start time).
//!
//! Unix only for now: ports are locked under `/tmp`, and processes are inspected with
//! `ps` and `lsof` and killed with signals. On other platforms the crate is empty.

#![cfg(unix)]

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use devlanes_testing::{TestHome, TestRepo};
use serde_json::Value;

/// Result type of the harness.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Failures of the harness itself or of a command that did not behave as required.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Creating the test home or repository failed.
    #[error(transparent)]
    Testing(#[from] devlanes_testing::Error),
    /// A command printed something that is not the JSON it should be.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The binaries or the lab could not be prepared.
    #[error("{0}")]
    Setup(String),
    /// A command or a wait outlived its bound.
    #[error("timed out after {after:?}: {what}\nstdout:\n{stdout}\nstderr:\n{stderr}")]
    Timeout {
        what: String,
        after: Duration,
        stdout: String,
        stderr: String,
    },
    /// A command exited or answered differently than the test required.
    #[error("{0}")]
    Unexpected(String),
}

/// Default bound for one CLI invocation.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// First port of the e2e port slots; slots stay inside the default pool's range and
/// below the Linux and macOS ephemeral ranges.
const SLOT_BASE: u16 = 26000;
const SLOT_SIZE: u16 = 100;
const SLOTS: u16 = 60;

/// Root of the workspace this crate belongs to.
pub fn workspace_root() -> &'static Path {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest)
}

/// Directory holding `devlanes` and `fixture-server`.
///
/// `DEVLANES_E2E_BIN_DIR` wins. Otherwise the first call runs
/// `cargo build -p devlanes -p devlanes-testing` into `$CARGO_TARGET_DIR` (relative to the
/// workspace root) or `target`, and uses its `debug` directory.
pub fn bin_dir() -> Result<&'static Path> {
    static DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    DIR.get_or_init(|| locate_bins().map_err(|error| error.to_string()))
        .as_deref()
        .map_err(|error| Error::Setup(error.clone()))
}

/// Resolves the binary directory, building it when no explicit directory is given.
fn locate_bins() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DEVLANES_E2E_BIN_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        return check_bins(&dir).map(|()| dir);
    }
    let root = workspace_root();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| root.join("target"), |dir| root.join(dir));
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args([
            "build",
            "--quiet",
            "-p",
            "devlanes",
            "-p",
            "devlanes-testing",
        ])
        .arg("--target-dir")
        .arg(&target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(Error::Setup(format!(
            "cargo build -p devlanes -p devlanes-testing failed ({status}); \
             set DEVLANES_E2E_BIN_DIR to use prebuilt binaries"
        )));
    }
    let dir = target.join("debug");
    check_bins(&dir).map(|()| dir)
}

/// Both binaries must exist before any lab starts.
fn check_bins(dir: &Path) -> Result<()> {
    for name in ["devlanes", "fixture-server"] {
        if !dir.join(name).is_file() {
            return Err(Error::Setup(format!(
                "{} is missing; build with cargo build -p devlanes -p devlanes-testing",
                dir.join(name).display()
            )));
        }
    }
    Ok(())
}

/// Path of a fixture directory in this repository.
pub fn fixture_dir(name: &str) -> PathBuf {
    workspace_root().join("fixtures").join(name)
}

/// The `devlanes.yaml` of a fixture, for tests that extend it.
pub fn fixture_yaml(name: &str) -> Result<String> {
    Ok(fs::read_to_string(fixture_dir(name).join("devlanes.yaml"))?)
}

/// A machine-wide lock on 100 ports, so parallel labs never share ports.
struct PortSlot {
    base: u16,
    _lock: File,
}

impl PortSlot {
    /// Takes the first free slot; the lock is released when the file closes, even on a crash.
    fn acquire() -> Result<Self> {
        let dir = Path::new("/tmp").join(format!(
            "devlanes-e2e-ports-{}",
            rustix::process::geteuid().as_raw()
        ));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        for slot in 0..SLOTS {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("{slot}.lock")))?;
            match file.try_lock() {
                Ok(()) => {
                    let base = SLOT_BASE + slot * SLOT_SIZE;
                    // An earlier run that died before its cleanup may have left these.
                    reap_fixture_listeners(base, base + SLOT_SIZE - 1);
                    return Ok(Self { base, _lock: file });
                }
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        Err(Error::Setup("every e2e port slot is in use".into()))
    }
}

/// Exit status and captured output of one finished command.
#[derive(Debug, Clone)]
pub struct Output {
    /// What ran, for messages.
    pub what: String,
    /// Exit code; `None` when a signal ended the process.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// True on exit code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Fails with the full output unless the exit code is `code`.
    pub fn expect_code(&self, code: i32) -> Result<&Self> {
        if self.code == Some(code) {
            Ok(self)
        } else {
            Err(Error::Unexpected(format!(
                "expected exit code {code}\n{self}"
            )))
        }
    }

    /// Parses stdout as one JSON document, or the last line of an NDJSON stream.
    pub fn json(&self) -> Result<Value> {
        match serde_json::from_str(&self.stdout) {
            Ok(value) => Ok(value),
            Err(error) => self
                .stdout
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(serde_json::from_str)
                .unwrap_or(Err(error))
                .map_err(|error| {
                    Error::Unexpected(format!("stdout is not JSON ({error})\n{self}"))
                }),
        }
    }

    /// Every line of stdout that parses as JSON.
    pub fn ndjson(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

impl fmt::Display for Output {
    /// Multi-line form for assertion messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "$ {}\nexit: {:?}\nstdout:\n{}\nstderr:\n{}",
            self.what, self.code, self.stdout, self.stderr
        )
    }
}

/// A process the test saw, with the start time that proves it is still the same process.
struct Owned {
    pid: i32,
    started: String,
}

/// One isolated devlanes environment: home, repository, worktrees, ports and a deadline.
pub struct Lab {
    home: TestHome,
    repo: TestRepo,
    /// Empty file every command uses as its global Git config.
    git_config: tempfile::TempPath,
    worktrees: tempfile::TempDir,
    slot: PortSlot,
    deadline: Instant,
    /// Compose projects the guard removes; any also passes the Docker client settings through.
    compose_projects: Vec<String>,
    owned: Mutex<Vec<Owned>>,
}

impl Lab {
    /// A lab whose repository is a copy of `fixtures/<name>`.
    pub fn new(fixture: &str, budget: Duration) -> Result<Self> {
        let source = fixture_dir(fixture);
        Self::create(budget, |repo| copy_dir(&source, repo))
    }

    /// A lab for Docker tests: the guard also runs `docker compose -p <compose_project>
    /// down --volumes` in the primary worktree.
    pub fn docker(fixture: &str, compose_project: &str, budget: Duration) -> Result<Self> {
        Self::docker_projects(fixture, &[compose_project], budget)
    }

    /// Like [`Lab::docker`] for several Compose projects (a shared one and the
    /// `<project>-lane<N>` ones). They are removed before the test too, so leftovers of
    /// a run that died cannot leak data into this one.
    pub fn docker_projects(fixture: &str, projects: &[&str], budget: Duration) -> Result<Self> {
        let source = fixture_dir(fixture);
        let mut lab = Self::create(budget, |repo| copy_dir(&source, repo))?;
        lab.compose_projects = projects.iter().map(|&p| p.to_owned()).collect();
        lab.compose_down();
        Ok(lab)
    }

    /// A lab whose repository holds only this `devlanes.yaml`.
    pub fn with_config(yaml: &str, budget: Duration) -> Result<Self> {
        Self::create(budget, |repo| {
            fs::write(repo.join("devlanes.yaml"), yaml).map_err(Error::from)
        })
    }

    /// A lab whose repository has a commit but no `devlanes.yaml`.
    pub fn without_config(budget: Duration) -> Result<Self> {
        Self::create(budget, |repo| {
            fs::write(repo.join("README.md"), "no devlanes config here\n").map_err(Error::from)
        })
    }

    /// Writes a file into the primary worktree without committing it (e.g. `.env`).
    pub fn write(&self, relative: &str, contents: &str) -> Result<()> {
        Ok(fs::write(self.repo().join(relative), contents)?)
    }

    /// A port of this lab's slot outside the lane pool and the proxy ports; `index` 0..=7.
    pub fn spare_port(&self, index: u16) -> u16 {
        self.slot.base + 92 + index.min(7)
    }

    /// Builds the home, user config and committed repository; `budget` bounds the whole test.
    fn create(budget: Duration, populate: impl FnOnce(&Path) -> Result<()>) -> Result<Self> {
        // The first lab of a test process may build the binaries; that must not count
        // against the test's budget.
        bin_dir()?;
        let deadline = Instant::now() + budget;
        let slot = PortSlot::acquire()?;
        let home = TestHome::new()?;
        let base = slot.base;
        // The proxy gets its own ports so parallel labs and a developer's daemon never
        // compete for 443/8443; the lane pool stays below them.
        fs::write(
            home.path().join("config.yaml"),
            format!(
                "ports:\n  range: {base}-{}\nproxy:\n  https_port: {}\n  http_port: {}\n",
                base + 79,
                base + 90,
                base + 91
            ),
        )?;
        let repo = TestRepo::new()?;
        repo.git(&["config", "commit.gpgsign", "false"])?;
        populate(repo.path())?;
        repo.git(&["add", "-A"])?;
        repo.git(&["commit", "-q", "-m", "fixture"])?;
        let worktrees = tempfile::Builder::new().prefix("devlanes-wt-").tempdir()?;
        Ok(Self {
            home,
            repo,
            git_config: devlanes_testing::empty_git_config()?,
            worktrees,
            slot,
            deadline,
            compose_projects: Vec::new(),
            owned: Mutex::new(Vec::new()),
        })
    }

    /// The primary worktree (lane 0).
    pub fn repo(&self) -> &Path {
        self.repo.path()
    }

    /// `DEVLANES_HOME` of this lab.
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// The HTTPS port the proxy is configured to bind.
    pub fn https_port(&self) -> u16 {
        self.slot.base + 90
    }

    /// The directory that holds linked worktrees; tests may create or move worktrees in it.
    pub fn worktrees_dir(&self) -> &Path {
        self.worktrees.path()
    }

    /// Adds a linked worktree on a new branch, outside the primary worktree.
    pub fn add_worktree(&self, branch: &str) -> Result<PathBuf> {
        let path = self.worktrees.path().join(branch.replace('/', "-"));
        let target = path.to_string_lossy().into_owned();
        self.repo
            .git(&["worktree", "add", "-q", "-b", branch, &target])?;
        Ok(path)
    }

    /// Runs Git in the primary worktree.
    pub fn git(&self, args: &[&str]) -> Result<()> {
        self.repo.git(args)?;
        Ok(())
    }

    /// Time left before the test's deadline.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// A `devlanes` command with the lab's isolated environment and `DEVLANES_OUTPUT=json`.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Result<Command> {
        let bins = bin_dir()?;
        let mut command = Command::new(bins.join("devlanes"));
        command.env_clear();
        for key in ["TMPDIR", "USER", "LOGNAME", "LANG"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut path = std::ffi::OsString::from(bins);
        if let Some(system) = std::env::var_os("PATH") {
            path.push(":");
            path.push(system);
        }
        command.env("PATH", path);
        self.home.configure(&mut command);
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("DEVLANES_AGENT", "0")
            .env("DEVLANES_OUTPUT", "json")
            .env("DEVLANES_LOG", "debug");
        if !self.compose_projects.is_empty() {
            self.docker_env(&mut command);
        }
        command.current_dir(cwd).args(args).stdin(Stdio::null());
        Ok(command)
    }

    /// Runs `docker <args>` in the primary worktree with the outer shell's Docker
    /// client settings, bounded by [`COMMAND_TIMEOUT`] and the test deadline.
    pub fn docker_cli(&self, args: &[&str]) -> Result<Output> {
        let mut command = Command::new("docker");
        self.docker_env(&mut command);
        command
            .current_dir(self.repo())
            .args(args)
            .stdin(Stdio::null());
        self.run(command, COMMAND_TIMEOUT)
    }

    /// Docker reads its context from `$HOME/.docker`, which the isolated `HOME` hides.
    fn docker_env(&self, command: &mut Command) {
        for key in ["DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if std::env::var_os("DOCKER_CONFIG").is_none()
            && let Some(home) = std::env::var_os("HOME")
        {
            let config = Path::new(&home).join(".docker");
            if config.is_dir() {
                command.env("DOCKER_CONFIG", config);
            }
        }
    }

    /// Runs `devlanes <args>` in `cwd` within [`COMMAND_TIMEOUT`] and the test deadline.
    pub fn devlanes(&self, cwd: &Path, args: &[&str]) -> Result<Output> {
        let command = self.command(cwd, args)?;
        self.run(command, COMMAND_TIMEOUT)
    }

    /// Like [`Lab::devlanes`] but requires exit 0 and returns the parsed JSON.
    pub fn json(&self, cwd: &Path, args: &[&str]) -> Result<Value> {
        self.devlanes(cwd, args)?.expect_code(0)?.json()
    }

    /// The lanes of `status --json` from `cwd`.
    pub fn lanes(&self, cwd: &Path) -> Result<Vec<Value>> {
        let status = self.json(cwd, &["status", "--json"])?;
        Ok(status["items"].as_array().cloned().unwrap_or_default())
    }

    /// Runs a configured command, bounded by `timeout` and the test deadline. Every PID
    /// in JSON output is recorded so the guard can kill leftovers.
    pub fn run(&self, command: Command, timeout: Duration) -> Result<Output> {
        let bound = timeout.min(self.remaining());
        if bound.is_zero() {
            return Err(Error::Timeout {
                what: describe(&command),
                after: Duration::ZERO,
                stdout: String::new(),
                stderr: "the test deadline has passed".into(),
            });
        }
        let output = run_bounded(command, bound)?;
        for value in output.ndjson() {
            self.track_json(&value);
        }
        Ok(output)
    }

    /// Spawns a long-running `devlanes` process with piped output; the caller must kill
    /// or wait for it. Its PID is recorded for the guard.
    pub fn spawn(&self, mut command: Command) -> Result<Child> {
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.track(child.id().cast_signed());
        Ok(child)
    }

    /// Polls `probe` every 100 ms until it returns `Some`, failing after `timeout`.
    pub fn eventually<T>(
        &self,
        what: &str,
        timeout: Duration,
        mut probe: impl FnMut() -> Result<Option<T>>,
    ) -> Result<T> {
        let bound = timeout.min(self.remaining());
        let start = Instant::now();
        let mut last = String::new();
        loop {
            match probe() {
                Ok(Some(value)) => return Ok(value),
                Ok(None) => {}
                Err(error) => last = error.to_string(),
            }
            if start.elapsed() >= bound {
                return Err(Error::Timeout {
                    what: what.into(),
                    after: bound,
                    stdout: String::new(),
                    stderr: last,
                });
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// True while some process holds the daemon lock of this home. Never spawns a daemon.
    pub fn daemon_running(&self) -> bool {
        let Ok(lock) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.home().join("run/daemon.lock"))
        else {
            return false;
        };
        matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock))
    }

    /// PID of the running daemon, without spawning one.
    pub fn daemon_pid(&self) -> Result<Option<i32>> {
        if !self.daemon_running() {
            return Ok(None);
        }
        let status = self.json(self.repo(), &["daemon", "status", "--json"])?;
        Ok(status["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok()))
    }

    /// Records a process the test is allowed to kill.
    pub fn track(&self, pid: i32) {
        let Some(started) = start_time(pid) else {
            return;
        };
        if let Ok(mut owned) = self.owned.lock()
            && !owned.iter().any(|o| o.pid == pid && o.started == started)
        {
            owned.push(Owned { pid, started });
        }
    }

    /// Records every `pid` field in a JSON value.
    fn track_json(&self, value: &Value) {
        match value {
            Value::Object(map) => {
                if let Some(pid) = map.get("pid").and_then(Value::as_i64)
                    && let Ok(pid) = i32::try_from(pid)
                {
                    self.track(pid);
                }
                map.values().for_each(|v| self.track_json(v));
            }
            Value::Array(items) => items.iter().for_each(|v| self.track_json(v)),
            _ => {}
        }
    }

    /// Stops the daemon, removes Docker resources and kills recorded processes:
    /// every unit the lanes still list (a previous daemon's orphans included,
    /// which `daemon stop` leaves running), with their process groups, and any
    /// fixture still listening on the slot's ports.
    fn shutdown(&self) {
        if self.daemon_running() {
            for args in [
                &["daemon", "status", "--json"][..],
                &["status", "--all", "--json"][..],
            ] {
                if let Ok(command) = self.command(self.repo(), args)
                    && let Ok(output) = run_bounded(command, Duration::from_secs(10))
                {
                    output.ndjson().iter().for_each(|v| self.track_json(v));
                }
            }
            if let Ok(command) = self.command(self.repo(), &["daemon", "stop", "--yes"]) {
                let _ = run_bounded(command, Duration::from_secs(20));
            }
            let start = Instant::now();
            while self.daemon_running() && start.elapsed() < Duration::from_secs(10) {
                thread::sleep(Duration::from_millis(100));
            }
        }
        if thread::panicking() || std::env::var_os("DEVLANES_E2E_DUMP").is_some() {
            self.dump_daemon_log();
        }
        self.compose_down();
        let owned = self
            .owned
            .lock()
            .map(|owned| owned.iter().map(|o| (o.pid, o.started.clone())).collect())
            .unwrap_or_else(|_| Vec::new());
        for (pid, started) in owned {
            if start_time(pid).as_deref() == Some(started.as_str()) {
                kill_tree(pid);
            }
        }
        reap_fixture_listeners(self.slot.base, self.slot.base + SLOT_SIZE - 1);
    }

    /// Removes the lab's Compose projects with their volumes.
    fn compose_down(&self) {
        for project in &self.compose_projects {
            let mut down = Command::new("docker");
            self.docker_env(&mut down);
            down.current_dir(self.repo())
                .args([
                    "compose",
                    "-p",
                    project,
                    "down",
                    "--volumes",
                    "--remove-orphans",
                ])
                .args(["--timeout", "5"])
                .stdin(Stdio::null());
            let _ = run_bounded(down, Duration::from_secs(120));
        }
    }

    /// Prints the end of the daemon log, which disappears with the home.
    fn dump_daemon_log(&self) {
        if let Ok(log) = fs::read_to_string(self.home().join("logs/daemon.log")) {
            let lines: Vec<&str> = log.lines().collect();
            let tail = &lines[lines.len().saturating_sub(80)..];
            eprintln!("--- daemon.log (last {} lines) ---", tail.len());
            for line in tail {
                eprintln!("{line}");
            }
        }
    }
}

impl Drop for Lab {
    /// Always runs `devlanes daemon stop --yes`, then kills only recorded processes.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Runs a command with piped output and kills it after `timeout`.
pub fn run_bounded(mut command: Command, timeout: Duration) -> Result<Output> {
    let what = describe(&command);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = reader(child.stdout.take());
    let stderr = reader(child.stderr.take());
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A grandchild may keep a pipe open; never wait on it for long.
    let stdout = stdout
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    let stderr = stderr
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    match status {
        Some(status) => Ok(Output {
            what,
            code: status.code(),
            stdout,
            stderr,
        }),
        None => Err(Error::Timeout {
            what,
            after: timeout,
            stdout,
            stderr,
        }),
    }
}

/// Reads a pipe to the end on its own thread.
fn reader(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = sender.send(String::from_utf8_lossy(&buffer).into_owned());
        });
    }
    receiver
}

/// Program and arguments, for messages.
fn describe(command: &Command) -> String {
    let program = Path::new(command.get_program())
        .file_name()
        .map_or_else(
            || command.get_program().to_string_lossy(),
            |n| n.to_string_lossy(),
        )
        .into_owned();
    std::iter::once(program)
        .chain(command.get_args().map(|a| a.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Copies a fixture directory recursively.
fn copy_dir(source: &Path, target: &Path) -> Result<()> {
    if !source.is_dir() {
        return Err(Error::Setup(format!(
            "missing fixture {}",
            source.display()
        )));
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let to = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&to)?;
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

/// Start time as `ps` reports it; `None` once the process is gone.
pub fn start_time(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// True while `pid` exists (zombies included).
pub fn alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// Sends SIGKILL; a missing process is not an error.
pub fn kill(pid: i32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
}

/// SIGKILL to `pid` and, when it leads a process group (every unit does), to
/// the whole group, so a unit's children die with it.
pub fn kill_tree(pid: i32) {
    if let Some(leader) = rustix::process::Pid::from_raw(pid)
        && rustix::process::getpgid(Some(leader)).ok() == Some(leader)
    {
        let _ = rustix::process::kill_process_group(leader, rustix::process::Signal::KILL);
    }
    kill(pid);
}

/// Kills `fixture-server` processes listening on ports `first..=last`. A test
/// that failed before cleaning up (or a daemon's orphan) must not hold the
/// slot's ports for later runs; nothing but the fixture is ever touched.
fn reap_fixture_listeners(first: u16, last: u16) {
    let Ok(output) = Command::new("lsof")
        .args([
            "-nP",
            "-t",
            &format!("-iTCP:{first}-{last}"),
            "-sTCP:LISTEN",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    for pid in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
    {
        let command = Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default();
        if Path::new(&command)
            .file_name()
            .is_some_and(|name| name == "fixture-server")
        {
            kill_tree(pid);
        }
    }
}

/// True when something accepts TCP connections on `127.0.0.1:port`.
pub fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// Plain HTTP/1.1 GET against `127.0.0.1:port`; returns the status code and body.
pub fn http_get(port: u16, path: &str) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Error::Unexpected(format!("not an HTTP response: {response:?}")))?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    Ok((status, body))
}

/// The lane with this canonical reference (`project@N`).
pub fn lane<'a>(lanes: &'a [Value], reference: &str) -> Option<&'a Value> {
    lanes.iter().find(|lane| lane["reference"] == reference)
}

/// The unit called `name` in a lane summary.
pub fn unit<'a>(lane: &'a Value, name: &str) -> Option<&'a Value> {
    lane["units"]
        .as_array()?
        .iter()
        .find(|unit| unit["name"] == name)
}

/// The `default` port of a unit summary.
pub fn port(unit: &Value) -> Option<u16> {
    unit["ports"]["default"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
}

/// The PID of a unit summary.
pub fn pid(unit: &Value) -> Option<i32> {
    unit["pid"].as_i64().and_then(|p| i32::try_from(p).ok())
}
