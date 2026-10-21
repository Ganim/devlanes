//! Data shared by daemon and clients; presentation belongs to the clients.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Changes only when an older peer cannot safely interpret this protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Bounds allocation before decoding an untrusted local or tunneled frame.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Negotiation remains decodable even when ordinary request schemas change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub build_id: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub managed_processes: usize,
}

impl Hello {
    /// Uses the build identifier supplied at release time, falling back for local builds.
    pub fn current(managed_processes: usize) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            build_id: option_env!("DEVLANES_BUILD_ID").unwrap_or("local").into(),
            features: Vec::new(),
            managed_processes,
        }
    }
}

/// Attribution is supplied by clients and never inferred by the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub kind: ActorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// The daemon stores this classification without applying presentation policy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    #[default]
    Human,
    Agent,
}

/// Per-request context must not be logged or persisted: it can contain secrets.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Context {
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub actor: Actor,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub yes: bool,
}

/// Stable method names allow clients to distinguish unsupported operations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Method {
    #[serde(rename = "daemon.status")]
    DaemonStatus,
    #[serde(rename = "daemon.shutdown")]
    DaemonShutdown,
    #[serde(rename = "daemon.reset-state")]
    DaemonResetState,
    #[serde(rename = "doctor")]
    Doctor,
    #[serde(rename = "config.check")]
    ConfigCheck,
    #[serde(rename = "schema")]
    Schema,
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "restart")]
    Restart,
    #[serde(rename = "resume")]
    Resume,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "lane.list")]
    LaneList,
    #[serde(rename = "lane.new")]
    LaneNew,
    #[serde(rename = "lane.clean")]
    LaneClean,
    #[serde(rename = "lane.gc")]
    LaneGc,
    #[serde(rename = "exec")]
    Exec,
    #[serde(rename = "run")]
    Run,
    #[serde(rename = "task.list")]
    TaskList,
    #[serde(rename = "logs")]
    Logs,
    #[serde(rename = "wait")]
    Wait,
    #[serde(rename = "attach")]
    Attach,
    #[serde(rename = "input")]
    Input,
    #[serde(rename = "resize")]
    Resize,
    #[serde(rename = "infra.up")]
    InfraUp,
    #[serde(rename = "infra.down")]
    InfraDown,
    #[serde(rename = "infra.status")]
    InfraStatus,
    #[serde(rename = "hosts")]
    Hosts,
    #[serde(rename = "env")]
    Env,
    #[serde(rename = "errors")]
    Errors,
    #[serde(rename = "why")]
    Why,
    #[serde(rename = "url")]
    Url,
    #[serde(rename = "metrics")]
    Metrics,
    #[serde(rename = "subscribe")]
    Subscribe,
    #[serde(rename = "sync.status")]
    SyncStatus,
    #[serde(rename = "connect")]
    Connect,
    #[serde(rename = "disconnect")]
    Disconnect,
    #[serde(rename = "host.list")]
    HostList,
    #[serde(rename = "unlock")]
    Unlock,
}

/// Context is deliberately not `Debug` to prevent accidental environment traces.
#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: Method,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub context: Context,
}

/// Structured errors are rendered with wording and hints by the receiving client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: String,
    #[serde(default)]
    pub data: Value,
    pub exit_code: u8,
}

impl RpcError {
    /// Keeps failure details machine-readable without embedding CLI instructions.
    pub fn new(code: impl Into<String>, data: Value, exit_code: u8) -> Self {
        Self {
            code: code.into(),
            data,
            exit_code,
        }
    }
}

/// One terminal response belongs to exactly one request identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub result: Outcome,
}

/// The tagged outcome avoids ambiguous combinations of result and error fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok { data: Value },
    Error { error: RpcError },
}

/// Events are sequenced so a subscriber can request a fresh state after a gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

/// JSON control frames; PTY bytes use a separate binary frame kind.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum Control {
    Hello(Hello),
    Request(Request),
    Response(Response),
    Event(Event),
}

/// List cursors are opaque to clients and scoped to the requested collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// Marks a collection complete when no backing items were omitted.
    pub fn complete(items: Vec<T>) -> Self {
        Self {
            items,
            truncated: false,
            next_cursor: None,
        }
    }
}

/// Shared selectors and start options, independent of CLI argument syntax.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Selection {
    #[serde(default)]
    pub units: Vec<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub mine: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Ad-hoc commands are argv to avoid an additional implicit shell interpretation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecParams {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub pty: bool,
}

/// Existing clients default to terminal-backed units when the field is absent.
fn default_true() -> bool {
    true
}

/// Limits apply before transfer; clients may apply tighter presentation budgets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    pub units: Vec<String>,
    pub tail: Option<usize>,
    pub max_bytes: Option<usize>,
    pub cursor: Option<String>,
    pub since: Option<String>,
    pub grep: Option<String>,
    pub level: Option<String>,
    pub around: Option<String>,
    #[serde(default)]
    pub context_lines: usize,
    #[serde(default)]
    pub follow: bool,
    #[serde(default)]
    pub mine: bool,
}

/// Persistent log sequence numbers remain useful after reconnect and rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub unit: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub text: String,
    pub level: Option<String>,
}

