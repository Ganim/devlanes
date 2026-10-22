//! `devlanes with` and `stop --logs-dir` over the real client.

use std::{collections::BTreeMap, path::Path};

use async_trait::async_trait;
use devlanes_cli::{
    CommandExit, LaneLogQuery, ResourcePeak, UnitFailure, UnitLogs, WithDriver, WithOwnership,
    WithStart,
};
use devlanes_client::{Client, Error as ClientError, Paths};
use devlanes_ipc::Frame;
use devlanes_proto::{
    Context, Control, Event, LaneSummary, LogLine, LogQuery, Method, Outcome, Page, Request,
    RpcError, Selection, StartResult, UnitPeak,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Byte budget of the first `logs --all` page per unit; small, so one response for
/// many units stays far below the frame limit. Longer logs continue per unit.
const FIRST_PAGE_BYTES: usize = 64 * 1024;
/// Byte budget of each follow-up page of one unit.
const PAGE_BYTES: usize = 1024 * 1024;

/// A signal that asks `with` to stop: SIGINT (Ctrl-C) or SIGTERM (CI cancellation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Interrupt,
    Terminate,
}

impl Stop {
    /// The signal number, for the `128 + n` exit status.
    fn number(self) -> u8 {
        match self {
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }
}

/// SIGINT and SIGTERM handlers for `with`. Installing them before connecting means
/// neither signal kills the client before it has stopped what it started.
pub struct Signals(mpsc::UnboundedReceiver<Stop>);

impl Signals {
    /// Replaces the default dispositions for the rest of the process.
    #[cfg(unix)]
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    Some(()) = interrupt.recv() => Stop::Interrupt,
                    Some(()) = terminate.recv() => Stop::Terminate,
                    else => break,
                };
                if sender.send(received).is_err() {
                    break;
                }
            }
        });
        Ok(Self(receiver))
    }

    /// Replaces the default console handling for the rest of the process. Ctrl-C
    /// maps to [`Stop::Interrupt`] and Ctrl-Break, the closest thing to SIGTERM a
    /// console delivers, to [`Stop::Terminate`].
    #[cfg(windows)]
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c};
        let mut interrupt = ctrl_c()?;
        let mut terminate = ctrl_break()?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    Some(()) = interrupt.recv() => Stop::Interrupt,
                    Some(()) = terminate.recv() => Stop::Terminate,
                    else => break,
                };
                if sender.send(received).is_err() {
                    break;
                }
            }
        });
        Ok(Self(receiver))
    }
}

/// Keeps one connection and the invocation context for every follow-up query.
pub struct ClientDriver {
    client: Client,
    context: Context,
    paths: Paths,
    ownership: WithOwnership,
    signals: Option<Signals>,
    /// The tab progress bar during the start phase, when this invocation draws it.
    progress: Option<devlanes_term::Wrap>,
}

impl ClientDriver {
    /// Records whether this connection spawned the daemon before anything else happens.
    pub fn new(client: Client, context: Context, paths: Paths) -> Self {
        let ownership = WithOwnership {
            daemon_spawned: client.spawned,
            ..WithOwnership::default()
        };
        Self {
            client,
            context,
            paths,
            ownership,
            signals: None,
            progress: None,
        }
    }

    /// Lets SIGINT/SIGTERM cancel the start and reach the test command.
    pub fn with_signals(mut self, signals: Signals) -> Self {
        self.signals = Some(signals);
        self
    }

    /// Draws the start's progress in the terminal tab; it ends before the
    /// command runs, so the command's own output keeps the tab.
    pub fn with_progress(mut self, wrap: Option<devlanes_term::Wrap>) -> Self {
        self.progress = wrap;
        self
    }

    /// A signal that arrived since the last check, without waiting.
    fn pending_signal(&mut self) -> Option<Stop> {
        self.signals.as_mut()?.0.try_recv().ok()
    }

    /// Lane summaries in the scope of `context`; the start's ownership comes from
    /// comparing a snapshot before the start with one after an interrupt.
    async fn lanes(&mut self, context: &Context) -> Option<Vec<LaneSummary>> {
        let data = self
            .client
            .request(Method::Status, json!({}), context.clone())
            .await
            .ok()?;
        serde_json::from_value::<Page<LaneSummary>>(data)
            .ok()
            .map(|page| page.items)
    }

    /// One unit's lines after `cursor`, page by page until the daemon has no more.
    async fn rest_of_unit(
        &mut self,
        lane: &str,
        unit: &str,
        since: Option<String>,
        mut cursor: String,
    ) -> Result<Vec<LogLine>, RpcError> {
        let mut lines = Vec::new();
        loop {
            let query = LogQuery {
                units: vec![format!("{lane}:{unit}")],
                since: since.clone(),
                cursor: Some(cursor),
                max_bytes: Some(PAGE_BYTES),
                ..LogQuery::default()
            };
            let params = serde_json::to_value(&query).map_err(|error| {
                RpcError::new("bad_request", json!({"message": error.to_string()}), 1)
            })?;
            let context = self.lane_context(lane);
            let data = self.call(Method::Logs, params, context).await?;
            let page: Page<LogLine> = serde_json::from_value(data).map_err(|error| {
                RpcError::new("bad_response", json!({"message": error.to_string()}), 1)
            })?;
            let next = page.next_cursor.filter(|_| page.truncated);
            let progressed = !page.items.is_empty();
            lines.extend(page.items);
            match next {
                Some(next) if progressed => cursor = next,
                _ => return Ok(lines),
            }
        }
    }

