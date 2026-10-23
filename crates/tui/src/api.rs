//! Daemon access behind a trait, so rendering and key handling are testable with a fake.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_client::{Client, Frame, Paths};
use devlanes_proto::{
    Actor, ActorKind, AttachParams, AttachResult, Context, Control, Event, LaneSummary, LogLine,
    LogQuery, Method, Outcome, Page, ResizeParams, RpcError, RunParams, Selection,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

/// Input and resize queues are small: a human cannot type faster than the socket drains.
const INPUT_QUEUE: usize = 256;
/// Output is forwarded per frame; the pump waits when the UI falls behind.
const OUTPUT_QUEUE: usize = 1024;
const EVENT_QUEUE: usize = 1024;

/// Failures the TUI shows in its status line or reacts to by reconnecting.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ApiError {
    /// The socket closed or could not be opened; the TUI retries.
    #[error("daemon unreachable: {0}")]
    Disconnected(String),
    /// The daemon answered with a structured failure.
    #[error("{}", .0.code)]
    Rpc(RpcError),
    /// The daemon answered with data this client does not understand.
    #[error("unexpected daemon data: {0}")]
    Decode(String),
}

impl ApiError {
    /// True when the connection itself is gone and a reconnect is needed.
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Disconnected(_))
    }
}

impl From<devlanes_client::Error> for ApiError {
    /// Keeps structured daemon errors and folds every transport failure into a disconnect.
    fn from(error: devlanes_client::Error) -> Self {
        match error {
            devlanes_client::Error::Rpc(rpc) => Self::Rpc(rpc),
            other => Self::Disconnected(other.to_string()),
        }
    }
}

/// What the monitor asks `metrics` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsQuery {
    /// How far back histories reach.
    pub range_ms: u64,
    /// Buckets per history; 0 sends every raw sample (one a second).
    pub points: usize,
}

/// One bucket of a history with its spread: `[start_ms, cpu_min, cpu_avg,
/// cpu_max, mem_min, mem_avg, mem_max]` on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Window {
    pub start_ms: u64,
    pub cpu_min: f64,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    pub mem_min: u64,
    pub mem_avg: u64,
    pub mem_max: u64,
}

/// One unit's or container's resource usage from `Method::Metrics`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitMetrics {
    /// The lane, or `<project> (shared)` for a project-wide container.
    pub lane: String,
    pub unit: String,
    /// `service`, `task`, `exec` or `container`; empty from older daemons.
    pub kind: String,
    pub role: Option<String>,
    /// The Docker container name, on container rows.
    pub container: Option<String>,
    pub shared: bool,
    /// Lanes that use a container.
    pub lanes: Vec<String>,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    /// Highest CPU and memory since the lane started.
    pub peak_cpu_percent: Option<f64>,
    pub peak_memory_bytes: Option<u64>,
    /// `(ts_ms, cpu_percent, memory_bytes)`, oldest first; with buckets, the
    /// highest CPU and the average memory of each.
    pub history: Vec<(u64, f64, u64)>,
    pub windows: Vec<Window>,
    /// Typical memory of recent runs, when the daemon sends one.
    pub baseline_memory_bytes: Option<u64>,
}

impl UnitMetrics {
    /// True for an infra container row.
    pub fn is_container(&self) -> bool {
        self.kind == "container"
    }
}

/// A lane's totals from its `Lane` series: its units and its own containers,
/// without shared ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaneMetrics {
    pub lane: String,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub peak_cpu_percent: Option<f64>,
    pub peak_memory_bytes: Option<u64>,
    pub history: Vec<(u64, f64, u64)>,
    pub windows: Vec<Window>,
}

/// The whole machine, when the daemon reads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemMetrics {
    pub cpus: Option<u32>,
    pub memory_total_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    /// Every process on the machine, 0 to 100.
    pub cpu_percent: Option<f64>,
    /// `normal`, `warn` or `critical`.
    pub memory_pressure: Option<String>,
    pub cpu_stall_percent: Option<f64>,
    /// Each CPU now, 0 to 100; empty from daemons that do not send it.
    pub core_percents: Vec<f64>,
    /// `(ts_ms, cpu_percent, available_memory_bytes)`, oldest first.
    pub history: Vec<(u64, f64, u64)>,
}

