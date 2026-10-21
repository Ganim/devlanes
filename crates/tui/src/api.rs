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
#[derive(Debug, Clone, thiserror::Error)]
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

/// One unit's resource usage from `Method::Metrics`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitMetrics {
    pub lane: String,
    pub unit: String,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    /// `(ts_ms, cpu_percent, memory_bytes)`, oldest first.
    pub history: Vec<(u64, f64, u64)>,
}

impl UnitMetrics {
    /// Parses `{units: [{lane, unit, cpu_percent, memory_bytes, history}]}` leniently:
    /// a daemon without metrics yet returns `{}` or `null`, which is an empty list.
    pub fn parse_list(value: &Value) -> Vec<Self> {
        let Some(units) = value.get("units").and_then(Value::as_array) else {
            return Vec::new();
        };
        units
            .iter()
            .filter_map(|unit| {
                let history = unit
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
                    .unwrap_or_default();
                Some(Self {
                    lane: unit.get("lane")?.as_str()?.to_owned(),
                    unit: unit.get("unit")?.as_str()?.to_owned(),
                    cpu_percent: unit.get("cpu_percent").and_then(Value::as_f64),
                    memory_bytes: unit
                        .get("memory_bytes")
                        .and_then(Value::as_f64)
                        .map(|bytes| bytes as u64),
                    history,
                })
            })
            .collect()
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
    Error(ApiError),
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
    /// Runs a task in a lane.
    async fn run_task(&self, lane: String, params: RunParams) -> Result<Value, ApiError>;
    /// Reads persisted log lines for deep scrollback.
    async fn logs(&self, lane: String, query: LogQuery) -> Result<Page<LogLine>, ApiError>;
    /// Resource samples; an empty list when the daemon has none.
    async fn metrics(&self) -> Result<Vec<UnitMetrics>, ApiError>;
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
    /// The daemon's own log file, read directly because the TUI runs on the
    /// same machine. `None` when there is no local file.
    fn daemon_log(&self) -> Option<PathBuf> {
        None
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

/// `profile.list`: `{lane, items: [ProfileInfo]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ProfileList {
    #[serde(default)]
    pub lane: String,
    #[serde(default)]
    pub items: Vec<ProfileInfo>,
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
                match client.next_frame().await {
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
            // The reason the stream ended, shown by the UI.
            let ending: ApiError = loop {
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
                            break ApiError::from(error);
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
                            Err(error) => break ApiError::Decode(error.to_string()),
                        };
                        match client
                            .request(Method::Resize, params, context.clone())
                            .await
                        {
                            Ok(_) => {}
                            // A refused resize leaves the stream usable; report and go on.
                            Err(devlanes_client::Error::Rpc(error)) => {
                                let event = AttachEvent::Error(ApiError::Rpc(error));
                                if output_tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                            Err(error) => break ApiError::from(error),
                        }
                    }
                    Step::Frame(Ok(Frame::Bytes {
                        stream_id: id,
                        data,
                    })) if id == stream_id => {
                        if output_tx.send(AttachEvent::Output(data)).await.is_err() {
                            return;
                        }
                    }
                    Step::Frame(Ok(Frame::Control(Control::Response(response)))) => {
                        if let Outcome::Error { error } = response.result {
                            break ApiError::Rpc(error);
                        }
                    }
                    Step::Frame(Ok(_)) => {}
                    Step::Frame(Err(error)) => break ApiError::from(error),
                }
            };
            if output_tx.send(AttachEvent::Error(ending)).await.is_err() {
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
    async fn run_task(&self, lane: String, params: RunParams) -> Result<Value, ApiError> {
        self.oneshot(
            Method::Run,
            to_value(&params)?,
            self.context(Some(lane), true),
        )
        .await
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
    async fn metrics(&self) -> Result<Vec<UnitMetrics>, ApiError> {
        let value = self
            .shared_request(Method::Metrics, json!({}), self.context(None, false))
            .await?;
        Ok(UnitMetrics::parse_list(&value))
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

    /// Shutdown, then a new daemon from this binary, as `devlanes daemon restart` does.
    async fn restart_daemon(&self) -> Result<Value, ApiError> {
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
        self.start_daemon().await
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

    /// `$DEVLANES_HOME/logs/daemon.log`.
    fn daemon_log(&self) -> Option<PathBuf> {
        Some(self.paths.logs.join("daemon.log"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_parse_tolerates_missing_and_partial_data() {
        assert!(UnitMetrics::parse_list(&Value::Null).is_empty());
        assert!(UnitMetrics::parse_list(&json!({})).is_empty());
        let parsed = UnitMetrics::parse_list(&json!({"units": [
            {"lane": "acme@0", "unit": "api", "cpu_percent": 12.5, "memory_bytes": 1024,
             "history": [[1, 1.0, 10], [2, 2.5, 20], ["bad"]]},
            {"lane": "acme@0"},
            {"lane": "acme@0", "unit": "web"}
        ]}));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].history, vec![(1, 1.0, 10), (2, 2.5, 20)]);
        assert_eq!(parsed[1].cpu_percent, None);
    }
}