    /// Lane-scoped context; the client env is only sent with process-starting requests.
    fn lane_context(&self, lane: &str) -> Context {
        Context {
            lane: Some(lane.to_owned()),
            env: None,
            ..self.context.clone()
        }
    }

    /// One request, mapping transport failures to the structured error shape.
    async fn call(
        &mut self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<Value, RpcError> {
        match self
            .client
            .request(method, params.clone(), context.clone())
            .await
        {
            Ok(data) => Ok(data),
            // Teardown must still happen after the connection broke (a frame over
            // the limit closes it, for one): reconnect once, never spawning a daemon.
            Err(error)
                if !matches!(error, ClientError::Rpc(_))
                    && matches!(method, Method::Stop | Method::DaemonShutdown) =>
            {
                self.client = Client::connect(&self.paths).await.map_err(rpc_error)?;
                self.client
                    .request(method, params, context)
                    .await
                    .map_err(rpc_error)
            }
            Err(error) => Err(rpc_error(error)),
        }
    }
}

/// The lane and units an interrupted start owns, from lane snapshots taken before
/// the start and after the interrupt: units active now that were not active before.
/// A lane that was idle (or did not exist) before belongs to the start as a whole.
/// `lane` comes from the request or the start's events; otherwise the lane whose
/// worktree contains `cwd`.
pub fn interrupted_ownership(
    before: &[LaneSummary],
    after: &[LaneSummary],
    lane: Option<&str>,
    cwd: &Path,
) -> Option<WithOwnership> {
    let current = match lane {
        Some(lane) => after
            .iter()
            .find(|summary| summary.reference == lane || summary.branch.as_deref() == Some(lane)),
        None => after
            .iter()
            .filter(|summary| devlanes_core::paths::path_starts_with(cwd, &summary.path))
            .max_by_key(|summary| summary.path.as_os_str().len()),
    }?;
    let previous = before
        .iter()
        .find(|summary| summary.reference == current.reference);
    // Not running: nothing this start may stop. `orphaned` units belong to nobody here.
    let active = |state: &str| !matches!(state, "idle" | "stopped" | "exited" | "unassigned");
    let units = current
        .units
        .iter()
        .filter(|unit| active(&unit.state) && unit.state != "orphaned")
        .filter(|unit| {
            !previous.is_some_and(|lane| {
                lane.units
                    .iter()
                    .any(|old| old.name == unit.name && active(&old.state) && old.state != "failed")
            })
        })
        .map(|unit| format!("{}:{}", current.reference, unit.name))
        .collect();
    Some(WithOwnership {
        lane: current.reference.clone(),
        units,
        whole_lane: previous.is_none_or(|lane| lane.state == "idle"),
        daemon_spawned: false,
    })
}

/// The start error for a signal; `with` exits `128 + n` after its teardown.
fn interrupted(stop: Stop) -> RpcError {
    RpcError::new(
        "interrupted",
        json!({"signal": if stop == Stop::Interrupt { "SIGINT" } else { "SIGTERM" }}),
        128 + stop.number(),
    )
}

/// Whether this process is in the terminal's foreground process group, so a Ctrl-C
/// on the terminal already reached the child (it shares the group).
#[cfg(unix)]
fn terminal_foreground() -> bool {
    rustix::termios::tcgetpgrp(std::io::stdin())
        .is_ok_and(|group| group == rustix::process::getpgrp())
}

/// Passes a stop on to the `with` command (`pid` as spawned).
#[cfg(unix)]
fn forward_stop(pid: Option<u32>, stop: Stop) {
    let Some(pid) = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let signal = match stop {
        // In the terminal's foreground group the child got the terminal's
        // SIGINT already; a second one would hit it twice.
        Stop::Interrupt if terminal_foreground() => return,
        Stop::Interrupt => rustix::process::Signal::INT,
        // A terminal never sends SIGTERM; CI cancellation sends it to us only.
        Stop::Terminate => rustix::process::Signal::TERM,
    };
    let _ = rustix::process::kill_process(pid, signal);
}

/// The `with` command before its environment and directory are set. std looks
/// `program` up on the `PATH` in `env` itself.
#[cfg(unix)]
async fn with_command(
    program: &str,
    args: &[String],
    _env: &BTreeMap<String, String>,
    _cwd: &Path,
) -> Result<tokio::process::Command, RpcError> {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    Ok(command)
}

/// Most `PATH` entries a `with` command's `not_found` error lists in
/// `path_searched`, as the daemon's `spawn_failed` does; `path_more` counts the
/// rest.
#[cfg(windows)]
const PATH_SHOWN: usize = 12;

/// The `with` command before its environment and directory are set. std would
/// only try `program.exe`, so the program is resolved with `PATHEXT` against the
/// `PATH` in `env` (finding `npm.cmd`), and batch files run through `cmd.exe`
/// with std's argument escaping. A program that is not found fails with 127,
/// an unusable command line (an argument a batch file cannot take, a NUL) with
/// 2.
#[cfg(windows)]
async fn with_command(
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<tokio::process::Command, RpcError> {
    use devlanes_runner_windows::command::CommandError;
    let spawn_failed = |message: String| {
        RpcError::new(
            "spawn_failed",
            json!({"message": message, "program": program}),
            127,
        )
    };
    let owned = (
        std::ffi::OsString::from(program),
        args.iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>(),
        env.clone(),
        cwd.to_owned(),
    );
    // Resolution checks candidate files, so it stays off the executor threads.
    let built = tokio::task::spawn_blocking(move || {
        let (program, args, env, cwd) = owned;
        devlanes_runner_windows::command::build(&program, &args, &env, &cwd)
    })
    .await
    .map_err(|error| spawn_failed(error.to_string()))?
    .map_err(|error| match &error {
        // Shaped like the daemon's `spawn_failed` for units, and 127 like a shell.
        CommandError::NotFound { searched, .. } => {
            let mut failed = spawn_failed(error.to_string());
            failed.data["reason"] = json!("not_found");
            failed.data["path_searched"] =
                json!(searched.iter().take(PATH_SHOWN).collect::<Vec<_>>());
            if searched.len() > PATH_SHOWN {
                failed.data["path_more"] = json!(searched.len() - PATH_SHOWN);
            }
            failed
        }
        // The command line itself is unusable, which no PATH or install fixes.
        CommandError::ArgumentNotSafeForBatch(_)
        | CommandError::ContainsNul(_)
        | CommandError::NotUnicode(_)
        | CommandError::InvalidProgram(_) => RpcError::new(
            "invalid_input",
            json!({"message": error.to_string(), "program": program}),
            2,
        ),
    })?;
    let mut command = tokio::process::Command::new(&built.application);
    if !built.arguments().is_empty() {
        command.raw_arg(built.arguments());
    }
    Ok(command)
}

/// Nothing to pass on: the `with` command shares this console, so Ctrl-C and
/// Ctrl-Break already reached it.
#[cfg(windows)]
fn forward_stop(_pid: Option<u32>, _stop: Stop) {}

/// The `with` command's exit: its code, or the signal that killed it.
#[cfg(unix)]
fn command_exit(status: std::process::ExitStatus) -> CommandExit {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => CommandExit::Code(u8::try_from(code).unwrap_or(1)),
        (None, Some(signal)) => CommandExit::Signal(u8::try_from(signal).unwrap_or(1)),
        (None, None) => CommandExit::Code(1),
    }
}

/// `STATUS_CONTROL_C_EXIT` as an exit code: the process ended on Ctrl-C.
#[cfg(windows)]
const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

/// The `with` command's exit code. A Ctrl-C exit reads like SIGINT on Unix, so
/// `with` reports and exits the same way on both. Codes above 255, such as an
/// NTSTATUS from a crash, become 1; the raw code goes to the debug log.
#[cfg(windows)]
fn command_exit(status: std::process::ExitStatus) -> CommandExit {
    match status.code() {
        Some(STATUS_CONTROL_C_EXIT) => CommandExit::Signal(Stop::Interrupt.number()),
        Some(code) => CommandExit::Code(u8::try_from(code).unwrap_or_else(|_| {
            tracing::debug!(
                code,
                hex = %format_args!("{:#010x}", code.cast_unsigned()),
                "with: exit code does not fit 0-255; reporting 1"
            );
            1
        })),
        None => CommandExit::Code(1),
    }
}

/// Transport errors become exit code 4 (daemon unreachable), RPC errors keep their own.
pub fn rpc_error(error: ClientError) -> RpcError {
    match error {
        ClientError::Rpc(error) => error,
        ClientError::UnsafeDirectory(path) => unsafe_directory(&path),
        // A daemon that refused to start reports `CODE: message`; keep the code.
        ClientError::Startup(reason) if startup_code(&reason).is_some() => {
            let (code, message) = startup_code(&reason).unwrap_or_default();
            RpcError::new(
                "daemon_unreachable",
                json!({"message": message, "reason": code}),
                4,
            )
        }
        ClientError::Sandbox(reason) => {
            RpcError::new("daemon_sandboxed", json!({"message": reason}), 4)
        }
        // Same shape as the daemon's own `protocol_mismatch`, so both read alike.
        ClientError::Incompatible { peer } => RpcError::new(
            "protocol_mismatch",
            json!({
                "protocol": peer.protocol,
                "client_protocol": devlanes_proto::PROTOCOL_VERSION,
                "version": peer.version,
                "build_id": peer.build_id,
                "managed_processes": peer.managed_processes,
            }),
            4,
        ),
        other => RpcError::new(
            "daemon_unreachable",
            json!({"message": other.to_string()}),
            4,
        ),
    }
}

/// Splits a daemon startup reason `STATE_LEGACY_SCHEMA: ...` into a lowercase
/// code (`state_legacy_schema`) and the message; `None` without such a prefix.
pub fn startup_code(reason: &str) -> Option<(String, String)> {
    let (code, message) = reason.split_once(':')?;
    let valid = !code.is_empty()
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    valid.then(|| (code.to_ascii_lowercase(), message.trim().to_owned()))
}

/// `unsafe_home` (exit 4): a state directory devlanes refuses, `{message, path,
/// problem, mode?, owner?, fix?}`. `problem` is `symlink`, `owner` (another
/// user's), `not_a_directory`, `unreadable`, or `mode` when tightening an own
/// directory to 0700 failed; only `mode` has a `fix` (`chmod 700 <path>`), since
/// the others need a person to decide. `owner` is the owning uid.
#[cfg(unix)]
pub fn unsafe_directory(path: &Path) -> RpcError {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path).ok();
    let problem = match &metadata {
        None => "unreadable",
        Some(meta) if meta.file_type().is_symlink() => "symlink",
        Some(meta) if !meta.is_dir() => "not_a_directory",
        Some(meta) if meta.uid() != rustix::process::geteuid().as_raw() => "owner",
        Some(_) => "mode",
    };
    let mut data = json!({
        "message": format!("unsafe devlanes home: {}", path.display()),
        "path": path,
        "problem": problem,
    });
    if let Some(meta) = &metadata {
        data["mode"] = json!(format!("{:o}", meta.permissions().mode() & 0o7777));
        data["owner"] = json!(meta.uid());
    }
    if problem == "mode" {
        data["fix"] = json!(format!(
            "chmod 700 {}",
            devlanes_cli::shell_quote(&path.display().to_string())
        ));
    }
    RpcError::new("unsafe_home", data, 4)
}