/// Everything one `metrics` call returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    /// The range the histories cover; `None` from daemons that ignore `range_ms`.
    pub range_ms: Option<u64>,
    pub units: Vec<UnitMetrics>,
    pub lanes: Vec<LaneMetrics>,
    pub system: Option<SystemMetrics>,
}

/// A number field as `u64`; JSON numbers may arrive as floats.
fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_f64).map(|n| n as u64)
}

/// `[[ts, cpu, bytes], ...]`, skipping points that do not parse.
fn points(value: &Value) -> Vec<(u64, f64, u64)> {
    value
        .get("history")
        .and_then(Value::as_array)
        .map(|points| {
            points
                .iter()
                .filter_map(|point| {
                    let point = point.as_array()?;
                    Some((
                        point.first()?.as_f64()? as u64,
                        point.get(1)?.as_f64()?,
                        point.get(2)?.as_f64()? as u64,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `windows` rows, skipping ones that do not parse.
fn windows(value: &Value) -> Vec<Window> {
    value
        .get("windows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let row = row.as_array()?;
                    let at = |i: usize| row.get(i).and_then(Value::as_f64);
                    Some(Window {
                        start_ms: at(0)? as u64,
                        cpu_min: at(1)?,
                        cpu_avg: at(2)?,
                        cpu_max: at(3)?,
                        mem_min: at(4)? as u64,
                        mem_avg: at(5)? as u64,
                        mem_max: at(6)? as u64,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Metrics {
    /// Parses a `metrics` reply leniently: a daemon without metrics yet
    /// returns `{}` or `null`, which is empty, and older daemons send only
    /// `units` with `lane`, `unit`, `cpu_percent`, `memory_bytes`, `history`.
    pub fn parse(value: &Value) -> Self {
        let text = |row: &Value, key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
        let units = value
            .get("units")
            .and_then(Value::as_array)
            .map(|units| {
                units
                    .iter()
                    .filter_map(|unit| {
                        Some(UnitMetrics {
                            lane: text(unit, "lane")?,
                            unit: text(unit, "unit")?,
                            kind: text(unit, "kind").unwrap_or_default(),
                            role: text(unit, "role"),
                            container: text(unit, "container"),
                            shared: unit.get("shared").and_then(Value::as_bool) == Some(true),
                            lanes: unit
                                .get("lanes")
                                .and_then(Value::as_array)
                                .map(|lanes| {
                                    lanes
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_default(),
                            cpu_percent: unit.get("cpu_percent").and_then(Value::as_f64),
                            memory_bytes: number(unit, "memory_bytes"),
                            peak_cpu_percent: unit.get("peak_cpu_percent").and_then(Value::as_f64),
                            peak_memory_bytes: number(unit, "peak_memory_bytes"),
                            history: points(unit),
                            windows: windows(unit),
                            baseline_memory_bytes: number(unit, "baseline_memory_bytes"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let lanes = value
            .get("lanes")
            .and_then(Value::as_array)
            .map(|lanes| {
                lanes
                    .iter()
                    .filter_map(|lane| {
                        Some(LaneMetrics {
                            lane: text(lane, "lane")?,
                            cpu_percent: lane.get("cpu_percent").and_then(Value::as_f64),
                            memory_bytes: number(lane, "memory_bytes"),
                            peak_cpu_percent: lane.get("peak_cpu_percent").and_then(Value::as_f64),
                            peak_memory_bytes: number(lane, "peak_memory_bytes"),
                            history: points(lane),
                            windows: windows(lane),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let system = value
            .get("system")
            .filter(|s| s.is_object())
            .map(|system| SystemMetrics {
                cpus: number(system, "cpus").and_then(|n| u32::try_from(n).ok()),
                memory_total_bytes: number(system, "memory_total_bytes"),
                available_memory_bytes: number(system, "available_memory_bytes"),
                cpu_percent: system.get("cpu_percent").and_then(Value::as_f64),
                memory_pressure: text(system, "memory_pressure"),
                cpu_stall_percent: system.get("cpu_stall_percent").and_then(Value::as_f64),
                core_percents: system
                    .get("core_percents")
                    .and_then(Value::as_array)
                    .map(|cores| cores.iter().filter_map(Value::as_f64).collect())
                    .unwrap_or_default(),
                history: points(system),
            });
        Self {
            range_ms: number(value, "range_ms"),
            units,
            lanes,
            system,
        }
    }
}

/// Live subscription events. An `Err` item says why the subscription ended;
/// a closed channel without one means the connection closed cleanly.
pub struct EventStream {
    events: mpsc::Receiver<Result<Event, ApiError>>,
}

impl EventStream {
    /// Wraps a channel; fakes push events into the sending half.
    pub fn new(events: mpsc::Receiver<Result<Event, ApiError>>) -> Self {
        Self { events }
    }

    /// Waits for the next event; `None` means the subscription is gone.
    pub async fn next(&mut self) -> Option<Result<Event, ApiError>> {
        self.events.recv().await
    }
}

/// Requests sent from the UI to an attached terminal stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachInput {
    Bytes(Bytes),
    Resize { columns: u16, rows: u16 },
}

/// What an attached stream delivers: terminal output, or a failure of input,
/// resize or the stream itself. The channel closes when the stream ends.
#[derive(Debug, Clone)]
pub enum AttachEvent {
    Output(Bytes),
    /// The stream failed; it closes next.
    Error(ApiError),
    /// The daemon refused a resize; the stream goes on (a pipe unit has no terminal).
    ResizeRefused(ApiError),
}

/// An open terminal stream. Dropping it detaches; the unit keeps running.
pub struct AttachSession {
    pub result: AttachResult,
    output: mpsc::Receiver<AttachEvent>,
    input: mpsc::Sender<AttachInput>,
}

impl AttachSession {
    /// Builds a session from channels; the real client and fakes both use this.
    pub fn new(
        result: AttachResult,
        output: mpsc::Receiver<AttachEvent>,
        input: mpsc::Sender<AttachInput>,
    ) -> Self {
        Self {
            result,
            output,
            input,
        }
    }

    /// Splits the session so output can be pumped while input stays with the executor.
    pub fn split(
        self,
    ) -> (
        AttachResult,
        mpsc::Receiver<AttachEvent>,
        mpsc::Sender<AttachInput>,
    ) {
        (self.result, self.output, self.input)
    }
}

/// Everything the TUI asks of the daemon. Context.actor is always human here.
#[async_trait]
pub trait DaemonApi: Send + Sync {
    /// Lanes and units; the daemon picks the scope from the context (no lane or project).
    async fn status(&self) -> Result<Vec<LaneSummary>, ApiError>;
    /// Opens the event subscription.
    async fn subscribe(&self) -> Result<EventStream, ApiError>;
    /// Opens a terminal stream on `params.unit`.
    async fn attach(&self, lane: String, params: AttachParams) -> Result<AttachSession, ApiError>;
    /// `Start`, `Stop` or `Restart` for a lane or some of its units.
    async fn control(
        &self,
        method: Method,
        lane: String,
        selection: Selection,
    ) -> Result<Value, ApiError>;
    /// Runs a task in a lane; `yes` answers the task's `confirm` after the user did.
    async fn run_task(&self, lane: String, params: RunParams, yes: bool)
    -> Result<Value, ApiError>;
    /// Reads persisted log lines for deep scrollback.
    async fn logs(&self, lane: String, query: LogQuery) -> Result<Page<LogLine>, ApiError>;
    /// Resource samples over `query`'s range; empty when the daemon has none.
    async fn metrics(&self, query: MetricsQuery) -> Result<Metrics, ApiError>;
    /// `daemon.status`: version, pid, uptime, socket, managed processes.
    async fn daemon_status(&self) -> Result<Value, ApiError>;
    /// `doctor`: the daemon's raw facts; the TUI words them as checks.
    async fn doctor(&self) -> Result<Value, ApiError>;
    /// `lane.clean` for one lane. Sends `yes`, so call it only after the user confirmed.
    async fn clean_lane(&self, lane: String) -> Result<Value, ApiError>;
    /// `lane.gc`. Sends `yes`, so call it only after the user confirmed.
    async fn lane_gc(&self) -> Result<Value, ApiError>;
    /// Stops the daemon and everything it manages. Sends `yes`; confirm first.
    async fn stop_daemon(&self) -> Result<Value, ApiError>;
    /// Stops the daemon, waits for it to exit and starts a new one; returns
    /// the new daemon's status. Sends `yes`; confirm first.
    async fn restart_daemon(&self) -> Result<Value, ApiError>;
    /// Starts the daemon when none is running; returns its status.
    async fn start_daemon(&self) -> Result<Value, ApiError>;
    /// `profile.list` for a lane: the profiles its config defines.
    async fn profile_list(&self, lane: String) -> Result<ProfileList, ApiError>;
    /// `task.list` for a lane: every task its config defines, run or not.
    async fn task_list(&self, _lane: String) -> Result<Vec<TaskInfo>, ApiError> {
        Ok(Vec::new())
    }
    /// The daemon's own log file, read directly because the TUI runs on the
    /// same machine. `None` when there is no local file.
    fn daemon_log(&self) -> Option<PathBuf> {
        None
    }
    /// `DEVLANES_HOME` of the daemon this TUI talks to, where config.yaml and
    /// the dismissed notices live.
    fn home(&self) -> Option<PathBuf> {
        None
    }
    /// How this devlanes was installed and the release notices dismissed so
    /// far, for the update notice. Reads the binary's path and the home's
    /// notice log; called on a blocking thread.
    fn notices(&self) -> (devlanes_client::update::InstallMethod, Vec<String>) {
        let method = devlanes_client::update::InstallMethod::current();
        let dismissed = self
            .home()
            .map(|home| devlanes_client::update::NoticeLog::load(&home).dismissed)
            .unwrap_or_default();
        (method, dismissed)
    }
}

/// One profile from `profile.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    /// Unit selectors the profile starts (names, globs, `+tag`).
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// The profile sets `infra: false`.
    #[serde(default)]
    pub skips_infra: bool,
    #[serde(default)]
    pub is_default: bool,
}

/// One task from `task.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct TaskInfo {
    pub name: String,
    /// The lifecycle hook that runs it (`lane.start`, `lane.create`, ...).
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub steps: Option<Vec<String>>,
    /// The config asks before it runs.
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub interactive: bool,
    /// The newest run the daemon stored, across daemon restarts; `None`
    /// when the task never ran in this lane.
    #[serde(default)]
    pub last_run: Option<LastRun>,
}

/// A task's newest run from `task.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct LastRun {
    /// `running`, `done`, `failed` or `interrupted`.
    pub state: String,
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

impl TaskInfo {
    /// Runs as part of every start or the first one.
    pub fn runs_at_start(&self) -> bool {
        matches!(self.when.as_deref(), Some("lane.start" | "lane.create"))
    }
}

/// `task.list`: `{items: [TaskInfo]}`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TaskListResult {
    #[serde(default)]
    items: Vec<TaskInfo>,
}

/// `profile.list`: `{lane, items: [ProfileInfo]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ProfileList {
    #[serde(default)]
    pub lane: String,
    #[serde(default)]
    pub items: Vec<ProfileInfo>,
}

/// What one frame on an attach connection means for the stream.
#[derive(Debug, PartialEq)]
pub enum StreamFrame {
    Output(Bytes),
    /// The daemon's empty binary frame: the unit exited and the stream is closed.
    End,
    Failed(ApiError),
    /// Frames for other streams or control traffic the pump ignores.
    Other,
}

/// Classifies a frame read on an attach connection.
pub fn attach_frame(frame: devlanes_client::Result<Frame>, stream_id: u64) -> StreamFrame {
    match frame {
        Ok(Frame::Bytes {
            stream_id: id,
            data,
        }) if id == stream_id => {
            if data.is_empty() {
                StreamFrame::End
            } else {
                StreamFrame::Output(data)
            }
        }
        Ok(Frame::Control(Control::Response(response))) => match response.result {
            Outcome::Error { error } => StreamFrame::Failed(ApiError::Rpc(error)),
            Outcome::Ok { .. } => StreamFrame::Other,
        },
        Ok(_) => StreamFrame::Other,
        Err(error) => StreamFrame::Failed(ApiError::from(error)),
    }
}

/// Codes an older daemon answers for a method it does not know.
pub fn is_unsupported(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::Rpc(rpc) if matches!(
            rpc.code.as_str(),
            "not_supported" | "not_supported_yet" | "unsupported" | "unsupported_method" | "unknown_method"
        )
    )
}

/// The real implementation over `devlanes-client`.
///
/// Short requests share one connection. Subscriptions, attaches and long
/// actions (`start` may wait for infra) each use their own connection, so a
/// slow action never blocks status refreshes or typing.
pub struct ClientApi {
    paths: Paths,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    shared: Mutex<Option<Client>>,
}

impl ClientApi {
    /// Connects once, spawning the daemon if needed, so startup errors surface
    /// before the terminal switches to the alternate screen.
    pub async fn connect(paths: Paths) -> Result<Arc<Self>, devlanes_client::Error> {
        let client = Client::connect_or_spawn(&paths).await?;
        Ok(Arc::new(Self {
            paths,
            cwd: std::env::current_dir().unwrap_or_default(),
            // Captured once: start/run from the TUI use this shell's env as the lane base env.
            env: std::env::vars().collect(),
            shared: Mutex::new(Some(client)),
        }))
    }

    /// Builds a request context; only start and run carry the environment.
    fn context(&self, lane: Option<String>, with_env: bool) -> Context {
        Context {
            cwd: self.cwd.clone(),
            env: with_env.then(|| self.env.clone()),
            actor: Actor {
                kind: ActorKind::Human,
                agent_id: None,
                session_id: None,
            },
            lane,
            ..Context::default()
        }
    }

    /// Reconnects without spawning: a daemon stopped on purpose stays stopped.
    async fn fresh(&self) -> Result<Client, ApiError> {
        Ok(Client::connect(&self.paths).await?)
    }

    /// Sends a request on the shared connection, dropping it after transport errors.
    async fn shared_request(
        &self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<Value, ApiError> {
        let mut guard = self.shared.lock().await;
        if guard.is_none() {
            *guard = Some(self.fresh().await?);
        }
        let Some(client) = guard.as_mut() else {
            return Err(ApiError::Disconnected("no connection".into()));
        };
        match client.request(method, params, context).await {
            Ok(value) => Ok(value),
            Err(error) => {
                let error = ApiError::from(error);
                if error.is_disconnect() {
                    *guard = None;
                }
                Err(error)
            }
        }
    }

    /// Sends one request on its own connection.
    async fn oneshot(
        &self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<Value, ApiError> {
        let mut client = self.fresh().await?;
        Ok(client.request(method, params, context).await?)
    }
}

/// Serializes params; proto types always serialize, so a failure is a decode error.
fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|error| ApiError::Decode(error.to_string()))
}

/// Deserializes a response body into a proto type.
fn from_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|error| ApiError::Decode(error.to_string()))
}

#[async_trait]
impl DaemonApi for ClientApi {
    /// Calls `status`.
    async fn status(&self) -> Result<Vec<LaneSummary>, ApiError> {
        let value = self
            // The TUI shows every project, not just the cwd's (cli-design 5).
            .shared_request(
                Method::Status,
                json!({"all": true}),
                self.context(None, false),
            )
            .await?;
        let page: Page<LaneSummary> = from_value(value)?;
        Ok(page.items)
    }

    /// Calls `subscribe` on a dedicated connection and pumps its events.
    async fn subscribe(&self) -> Result<EventStream, ApiError> {
        let mut client = self.fresh().await?;
        let id = client
            .begin_request(Method::Subscribe, json!({}), self.context(None, false))
            .await?;
        let (sender, receiver) = mpsc::channel(EVENT_QUEUE);
        tokio::spawn(async move {
            let ending = loop {
                // A replaced subscription drops its stream; stop at once instead of
                // holding the connection until the next event fails to send.
                let frame = tokio::select! {
                    frame = client.next_frame() => frame,
                    () = sender.closed() => return,
                };
                match frame {
                    Ok(Frame::Control(Control::Event(event))) => {
                        if sender.send(Ok(event)).await.is_err() {
                            // The TUI dropped the stream; nobody is listening.
                            return;
                        }
                    }
                    // A terminal response ends the subscription; an error says why.
                    Ok(Frame::Control(Control::Response(response))) if response.id == id => {
                        match response.result {
                            Outcome::Ok { .. } => return,
                            Outcome::Error { error } => break ApiError::Rpc(error),
                        }
                    }
                    Ok(_) => {}
                    Err(error) => break ApiError::from(error),
                }
            };
            if sender.send(Err(ending)).await.is_err() {
                // The TUI dropped the stream first; there is nobody left to tell.
            }
        });
        Ok(EventStream::new(receiver))
    }

    /// Calls `attach` on a dedicated connection; resize goes on the same
    /// connection because stream ids are scoped to it.
    async fn attach(&self, lane: String, params: AttachParams) -> Result<AttachSession, ApiError> {
        let mut client = self.fresh().await?;
        let context = self.context(Some(lane), false);
        let value = client
            .request(Method::Attach, to_value(&params)?, context.clone())
            .await?;
        let result: AttachResult = from_value(value)?;
        let stream_id = result.stream_id;
        let (output_tx, output_rx) = mpsc::channel(OUTPUT_QUEUE);
        let (input_tx, mut input_rx) = mpsc::channel(INPUT_QUEUE);
        tokio::spawn(async move {
            // One Step lives on the stack per loop iteration; boxing buys nothing.
            #[allow(clippy::large_enum_variant)]
            enum Step {
                Input(Option<AttachInput>),
                Frame(devlanes_client::Result<Frame>),
            }
            // Why the stream ended; `None` for the daemon's normal end of stream.
            let ending: Option<ApiError> = loop {
                // Both receives are cancel-safe, so losing the race drops nothing.
                let step = tokio::select! {
                    input = input_rx.recv() => Step::Input(input),
                    frame = client.next_frame() => Step::Frame(frame),
                };
                match step {
                    // The TUI detached; dropping the connection is the detach.
                    Step::Input(None) => return,
                    Step::Input(Some(AttachInput::Bytes(data))) => {
                        if let Err(error) = client.send_bytes(stream_id, data).await {
                            break Some(ApiError::from(error));
                        }
                    }
                    Step::Input(Some(AttachInput::Resize { columns, rows })) => {
                        let params = ResizeParams {
                            stream_id,
                            columns,
                            rows,
                        };
                        let params = match serde_json::to_value(params) {
                            Ok(params) => params,
                            Err(error) => break Some(ApiError::Decode(error.to_string())),
                        };
                        match client
                            .request(Method::Resize, params, context.clone())
                            .await
                        {
                            Ok(_) => {}
                            // A refused resize leaves the stream usable; report and go on.
                            Err(devlanes_client::Error::Rpc(error)) => {
                                let event = AttachEvent::ResizeRefused(ApiError::Rpc(error));
                                if output_tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                            Err(error) => break Some(ApiError::from(error)),
                        }
                    }
                    Step::Frame(frame) => match attach_frame(frame, stream_id) {
                        StreamFrame::Output(data) => {
                            if output_tx.send(AttachEvent::Output(data)).await.is_err() {
                                return;
                            }
                        }
                        StreamFrame::Other => {}
                        StreamFrame::End => break None,
                        StreamFrame::Failed(error) => break Some(error),
                    },
                }
            };
            // Closing the output channel tells the TUI the stream ended.
            if let Some(error) = ending
                && output_tx.send(AttachEvent::Error(error)).await.is_err()
            {
                // The TUI already detached; there is nobody left to tell.
            }
        });
        Ok(AttachSession::new(result, output_rx, input_tx))
    }

    /// Calls `start`, `stop` or `restart` on a dedicated connection.
    async fn control(
        &self,
        method: Method,
        lane: String,
        selection: Selection,
    ) -> Result<Value, ApiError> {
        // Restart reuses the lane's stored env (plan decision 7); start supplies this shell's.
        let with_env = method == Method::Start;
        self.oneshot(
            method,
            to_value(&selection)?,
            self.context(Some(lane), with_env),
        )
        .await
    }

    /// Calls `run` on a dedicated connection.
    async fn run_task(
        &self,
        lane: String,
        params: RunParams,
        yes: bool,
    ) -> Result<Value, ApiError> {
        let mut context = self.context(Some(lane), true);
        context.yes = yes;
        self.oneshot(Method::Run, to_value(&params)?, context).await
    }

    /// Calls `logs` on the shared connection.
    async fn logs(&self, lane: String, query: LogQuery) -> Result<Page<LogLine>, ApiError> {
        let value = self
            .shared_request(
                Method::Logs,
                to_value(&query)?,
                self.context(Some(lane), false),
            )
            .await?;
        from_value(value)
    }

    /// Calls `metrics` on the shared connection.
    async fn metrics(&self, query: MetricsQuery) -> Result<Metrics, ApiError> {
        let params = json!({
            "range_ms": query.range_ms,
            "points": query.points,
            "windows": true,
        });
        let value = self
            .shared_request(Method::Metrics, params, self.context(None, false))
            .await?;
        Ok(Metrics::parse(&value))
    }

    /// Calls `daemon.status` on the shared connection.
    async fn daemon_status(&self) -> Result<Value, ApiError> {
        self.shared_request(Method::DaemonStatus, json!({}), self.context(None, false))
            .await
    }

    /// Calls `doctor` on the shared connection.
    async fn doctor(&self) -> Result<Value, ApiError> {
        self.shared_request(Method::Doctor, json!({}), self.context(None, false))
            .await
    }

    /// Calls `lane.clean` on its own connection; cleaning waits for teardown.
    async fn clean_lane(&self, lane: String) -> Result<Value, ApiError> {
        let mut context = self.context(Some(lane.clone()), false);
        context.yes = true;
        self.oneshot(Method::LaneClean, json!({"lane": lane}), context)
            .await
    }

    /// Calls `lane.gc` on its own connection.
    async fn lane_gc(&self) -> Result<Value, ApiError> {
        let mut context = self.context(None, false);
        context.yes = true;
        self.oneshot(Method::LaneGc, json!({}), context).await
    }

    /// Calls `daemon.shutdown`; the shared connection dies with the daemon.
    async fn stop_daemon(&self) -> Result<Value, ApiError> {
        let mut context = self.context(None, false);
        context.yes = true;
        let result = self
            .oneshot(Method::DaemonShutdown, json!({}), context)
            .await;
        *self.shared.lock().await = None;
        result
    }

    /// Shutdown, then a new daemon from this binary that starts again what
    /// ran, as `devlanes daemon restart` does.
    async fn restart_daemon(&self) -> Result<Value, ApiError> {
        // The new daemon refuses an invalid user config, so check it before the old
        // one stops every lane, as `devlanes daemon restart` does.
        let path = self.paths.home.join("config.yaml");
        let checked = tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Ok(());
            }
            devlanes_config::load_user(&path)
                .map(drop)
                .map_err(|error| {
                    RpcError::new(
                        "config_error",
                        json!({
                            "message": error.message,
                            "file": path.display().to_string(),
                            "line": error.location.line,
                            "column": error.location.column,
                        }),
                        1,
                    )
                })
        })
        .await
        .map_err(|error| ApiError::Disconnected(error.to_string()))?;
        checked.map_err(ApiError::Rpc)?;
        // What runs now; a failure to read it only means nothing is resumed
        // (the restart itself still goes ahead, as in the CLI).
        let running = match self.fresh().await {
            Ok(mut client) => client.running_lanes().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        self.stop_daemon().await?;
        // The old daemon releases its socket only after stopping everything.
        let mut released = false;
        for _ in 0..200 {
            if Client::connect(&self.paths).await.is_err() {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if !released {
            return Err(ApiError::Rpc(RpcError::new(
                "timeout",
                json!({"message": "the old daemon did not exit within 10 seconds"}),
                1,
            )));
        }
        let mut status = self.start_daemon().await?;
        if !running.is_empty() {
            let context = self.context(None, true);
            let mut client = Client::connect_or_spawn(&self.paths).await?;
            let resumed = client
                .resume_running(&running, &context.env.unwrap_or_default(), &context.actor)
                .await;
            // The starts' results ride along; lanes report their progress
            // through events like any start.
            if status.is_object() {
                status["resumed"] = json!(resumed);
            }
        }
        Ok(status)
    }

    /// Spawns the daemon if needed and keeps the connection as the shared one.
    async fn start_daemon(&self) -> Result<Value, ApiError> {
        let mut client = Client::connect_or_spawn(&self.paths).await?;
        let status = client
            .request(Method::DaemonStatus, json!({}), self.context(None, false))
            .await?;
        *self.shared.lock().await = Some(client);
        Ok(status)
    }

    /// Calls `profile.list` on the shared connection; the lane travels in the context.
    async fn profile_list(&self, lane: String) -> Result<ProfileList, ApiError> {
        let value = self
            .shared_request(
                Method::ProfileList,
                json!({}),
                self.context(Some(lane), false),
            )
            .await?;
        from_value(value)
    }

    async fn task_list(&self, lane: String) -> Result<Vec<TaskInfo>, ApiError> {
        let value = self
            .shared_request(Method::TaskList, json!({}), self.context(Some(lane), false))
            .await?;
        from_value::<TaskListResult>(value).map(|list| list.items)
    }

    /// `$DEVLANES_HOME/logs/daemon.log`.
    fn daemon_log(&self) -> Option<PathBuf> {
        Some(self.paths.logs.join("daemon.log"))
    }

    /// The home the client resolved, explicit or default.
    fn home(&self) -> Option<PathBuf> {
        Some(self.paths.home.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_parse_tolerates_missing_and_partial_data() {
        assert_eq!(Metrics::parse(&Value::Null), Metrics::default());
        assert_eq!(Metrics::parse(&json!({})), Metrics::default());
        let parsed = Metrics::parse(&json!({"units": [
            {"lane": "acme@0", "unit": "api", "cpu_percent": 12.5, "memory_bytes": 1024,
             "history": [[1, 1.0, 10], [2, 2.5, 20], ["bad"]]},
            {"lane": "acme@0"},
            {"lane": "acme@0", "unit": "web"}
        ]}));
        assert_eq!(parsed.units.len(), 2);
        assert_eq!(parsed.units[0].history, vec![(1, 1.0, 10), (2, 2.5, 20)]);
        assert_eq!(parsed.units[1].cpu_percent, None);
        assert_eq!(parsed.system, None);
    }

    #[test]
    fn metrics_parse_reads_containers_lanes_windows_and_the_system() {
        let parsed = Metrics::parse(&json!({
            "range_ms": 900000,
            "units": [{"lane": "acme (shared)", "unit": "postgres", "kind": "container",
                       "container": "acme-postgres-1", "shared": true, "lanes": ["acme@0", "acme@1"],
                       "cpu_percent": 6.0, "memory_bytes": 598000000.0,
                       "windows": [[1, 0.5, 2.0, 9.0, 10, 20, 30], [2, "bad"]]}],
            "lanes": [{"lane": "acme@0", "cpu_percent": 395.0, "memory_bytes": 7000,
                       "peak_cpu_percent": 410.0, "history": [[1, 300.0, 6000]]}],
            "system": {"cpus": 10, "memory_total_bytes": 34359738368u64, "cpu_percent": 80.0,
                       "memory_pressure": "normal", "history": [[1, 70.0, 9000]],
                       "core_percents": [12.5, 90.0, "bad"]}
        }));
        assert_eq!(parsed.range_ms, Some(900_000));
        let postgres = &parsed.units[0];
        assert!(postgres.is_container() && postgres.shared);
        assert_eq!(postgres.lanes, ["acme@0", "acme@1"]);
        assert_eq!(postgres.memory_bytes, Some(598_000_000));
        assert_eq!(postgres.windows.len(), 1);
        assert_eq!(postgres.windows[0].cpu_max, 9.0);
        assert_eq!(parsed.lanes[0].peak_cpu_percent, Some(410.0));
        let system = parsed
            .system
            .as_ref()
            .map(|s| (s.cpus, s.memory_pressure.as_deref()));
        assert_eq!(system, Some((Some(10), Some("normal"))));
        assert_eq!(
            parsed.system.map(|s| s.core_percents),
            Some(vec![12.5, 90.0])
        );
    }

    #[test]
    fn an_empty_binary_frame_ends_the_stream() {
        let bytes = |id, data: &'static [u8]| {
            Ok(Frame::Bytes {
                stream_id: id,
                data: Bytes::from_static(data),
            })
        };
        assert_eq!(
            attach_frame(bytes(7, b"hello"), 7),
            StreamFrame::Output(Bytes::from_static(b"hello"))
        );
        assert_eq!(attach_frame(bytes(7, b""), 7), StreamFrame::End);
        // Another stream's frames, empty or not, are not this stream's end.
        assert_eq!(attach_frame(bytes(8, b""), 7), StreamFrame::Other);
        assert!(matches!(
            attach_frame(Err(devlanes_client::Error::Timeout), 7),
            StreamFrame::Failed(ApiError::Disconnected(_))
        ));
    }
}
