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
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

/// Byte budget of the first `logs --all` page per unit; small, so one response for
/// many units stays far below the frame limit. Longer logs continue per unit.
const FIRST_PAGE_BYTES: usize = 64 * 1024;
/// Byte budget of each follow-up page of one unit.
const PAGE_BYTES: usize = 1024 * 1024;

/// A signal that asks `with` to stop: SIGINT (Ctrl-C) or SIGTERM (CI cancellation).
/// On Windows, Ctrl-C and Ctrl-Break.
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
    /// Handles Ctrl-C as SIGINT and Ctrl-Break as SIGTERM for the rest of the
    /// process: the console sends both to every process attached to it, so
    /// this one survives them to stop what it started.
    #[cfg(windows)]
    pub fn install() -> std::io::Result<Self> {
        let mut interrupt = tokio::signal::windows::ctrl_c()?;
        let mut terminate = tokio::signal::windows::ctrl_break()?;
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

    /// Replaces the default dispositions for the rest of the process.
    #[cfg(unix)]
    pub fn install() -> std::io::Result<Self> {
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
    /// The tab title for the whole run; put back when the driver drops.
    title: Option<crate::terminal::Title>,
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
            title: None,
        }
    }

    /// Keeps the tab's title current from the start's events.
    pub fn with_title(mut self, title: Option<crate::terminal::Title>) -> Self {
        self.title = title;
        self
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

/// `spawn_failed` for a test command that did not start: exit 127 like a
/// shell when it is missing (with the `PATH` directories searched) or could
/// not be created, exit 2 when the command line itself is refused (a batch
/// file argument `cmd.exe` cannot take, a NUL, a drive-relative program).
#[cfg(windows)]
fn foreground_error(program: String, error: &devlanes_runner_windows::SpawnError) -> RpcError {
    use devlanes_runner_windows::SpawnError;
    let mut data = json!({"message": error.to_string(), "program": program});
    let exit = match error {
        SpawnError::Io(_) => 127,
        SpawnError::NotFound { searched, .. } => {
            data["path_searched"] = json!(searched);
            127
        }
        _ => 2,
    };
    if !matches!(error, SpawnError::Io(_)) {
        data["reason"] = json!(error.code());
    }
    RpcError::new("spawn_failed", data, exit)
}

/// Whether this process is in the terminal's foreground process group, so a Ctrl-C
/// on the terminal already reached the child (it shares the group).
#[cfg(unix)]
fn terminal_foreground() -> bool {
    rustix::termios::tcgetpgrp(std::io::stdin())
        .is_ok_and(|group| group == rustix::process::getpgrp())
}

/// Transport errors become exit code 4 (daemon unreachable), RPC errors keep their own.
/// A daemon that refused an invalid user config already arrives as the client's
/// `config_error`, so it reads and hints like a strict load of that file.
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
/// problem, owner?, acl?, fix?}`. `problem` is `reparse_point` (a symlink or
/// junction), `owner` (another account's; `owner` is its SID),
/// `not_a_directory`, `unreadable`, or `acl` when other accounts (`acl`, their
/// SIDs) can reach an own directory the daemon could not tighten because it
/// holds entries. Only `acl` has a `fix`, the three `icacls` steps.
#[cfg(windows)]
pub fn unsafe_directory(path: &Path) -> RpcError {
    let mut data = json!({
        "message": format!("unsafe devlanes home: {}", path.display()),
        "path": path,
    });
    match privacy_problem(path) {
        Ok(Some(facts)) => extend(&mut data, facts),
        // The check passes now: the access list was the problem when it failed.
        Ok(None) => {
            data["problem"] = json!("acl");
            data["fix"] = json!(icacls_fix(path));
        }
        Err(_) => data["problem"] = json!("unreadable"),
    }
    RpcError::new("unsafe_home", data, 4)
}

/// Copies the fields of `extra` into the object `data`.
#[cfg(windows)]
fn extend(data: &mut Value, extra: Value) {
    if let (Some(data), Value::Object(extra)) = (data.as_object_mut(), extra) {
        data.extend(extra);
    }
}

/// `{problem, owner?, acl?, fix?}` when the directory `path` is not private to
/// this user (see [`unsafe_directory`]); `None` when it is. Blocking.
#[cfg(windows)]
pub fn privacy_problem(path: &Path) -> std::io::Result<Option<Value>> {
    use devlanes_sys_windows::security::{Problem, check_private_dir};
    let Err(problem) = check_private_dir(path)? else {
        return Ok(None);
    };
    Ok(Some(match problem {
        Problem::ReparsePoint => json!({"problem": "reparse_point"}),
        Problem::NotADirectory | Problem::NotAFile => json!({"problem": "not_a_directory"}),
        Problem::Owner(owner) => json!({"problem": "owner", "owner": owner.to_string()}),
        Problem::Acl(others) => json!({
            "problem": "acl",
            "acl": others.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "fix": icacls_fix(path),
        }),
        Problem::HardLinks(_) | Problem::Unreadable => json!({"problem": "unreadable"}),
    }))
}

/// The `icacls` steps that make the own directory `path` private, as
/// [`devlanes_pki::icacls_fix`] writes them; `None` when this user's SID
/// cannot be read.
#[cfg(windows)]
fn icacls_fix(path: &Path) -> Option<String> {
    devlanes_pki::icacls_fix(path, true)
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
                        crate::terminal::title_event(&mut self.title, &event);
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

    /// Runs the test command in the foreground, resolved like units (`PATHEXT`,
    /// batch files through `cmd.exe`), and teardown follows its exit. Ctrl-C
    /// and Ctrl-Break are not forwarded: the command shares this console, so
    /// they reached it already. Its exit code reads like a shell's: 130 after
    /// Ctrl-C, codes above 255 as 1.
    #[cfg(windows)]
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
        // Resolving the program checks files, so it stays off the executor.
        let started = {
            let (program, args) = (program.clone(), args.to_vec());
            let (env, cwd) = (env.clone(), cwd.to_path_buf());
            tokio::task::spawn_blocking(move || {
                devlanes_runner_windows::spawn_foreground(&program, &args, &env, &cwd)
            })
            .await
            .map_err(|error| {
                RpcError::new("spawn_failed", json!({"message": error.to_string()}), 1)
            })?
        };
        let child = started.map_err(|error| foreground_error(program.clone(), &error))?;
        let child = std::sync::Arc::new(child);
        let mut waiter = tokio::task::spawn_blocking({
            let child = child.clone();
            move || child.wait()
        });
        loop {
            let received = async {
                match self.signals.as_mut() {
                    Some(signals) => signals.0.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                status = &mut waiter => {
                    let wait_failed = |message: String| {
                        RpcError::new("wait_failed", json!({"message": message}), 1)
                    };
                    let code = status
                        .map_err(|error| wait_failed(error.to_string()))?
                        .map_err(|error| wait_failed(error.to_string()))?;
                    return Ok(CommandExit::Code(devlanes_runner_windows::shell_exit_code(code)));
                }
                // The console delivered the event to the command as well.
                Some(_) = received => {}
            }
        }
    }

    /// Runs the test command in the foreground; Ctrl-C and SIGTERM go to the command,
    /// and teardown follows its exit.
    #[cfg(unix)]
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
        let mut child = tokio::process::Command::new(program)
            .args(args)
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
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw);
        let forward = |signal| {
            if let Some(pid) = pid {
                let _ = rustix::process::kill_process(pid, signal);
            }
        };
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
                    use std::os::unix::process::ExitStatusExt;
                    return Ok(match (status.code(), status.signal()) {
                        (Some(code), _) => CommandExit::Code(u8::try_from(code).unwrap_or(1)),
                        (None, Some(signal)) => CommandExit::Signal(u8::try_from(signal).unwrap_or(1)),
                        (None, None) => CommandExit::Code(1),
                    });
                }
                Some(stop) = received => match stop {
                    // In the terminal's foreground group the child got the terminal's
                    // SIGINT already; a second one would hit it twice.
                    Stop::Interrupt if terminal_foreground() => {}
                    Stop::Interrupt => forward(rustix::process::Signal::INT),
                    // A terminal never sends SIGTERM; CI cancellation sends it to us only.
                    Stop::Terminate => forward(rustix::process::Signal::TERM),
                },
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
    async fn fake_daemon(
        paths: &Paths,
        connections: usize,
        answer: impl Fn(&Request) -> Option<Value> + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<Vec<Request>>, devlanes_ipc::local::BindError> {
        let mut listener = devlanes_ipc::local::LocalListener::bind(&paths.socket).await?;
        Ok(tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..connections {
                let Ok(stream) = listener.accept().await else {
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

    /// Paths with the socket inside `home`, away from the runner's
    /// XDG_RUNTIME_DIR. A Windows pipe is named after the home already.
    fn test_paths(home: &Path) -> Paths {
        #[cfg_attr(windows, expect(unused_mut, reason = "the pipe name stays"))]
        let mut paths = Paths::new(home).expect("paths");
        #[cfg(unix)]
        {
            paths.socket = paths.run.join("daemon.sock");
        }
        paths.prepare().expect("prepare");
        paths
    }

    /// One log line of `unit`.
    fn line(unit: &str, seq: u64) -> Value {
        json!({"unit": unit, "seq": seq, "timestamp_ms": seq, "text": format!("line {seq}"), "level": null})
    }

    /// `with --logs-dir` gets every line: a truncated unit continues page by page
    /// with its cursor until the daemon has no more.
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
            .await
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
            .await
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

    /// `with` resolves its command like units do (`PATHEXT`, batch files),
    /// exits with the command's code as a shell reports it, and refuses
    /// missing programs (127) and unsafe batch arguments (2) with their reason.
    #[cfg(windows)]
    #[tokio::test]
    async fn with_runs_the_command_and_keeps_its_exit_code() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = test_paths(&temp.path().join("h"));
            let _daemon = fake_daemon(&paths, 1, |_| None).await.expect("bind");
            let client = Client::connect(&paths).await.expect("connect");
            let mut driver = ClientDriver::new(client, Context::default(), paths.clone());
            let bin = temp.path().join("bin");
            std::fs::create_dir(&bin).expect("bin");
            std::fs::write(bin.join("tool.cmd"), "@exit /b 5\r\n").expect("tool.cmd");
            let system = devlanes_sys_windows::file::system_directory().expect("System32");
            let mut env: BTreeMap<String, String> = ["SYSTEMROOT", "PATHEXT", "TEMP"]
                .iter()
                .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
                .collect();
            env.insert(
                "PATH".into(),
                format!("{};{}", bin.display(), system.display()),
            );
            let argv = |args: &[&str]| -> Vec<String> {
                args.iter().map(|arg| (*arg).to_owned()).collect()
            };
            let mut code = async |args: &[&str]| {
                driver
                    .spawn_command(&argv(args), &env, temp.path())
                    .await
                    .map(CommandExit::code)
            };
            assert_eq!(code(&["cmd", "/c", "exit 3"]).await.expect("cmd"), 3);
            assert_eq!(code(&["tool"]).await.expect("tool.cmd"), 5);
            // STATUS_CONTROL_C_EXIT, as a process ended by Ctrl-C exits.
            assert_eq!(
                code(&["cmd", "/c", "exit -1073741510"]).await.expect("cmd"),
                130
            );
            assert_eq!(code(&["cmd", "/c", "exit 300"]).await.expect("cmd"), 1);
            let missing = code(&["no-such-tool"]).await.expect_err("missing");
            assert_eq!(
                (missing.code.as_str(), missing.exit_code),
                ("spawn_failed", 127)
            );
            assert_eq!(missing.data["reason"], "not_found");
            assert_eq!(missing.data["path_searched"], json!([bin, system]));
            let unsafe_arg = code(&["tool", "a\nb"]).await.expect_err("refused");
            assert_eq!(unsafe_arg.exit_code, 2);
            assert_eq!(unsafe_arg.data["reason"], "unsafe_batch_argument");
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
        // An unreadable user config line (the client parses a whole one into a
        // `config_error`) keeps its code as the reason, like other startup codes.
        let error = rpc_error(ClientError::Startup("USER_CONFIG_INVALID: cut{".into()));
        assert_eq!(
            (error.code.as_str(), error.data["reason"].as_str()),
            ("daemon_unreachable", Some("user_config_invalid"))
        );
    }

    /// `unsafe_home` names the accounts that can reach an own home and carries
    /// the `icacls` steps as its fix; a junction gets no command.
    #[cfg(windows)]
    #[test]
    fn unsafe_directory_names_the_fix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("my home");
        std::fs::create_dir(&home).expect("home");
        let system = devlanes_sys_windows::file::system_directory().expect("System32");
        let run = |program: &str, args: &[&std::ffi::OsStr]| {
            let status = std::process::Command::new(system.join(program))
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .expect("run");
            assert!(status.success(), "{program} {args:?}");
        };
        run(
            "icacls.exe",
            &[
                home.as_os_str(),
                "/grant".as_ref(),
                "*S-1-1-0:(OI)(CI)R".as_ref(),
            ],
        );
        let error = rpc_error(ClientError::UnsafeDirectory(home.clone()));
        assert_eq!((error.code.as_str(), error.exit_code), ("unsafe_home", 4));
        assert_eq!(error.data["path"], json!(home));
        assert_eq!(error.data["problem"], "acl");
        assert_eq!(error.data["acl"], json!(["S-1-1-0"]));
        let user = devlanes_sys_windows::token::current_user().expect("user");
        assert_eq!(
            error.data["fix"],
            format!(
                r#"icacls "{dir}" /reset; icacls "{dir}" /inheritance:r /grant:r "*{user}:(OI)(CI)F"; icacls "{dir}\*" /reset /T /C"#,
                dir = home.display()
            )
        );

        let link = tmp.path().join("link");
        run(
            "cmd.exe",
            &[
                "/d".as_ref(),
                "/c".as_ref(),
                "mklink".as_ref(),
                "/J".as_ref(),
                link.as_os_str(),
                home.as_os_str(),
            ],
        );
        let error = rpc_error(ClientError::UnsafeDirectory(link));
        assert_eq!(error.data["problem"], "reparse_point");
        assert!(error.data.get("fix").is_none());
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
}