/// `unsafe_home` (exit 4): a state directory devlanes refuses, `{message, path,
/// problem, fix?}`. `problem` is `symlink` (also junctions), `owner` (another
/// account's, for example Administrators after an elevated run),
/// `not_a_directory`, `unreadable`, or `acl` when other accounts have access and
/// tightening it failed. Only `acl` has a `fix`, an `icacls` command keeping this
/// user, SYSTEM and Administrators; the others need a person to decide.
#[cfg(windows)]
pub fn unsafe_directory(path: &Path) -> RpcError {
    use devlanes_sys::windows::security::{DirProblem, check_private_dir};
    let (problem, others) = match check_private_dir(path) {
        Err(_) => ("unreadable", Vec::new()),
        Ok(Err(DirProblem::ReparsePoint)) => ("symlink", Vec::new()),
        Ok(Err(DirProblem::NotADirectory)) => ("not_a_directory", Vec::new()),
        Ok(Err(DirProblem::Owner)) => ("owner", Vec::new()),
        Ok(Err(DirProblem::Acl(others))) => ("acl", others),
        // A directory that checks out now was refused for its access list.
        Ok(Ok(())) => ("acl", Vec::new()),
    };
    let mut data = json!({
        "message": format!("unsafe devlanes home: {}", path.display()),
        "path": path,
        "problem": problem,
    });
    if problem == "acl"
        && let Some(fix) = acl_fix(path, &others)
    {
        data["fix"] = json!(fix);
    }
    RpcError::new("unsafe_home", data, 4)
}