/// Public service state is separate from an OS-specific process handle.
/// States: `idle`, `starting`, `ready`, `failed`, `stopping`, `stopped`, `exited`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitSummary {
    pub name: String,
    pub lane: String,
    pub state: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub ports: BTreeMap<String, u16>,
    pub urls: Vec<String>,
    pub actor: Actor,
    pub reason: Option<RpcError>,
    /// `service`, `task` or `exec`; clients group and filter on it.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    /// Filled by the metrics sampler when it has a recent sample for the unit.
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
}

/// Canonical lane references can be pasted into a later command.
/// States: `idle`, `starting`, `running`, `failed`, `stopping`, `unassigned`, `no-config`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneSummary {
    pub reference: String,
    pub project: String,
    pub index: Option<u32>,
    pub branch: Option<String>,
    pub path: PathBuf,
    pub state: String,
    pub config_changed: bool,
    pub units: Vec<UnitSummary>,
    #[serde(default)]
    pub profile: Option<String>,
    /// Set when the worktree changed branch and will change lane at its next start.
    #[serde(default)]
    pub moves_to: Option<u32>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub port_block: Option<(u16, u16)>,
    /// Machine-readable reason for `unassigned`/`no-config` or a start failure.
    #[serde(default)]
    pub reason: Option<RpcError>,
    /// True when the lane's client environment is gone (daemon restart) and a
    /// `start`/`resume` from a shell is required before services can run.
    #[serde(default)]
    pub needs_env: bool,
}

/// Opens a terminal stream. The daemon answers with [`AttachResult`], then sends a
/// screen snapshot followed by live output as binary frames on `stream_id`. Input
/// travels back as binary frames on the same id. An empty binary frame from the
/// daemon means the unit exited and the stream is closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachParams {
    pub unit: String,
    /// Interactive viewers take terminal size ownership; observers (agents, previews) never resize.
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub columns: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// Stream ids are unique per connection; closing the connection detaches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub stream_id: u64,
    pub unit: String,
    pub lane: String,
}

/// Resizes an attached stream; ignored unless the stream is the active interactive viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizeParams {
    pub stream_id: u64,
    pub columns: u16,
    pub rows: u16,
}

/// Task invocation through `devlanes run`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunParams {
    pub task: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub detach: bool,
    /// Present when the client has a terminal for an `interactive: true` task.
    #[serde(default)]
    pub terminal: bool,
}

/// Final `start`/`restart`/`resume` result, after every started unit is ready or failed
/// (or the timeout passed). Ownership fields let `devlanes with` stop only what it started.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartResult {
    /// Canonical lane reference, e.g. `acme@2`.
    pub lane: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Units this request spawned, as canonical `lane:unit` references.
    #[serde(default)]
    pub started: Vec<String>,
    /// Selected units that were already running and left untouched.
    #[serde(default)]
    pub already_running: Vec<String>,
    /// True when the lane was idle and this request brought it up.
    #[serde(default)]
    pub whole_lane: bool,
    pub ready: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub failures: Vec<UnitFailureData>,
    /// After-services tasks skipped because a service failed.
    #[serde(default)]
    pub skipped_tasks: Vec<String>,
    #[serde(default)]
    pub units: Vec<UnitSummary>,
}

/// A failed unit or task with the data clients need for annotations and hints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitFailureData {
    pub lane: String,
    pub unit: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Last ≤20 ANSI-stripped lines.
    #[serde(default)]
    pub log_tail: Vec<String>,
    #[serde(default)]
    pub reason: Option<RpcError>,
}

/// Per-unit peaks for the current lane start, for CI summaries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitPeak {
    pub unit: String,
    pub peak_memory_bytes: Option<u64>,
    pub peak_cpu_percent: Option<f64>,
}

/// Event names. Payloads are JSON objects documented next to each constant.
pub mod events {
    /// `{lane: LaneSummary}` whenever lane state, membership or config status changes.
    pub const LANE_CHANGED: &str = "lane.changed";
    /// `{lane: reference}` after a lane and its records are removed.
    pub const LANE_REMOVED: &str = "lane.removed";
    /// `{unit: UnitSummary}` on every unit state transition.
    pub const UNIT_CHANGED: &str = "unit.changed";
    /// `{lane, phase, state: started|done|failed|skipped, reason?}` for start pipeline progress.
    pub const PHASE: &str = "lane.phase";
    /// `{lane, task, state, exit_code?}` for task runs.
    pub const TASK: &str = "task";
    /// `{unit, line: LogLine}` for followed logs.
    pub const LOG: &str = "log";
    /// `{lane, unit?}` sent once when a waited-for target is ready or failed.
    pub const RESULT: &str = "result";
    /// Sent instead of dropped events after a subscriber overflowed; clients must re-query.
    pub const RESYNC: &str = "resync";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_method_stays_frozen() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_string(&Method::DaemonShutdown)?,
            "\"daemon.shutdown\""
        );
        Ok(())
    }

    #[test]
    fn hello_accepts_additive_fields() -> Result<(), serde_json::Error> {
        let hello: Hello = serde_json::from_value(serde_json::json!({
            "protocol": 1, "version": "next", "build_id": "hash", "extra": true
        }))?;
        assert_eq!(hello.managed_processes, 0);
        assert!(hello.features.is_empty());
        Ok(())
    }
}
