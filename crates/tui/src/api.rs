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

/// Live subscription events; ends when the subscription connection closes.
pub struct EventStream {
    events: mpsc::Receiver<Event>,
}

impl EventStream {
    /// Wraps a channel; fakes push events into the sending half.
    pub fn new(events: mpsc::Receiver<Event>) -> Self {
        Self { events }
    }

    /// Waits for the next event; `None` means the subscription is gone.
    pub async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

/// Requests sent from the UI to an attached terminal stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachInput {
    Bytes(Bytes),
    Resize { columns: u16, rows: u16 },
}

/// An open terminal stream. Dropping it detaches; the unit keeps running.
pub struct AttachSession {
    pub result: AttachResult,
    output: mpsc::Receiver<Bytes>,
    input: mpsc::Sender<AttachInput>,
}

impl AttachSession {
    /// Builds a session from channels; the real client and fakes both use this.
    pub fn new(
        result: AttachResult,
        output: mpsc::Receiver<Bytes>,
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
        mpsc::Receiver<Bytes>,
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
            loop {
                match client.next_frame().await {
                    Ok(Frame::Control(Control::Event(event))) => {
                        if sender.send(event).await.is_err() {
                            break;
                        }
                    }
                    // A terminal response ends the subscription, whether ok or error.
                    Ok(Frame::Control(Control::Response(response))) if response.id == id => {
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
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
            loop {
                // Both receives are cancel-safe, so losing the race drops nothing.
                let step = tokio::select! {
                    input = input_rx.recv() => Step::Input(input),
                    frame = client.next_frame() => Step::Frame(frame),
                };
                match step {
                    Step::Input(None) => break,
                    Step::Input(Some(AttachInput::Bytes(data))) => {
                        if client.send_bytes(stream_id, data).await.is_err() {
                            break;
                        }
                    }
                    Step::Input(Some(AttachInput::Resize { columns, rows })) => {
                        let params = ResizeParams {
                            stream_id,
                            columns,
                            rows,
                        };
                        let Ok(params) = serde_json::to_value(params) else {
                            continue;
                        };
                        // Observers may be refused; the stream stays usable either way.
                        match client
                            .request(Method::Resize, params, context.clone())
                            .await
                        {
                            Ok(_) | Err(devlanes_client::Error::Rpc(_)) => {}
                            Err(_) => break,
                        }
                    }
                    Step::Frame(Ok(Frame::Bytes {
                        stream_id: id,
                        data,
                    })) if id == stream_id => {
                        if output_tx.send(data).await.is_err() {
                            break;
                        }
                    }
                    Step::Frame(Ok(Frame::Control(Control::Response(response))))
                        if matches!(response.result, Outcome::Error { .. }) =>
                    {
                        break;
                    }
                    Step::Frame(Ok(_)) => {}
                    Step::Frame(Err(_)) => break,
                }
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