/// Two `icacls` commands that leave `path` and everything in it to this user,
/// SYSTEM and Administrators. The first makes the directory private: stops
/// inheritance, grants those three, and removes the explicit grants of `others`
/// (SIDs), which `/grant:r` would keep. The second resets every entry inside to
/// inherit from it; entries keep their old grants otherwise. (`/T` on the first
/// would copy directory-only inheritance flags onto files and lock them.)
///
/// SIDs instead of names, which Windows translates per display language. The
/// quotes work in cmd and PowerShell, but no separator works in both, so the
/// commands are joined by ", then ".
#[cfg(windows)]
pub(crate) fn acl_fix(path: &Path, others: &[String]) -> Option<String> {
    let user = devlanes_sys::windows::token::current_user()
        .and_then(|sid| sid.to_sddl())
        .ok()?;
    let path = path.display();
    let mut directory = format!(
        r#"icacls "{path}" /inheritance:r /grant:r "*{user}:(OI)(CI)F" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F""#
    );
    if !others.is_empty() {
        directory.push_str(" /remove:g");
        for sid in others {
            directory.push_str(&format!(r#" "*{sid}""#));
        }
    }
    Some(format!(
        r#"{directory}, then icacls "{path}\*" /reset /T /C"#
    ))
}

#[async_trait]
impl WithDriver for ClientDriver {
    /// Streams start events and keeps ownership even when the start fails part-way.
    async fn start(
        &mut self,
        request: &Request,
        events: &mut (dyn FnMut(Event) + Send),
    ) -> Result<WithStart, RpcError> {
        // The tab's progress bar covers the start only; it is gone before the command runs.
        let progress = crate::progress::Follower::start_with(self.progress, request);
        let result: Result<WithStart, RpcError> = async {
            if let Some(stop) = self.pending_signal() {
                return Err(interrupted(stop));
            }
            // Only needed to work out ownership if a signal cuts the start short.
            let before = match self.signals {
                Some(_) => self.lanes(&request.context).await,
                None => None,
            };
            let id = self
                .client
                .begin_request(
                    request.method,
                    request.params.clone(),
                    request.context.clone(),
                )
                .await
                .map_err(rpc_error)?;
            let mut lane = request.context.lane.clone();
            loop {
                let frame = match self.signals.as_mut() {
                    Some(signals) => tokio::select! {
                        frame = self.client.next_frame() => frame,
                        Some(stop) = signals.0.recv() => {
                            // The daemon keeps starting after a client leaves, so the units
                            // this start brought up so far are stopped by the teardown.
                            if let Some(before) = &before {
                                let after = self.lanes(&request.context).await.unwrap_or_default();
                                if let Some(owned) = interrupted_ownership(
                                    before,
                                    &after,
                                    lane.as_deref(),
                                    &request.context.cwd,
                                ) {
                                    self.ownership = WithOwnership {
                                        daemon_spawned: self.ownership.daemon_spawned,
                                        ..owned
                                    };
                                }
                            }
                            return Err(interrupted(stop));
                        }
                    },
                    None => self.client.next_frame().await,
                };
                match frame.map_err(rpc_error)? {
                    Frame::Control(Control::Event(event)) => {
                        if lane.is_none()
                            && let Some(name) = event.data.get("lane").and_then(Value::as_str)
                        {
                            lane = Some(name.to_owned());
                        }
                        progress.event(&event);
                        events(event)
                    }
                    Frame::Control(Control::Response(response)) if response.id == id => {
                        return match response.result {
                            Outcome::Ok { data } => {
                                let raw = data.clone();
                                let result: StartResult =
                                    serde_json::from_value(data).map_err(|error| {
                                        RpcError::new(
                                            "bad_response",
                                            json!({"message": error.to_string()}),
                                            1,
                                        )
                                    })?;
                                self.ownership.lane.clone_from(&result.lane);
                                self.ownership.units.clone_from(&result.started);
                                self.ownership.whole_lane = result.whole_lane;
                                Ok(WithStart {
                                    ready: result.ready && result.failures.is_empty(),
                                    failures: result
                                        .failures
                                        .into_iter()
                                        .map(|failure| UnitFailure {
                                            lane: failure.lane,
                                            unit: failure.unit,
                                            exit_code: failure.exit_code,
                                            log_tail: failure.log_tail,
                                        })
                                        .collect(),
                                    result: raw,
                                })
                            }
                            Outcome::Error { error } => {
                                // A failed start may still name its lane, so teardown can find it.
                                if let Some(lane) = error.data.get("lane").and_then(Value::as_str) {
                                    self.ownership.lane = lane.to_owned();
                                }
                                if let Some(started) =
                                    error.data.get("started").and_then(Value::as_array)
                                {
                                    self.ownership.units = started
                                        .iter()
                                        .filter_map(|v| v.as_str().map(str::to_owned))
                                        .collect();
                                }
                                Err(error)
                            }
                        };
                    }
                    _ => {}
                }
            }
        }
        .await;
        // A signal ends the start on purpose; only a real failure turns the bar red.
        let ok = match &result {
            Ok(start) => start.ready && start.failures.is_empty(),
            Err(error) => error.code == "interrupted",
        };
        progress.finish(ok).await;
        result
    }

    /// The journal recorded by `start`.
    fn ownership(&self) -> WithOwnership {
        self.ownership.clone()
    }

    /// Lane-level env: DEVLANES_*, DEVLANES_PUBLIC_URL_<SERVICE>, CA variables.
    async fn env(&mut self, lane: &str) -> Result<BTreeMap<String, String>, RpcError> {
        let context = self.lane_context(lane);
        let data = self
            .call(Method::Env, json!({"lane_env": true}), context)
            .await?;
        serde_json::from_value(data.get("env").cloned().unwrap_or_default()).map_err(|error| {
            RpcError::new("bad_response", json!({"message": error.to_string()}), 1)
        })
    }

    /// All lines of every unit since the lane's current start: a small first page of
    /// every unit, then each truncated unit continues page by page with its cursor.
    async fn logs(
        &mut self,
        lane: &str,
        mut query: LaneLogQuery,
    ) -> Result<Vec<UnitLogs>, RpcError> {
        let bad = |error: serde_json::Error| {
            RpcError::new("bad_response", json!({"message": error.to_string()}), 1)
        };
        let since = query.query.since.clone();
        query.query.max_bytes = Some(FIRST_PAGE_BYTES);
        let context = self.lane_context(lane);
        let params = serde_json::to_value(&query).map_err(|error| {
            RpcError::new("bad_request", json!({"message": error.to_string()}), 1)
        })?;
        let data = self.call(Method::Logs, params, context).await?;
        // `{lane, units: [{unit, lines, truncated}]}`.
        let first = data
            .get("units")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut units = Vec::with_capacity(first.len());
        for page in first {
            let unit = page
                .get("unit")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut lines: Vec<LogLine> =
                serde_json::from_value(page.get("lines").cloned().unwrap_or_else(|| json!([])))
                    .map_err(bad)?;
            let truncated = page.get("truncated").and_then(Value::as_bool) == Some(true);
            // Forward cursors are `unit#seq` and exclude that line.
            if truncated && let Some(last) = lines.last() {
                let cursor = format!("{unit}#{}", last.seq);
                let rest = self
                    .rest_of_unit(lane, &unit, since.clone(), cursor)
                    .await?;
                lines.extend(rest);
            }
            units.push(UnitLogs { unit, lines });
        }
        Ok(units)
    }

    /// Peaks from run history for the summary table.
    async fn metrics(&mut self, lane: &str) -> Result<Vec<ResourcePeak>, RpcError> {
        let context = self.lane_context(lane);
        let data = self
            .call(Method::Metrics, json!({"peaks": true}), context)
            .await?;
        let peaks: Vec<UnitPeak> =
            serde_json::from_value(data.get("peaks").cloned().unwrap_or_else(|| json!([])))
                .map_err(|error| {
                    RpcError::new("bad_response", json!({"message": error.to_string()}), 1)
                })?;
        Ok(peaks
            .into_iter()
            .map(|peak| ResourcePeak {
                unit: peak.unit,
                peak_memory_bytes: peak.peak_memory_bytes,
                peak_cpu_percent: peak.peak_cpu_percent,
            })
            .collect())
    }

    /// Stops exactly the selection in that lane.
    async fn stop(&mut self, lane: &str, selection: Selection) -> Result<Value, RpcError> {
        let context = self.lane_context(lane);
        let params = serde_json::to_value(selection).map_err(|error| {
            RpcError::new("bad_request", json!({"message": error.to_string()}), 1)
        })?;
        self.call(Method::Stop, params, context).await
    }

    /// Only reached when this invocation spawned the daemon.
    async fn stop_daemon_if_spawned(&mut self) -> Result<(), RpcError> {
        let context = Context {
            yes: true,
            ..self.lane_context("")
        };
        self.call(Method::DaemonShutdown, json!({}), context)
            .await
            .map(drop)
    }

    /// Runs the test command in the foreground; Ctrl-C and SIGTERM go to the command,
    /// and teardown follows its exit.
    async fn spawn_command(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<CommandExit, RpcError> {
        // A signal while the lane env was fetched: skip the command, go to teardown.
        if let Some(stop) = self.pending_signal() {
            return Ok(CommandExit::Signal(stop.number()));
        }
        let Some((program, args)) = argv.split_first() else {
            return Err(RpcError::new(
                "usage",
                json!({"message": "missing command"}),
                2,
            ));
        };
        let mut command = with_command(program, args, env, cwd).await?;
        let mut child = command
            .env_clear()
            .envs(env)
            .current_dir(cwd)
            .spawn()
            .map_err(|error| {
                RpcError::new(
                    "spawn_failed",
                    json!({"message": error.to_string(), "program": program}),
                    127,
                )
            })?;
        let pid = child.id();
        loop {
            let received = async {
                match self.signals.as_mut() {
                    Some(signals) => signals.0.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                status = child.wait() => {
                    let status = status.map_err(|error| RpcError::new("wait_failed", json!({"message": error.to_string()}), 1))?;
                    return Ok(command_exit(status));
                }
                Some(stop) = received => forward_stop(pid, stop),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// A lane summary with `(unit, state)` pairs.
    fn lane(state: &str, units: &[(&str, &str)]) -> LaneSummary {
        LaneSummary {
            reference: "acme@2".into(),
            branch: Some("feat/billing".into()),
            path: "/work/acme-2".into(),
            state: state.into(),
            units: units
                .iter()
                .map(|(name, state)| devlanes_proto::UnitSummary {
                    name: (*name).into(),
                    lane: "acme@2".into(),
                    state: (*state).into(),
                    ..devlanes_proto::UnitSummary::default()
                })
                .collect(),
            ..LaneSummary::default()
        }
    }

    /// An interrupted start owns what became active since the snapshot, never what
    /// ran before or what a previous daemon left running.
    #[test]
    fn interrupted_start_owns_only_what_it_started() {
        let before = [lane(
            "running",
            &[
                ("db", "ready"),
                ("api", "stopped"),
                ("old", "orphaned"),
                ("web", "failed"),
            ],
        )];
        let after = [lane(
            "starting",
            &[
                ("db", "ready"),
                ("api", "starting"),
                ("old", "orphaned"),
                ("web", "queued"),
                ("seed", "idle"),
            ],
        )];
        let cwd = Path::new("/work/acme-2/src");
        let owned = interrupted_ownership(&before, &after, None, cwd).expect("lane");
        assert_eq!(owned.lane, "acme@2");
        assert_eq!(owned.units, ["acme@2:api", "acme@2:web"]);
        assert!(!owned.whole_lane);
        let by_branch =
            interrupted_ownership(&before, &after, Some("feat/billing"), Path::new("/"));
        assert_eq!(by_branch.map(|owned| owned.lane).as_deref(), Some("acme@2"));
        // An idle lane, or one that did not exist yet, belongs to the start as a whole.
        let idle = [lane("idle", &[("db", "idle")])];
        assert!(interrupted_ownership(&idle, &after, None, cwd).is_some_and(|o| o.whole_lane));
        assert!(interrupted_ownership(&[], &after, None, cwd).is_some_and(|o| o.whole_lane));
        assert!(interrupted_ownership(&before, &after, None, Path::new("/elsewhere")).is_none());
    }

    /// A fake daemon on `paths.socket`: answers hello, then hands every request of
    /// each accepted connection to `answer`; `None` drops that connection.
    // Windows has no local listener until named pipes land.
    #[cfg(unix)]
    fn fake_daemon(
        paths: &Paths,
        connections: usize,
        answer: impl Fn(&Request) -> Option<Value> + Send + 'static,
    ) -> std::io::Result<tokio::task::JoinHandle<Vec<Request>>> {
        let listener = tokio::net::UnixListener::bind(&paths.socket)?;
        Ok(tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..connections {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let mut connection = devlanes_ipc::Connection::new(stream);
                if connection.recv().await.is_err() {
                    break;
                }
                let hello = Control::Hello(devlanes_proto::Hello::current(0));
                if connection.send(Frame::Control(hello)).await.is_err() {
                    break;
                }
                while let Ok(Frame::Control(Control::Request(request))) = connection.recv().await {
                    let reply = answer(&request);
                    seen.push(request.clone());
                    let Some(data) = reply else { break };
                    let response = devlanes_proto::Response {
                        id: request.id,
                        result: Outcome::Ok { data },
                    };
                    if connection
                        .send(Frame::Control(Control::Response(response)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            seen
        }))
    }

    /// Paths with the socket inside `home`, away from the runner's XDG_RUNTIME_DIR.
    #[cfg(unix)]
    fn test_paths(home: &Path) -> Paths {
        let mut paths = Paths::new(home).expect("paths");
        paths.socket = paths.run.join("daemon.sock");
        paths.prepare().expect("prepare");
        paths
    }

    /// One log line of `unit`.
    #[cfg(unix)]
    fn line(unit: &str, seq: u64) -> Value {
        json!({"unit": unit, "seq": seq, "timestamp_ms": seq, "text": format!("line {seq}"), "level": null})
    }

    /// `with --logs-dir` gets every line: a truncated unit continues page by page
    /// with its cursor until the daemon has no more.
    #[cfg(unix)]
    #[tokio::test]
    async fn logs_page_each_truncated_unit_to_the_end() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = test_paths(&temp.path().join("h"));
            let daemon = fake_daemon(&paths, 1, |request| {
                let cursor = request.params.get("cursor").and_then(Value::as_str);
                Some(match cursor {
                    None => json!({"lane": "acme@0", "units": [
                        {"unit": "api", "lines": [line("api", 1)], "truncated": true},
                        {"unit": "web", "lines": [line("web", 1)], "truncated": false},
                    ]}),
                    Some("api#1") => json!({"items": [line("api", 2)], "truncated": true, "next_cursor": "api#2"}),
                    Some(_) => json!({"items": [line("api", 3)], "truncated": false, "next_cursor": null}),
                })
            })
            .expect("bind");
            let client = Client::connect(&paths).await.expect("connect");
            let mut driver = ClientDriver::new(client, Context::default(), paths.clone());
            let query = LaneLogQuery {
                all: true,
                query: LogQuery {
                    since: Some("start".into()),
                    ..LogQuery::default()
                },
            };
            let logs = driver.logs("acme@0", query).await.expect("logs");
            let seqs = |unit: &str| -> Vec<u64> {
                logs.iter()
                    .find(|logs| logs.unit == unit)
                    .map(|logs| logs.lines.iter().map(|line| line.seq).collect())
                    .unwrap_or_default()
            };
            assert_eq!(seqs("api"), [1, 2, 3]);
            assert_eq!(seqs("web"), [1]);
            drop(driver);
            let requests = daemon.await.expect("daemon");
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].params["max_bytes"], FIRST_PAGE_BYTES);
            assert_eq!(requests[1].params["units"], json!(["acme@0:api"]));
            assert_eq!(requests[1].params["since"], "start");
            assert_eq!(requests[2].params["cursor"], "api#2");
        })
        .await
        .expect("in time");
    }

    /// A connection that broke (say, on an oversized frame) does not cost the
    /// teardown: stop reconnects once, without spawning a daemon.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_reconnects_after_a_broken_connection() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = test_paths(&temp.path().join("h"));
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = calls.clone();
            let daemon = fake_daemon(&paths, 2, move |_| {
                // The first request kills the connection; the retry is answered.
                (counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0)
                    .then(|| json!({"items": []}))
            })
            .expect("bind");
            let client = Client::connect(&paths).await.expect("connect");
            let mut driver = ClientDriver::new(client, Context::default(), paths.clone());
            let selection = Selection {
                units: vec!["acme@0:api".into()],
                ..Selection::default()
            };
            driver
                .stop("acme@0", selection)
                .await
                .expect("stop after reconnect");
            drop(driver);
            let requests = daemon.await.expect("daemon");
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.method == Method::Stop)
            );
        })
        .await
        .expect("in time");
    }

    /// A daemon of another protocol reads like the daemon's own `protocol_mismatch`.
    #[test]
    fn incompatible_daemon_is_a_protocol_mismatch() {
        let mut peer = devlanes_proto::Hello::current(6);
        peer.protocol += 1;
        let error = rpc_error(ClientError::Incompatible { peer });
        assert_eq!(error.code, "protocol_mismatch");
        // Plan 3.5: an incompatible daemon exits 4.
        assert_eq!(error.exit_code, 4);
        assert_eq!(error.data["managed_processes"], 6);
        assert_eq!(
            error.data["client_protocol"],
            devlanes_proto::PROTOCOL_VERSION
        );
    }

    /// Startup reasons keep their code, so hints match it instead of the wording.
    #[test]
    fn startup_reasons_keep_their_code() {
        assert_eq!(
            startup_code("STATE_LEGACY_SCHEMA: state.db has a pre-release schema"),
            Some((
                "state_legacy_schema".into(),
                "state.db has a pre-release schema".into()
            ))
        );
        assert_eq!(startup_code("could not bind: address in use"), None);
        assert_eq!(startup_code("no colon"), None);
        let error = rpc_error(ClientError::Startup("STATE_LEGACY_SCHEMA: old".into()));
        assert_eq!(
            (error.code.as_str(), error.data["reason"].as_str()),
            ("daemon_unreachable", Some("state_legacy_schema"))
        );
    }

    #[cfg(unix)]
    #[test]
    /// `unsafe_home` carries path, mode and owner; an own home whose tightening
    /// failed carries `chmod 700 <dir>` as its fix, a symlinked one no command,
    /// since chmod would not make it safe.
    fn unsafe_directory_names_the_fix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("my home");
        std::fs::create_dir(&home).expect("home");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = rpc_error(ClientError::UnsafeDirectory(home.clone()));
        assert_eq!((error.code.as_str(), error.exit_code), ("unsafe_home", 4));
        assert_eq!(error.data["path"], json!(home));
        assert_eq!(error.data["owner"], rustix::process::geteuid().as_raw());
        assert_eq!(error.data["problem"], "mode");
        assert_eq!(error.data["mode"], "755");
        assert_eq!(error.data["fix"], format!("chmod 700 '{}'", home.display()));

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&home, &link).expect("symlink");
        let error = rpc_error(ClientError::UnsafeDirectory(link));
        assert_eq!(error.data["problem"], "symlink");
        assert!(error.data.get("fix").is_none());
    }

    #[cfg(windows)]
    #[test]
    /// Each refusal is named; only an access list problem comes with an icacls fix.
    fn unsafe_directory_names_the_problem() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let error = |path: &Path| {
            let error = rpc_error(ClientError::UnsafeDirectory(path.to_owned()));
            assert_eq!(error.code, "unsafe_home");
            assert_eq!(error.exit_code, 4);
            assert!(error.data.get("mode").is_none());
            error.data
        };
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").expect("file");
        assert_eq!(error(&file)["problem"], "not_a_directory");
        assert!(error(&file).get("fix").is_none());
        assert_eq!(error(&tmp.path().join("missing"))["problem"], "unreadable");
        let open_dir = tmp.path().join("open");
        std::fs::create_dir(&open_dir).expect("dir");
        // Everyone, read access.
        let granted = std::process::Command::new("icacls")
            .arg(&open_dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("icacls runs");
        assert!(granted.success());
        let open = error(&open_dir);
        assert_eq!(open["problem"], "acl");
        let fix = open["fix"].as_str().expect("fix");
        assert!(
            fix.starts_with("icacls \"") && fix.contains("/inheritance:r"),
            "{fix}"
        );
        assert!(
            fix.contains(r#"/remove:g "*S-1-1-0", then icacls"#),
            "{fix}"
        );
        assert!(fix.ends_with(r#"\*" /reset /T /C"#), "{fix}");
    }

    #[cfg(windows)]
    #[test]
    /// A command ended by Ctrl-C exits like one killed by SIGINT on Unix.
    fn ctrl_c_exit_reads_as_sigint() {
        use std::os::windows::process::ExitStatusExt;
        let exit = |code: u32| command_exit(std::process::ExitStatus::from_raw(code));
        assert!(matches!(exit(0xC000_013A), CommandExit::Signal(2)));
        assert!(matches!(exit(0), CommandExit::Code(0)));
        assert!(matches!(exit(3), CommandExit::Code(3)));
        assert!(matches!(exit(300), CommandExit::Code(1)));
        assert!(matches!(exit(0xC000_0005), CommandExit::Code(1)));
    }

    #[cfg(windows)]
    #[tokio::test]
    /// A missing program fails like the daemon's unit spawns (127, `not_found`,
    /// the PATH searched); an unusable command line is invalid input (2).
    async fn with_command_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = BTreeMap::from([("PATH".to_owned(), dir.path().display().to_string())]);
        let missing = with_command("devlanes-no-such-program", &[], &env, dir.path())
            .await
            .expect_err("not found");
        assert_eq!(
            (missing.code.as_str(), missing.exit_code),
            ("spawn_failed", 127)
        );
        assert_eq!(missing.data["reason"], "not_found");
        assert_eq!(missing.data["program"], "devlanes-no-such-program");
        assert_eq!(missing.data["path_searched"], json!([dir.path()]));
        let crowded = (0..20)
            .map(|n| dir.path().join(n.to_string()).display().to_string())
            .collect::<Vec<_>>()
            .join(";");
        let env = BTreeMap::from([("PATH".to_owned(), crowded)]);
        let missing = with_command("devlanes-no-such-program", &[], &env, dir.path())
            .await
            .expect_err("not found");
        assert_eq!(
            missing.data["path_searched"].as_array().map(Vec::len),
            Some(PATH_SHOWN)
        );
        assert_eq!(missing.data["path_more"], 20 - PATH_SHOWN);
        for (program, args) in [("", vec![]), ("tool", vec!["a\0b".to_owned()])] {
            let invalid = with_command(program, &args, &env, dir.path())
                .await
                .expect_err("invalid");
            assert_eq!(
                (invalid.code.as_str(), invalid.exit_code),
                ("invalid_input", 2),
                "{program:?} {args:?}"
            );
        }
    }
}
