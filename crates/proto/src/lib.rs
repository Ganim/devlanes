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
    /// Profiles of the lane's project: `{lane, items: [ProfileSummary]}`.
    #[serde(rename = "profile.list")]
    ProfileList,
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
    /// Sent by `devlanes certs trust --hosts` and `certs untrust --hosts` after changing the
    /// helper: the daemon drops its sudo backoff and reapplies `/etc/hosts` now.
    #[serde(rename = "hosts.retry")]
    HostsRetry,
    /// Any method name this build does not know, so a request from a newer client
    /// decodes and gets an `unknown_method` answer instead of a dropped connection.
    #[serde(rename = "unknown", other)]
    Unknown,
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
#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum Control {
    Hello(Hello),
    Request(Request),
    Response(Response),
    Event(Event),
    /// A frame type this build does not know; receivers skip it instead of
    /// closing the connection.
    Unknown,
}

impl<'de> Deserialize<'de> for Control {
    /// `{type, body}` like the derived form, except that an unknown `type` (with any
    /// body) becomes [`Control::Unknown`]; serde's `other` only takes bodiless variants.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            body: Value,
        }
        let raw = Raw::deserialize(deserializer)?;
        let body = raw.body;
        Ok(match raw.kind.as_str() {
            "hello" => Self::Hello(serde_json::from_value(body).map_err(D::Error::custom)?),
            "request" => Self::Request(serde_json::from_value(body).map_err(D::Error::custom)?),
            "response" => Self::Response(serde_json::from_value(body).map_err(D::Error::custom)?),
            "event" => Self::Event(serde_json::from_value(body).map_err(D::Error::custom)?),
            _ => Self::Unknown,
        })
    }
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
    /// Single profile from clients that predate `profiles`; counts as the first entry.
    #[serde(default)]
    pub profile: Option<String>,
    /// Profiles to start together, in the order given (later env wins).
    ///
    /// With neither `profile` nor `profiles`, `start`, `restart` and `resume` keep
    /// the lane's profiles from its previous start. To go back to the default,
    /// send [`DEFAULT_PROFILE`]: it selects the config's `default` profile, or
    /// every `autostart` service when the config defines none.
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub mine: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Start every selected service at once, bypassing the start gate for this
    /// request only (`start --all-at-once`).
    #[serde(default)]
    pub all_at_once: bool,
}

/// Profile name that resets a lane to its default selection (see [`Selection::profiles`]).
pub const DEFAULT_PROFILE: &str = "default";

impl Selection {
    /// Requested profiles: `profile` first, then `profiles`, without repeats and
    /// blank names. Empty means "keep the lane's previous profiles".
    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for name in self.profile.iter().chain(&self.profiles) {
            if !name.trim().is_empty() && !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
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
/// States: `idle`, `queued`, `starting`, `ready`, `failed`, `stopping`, `stopped`, `exited`.
/// `queued` units wait for a start slot; their `reason` has code `queued` with
/// `{position, queued, in_flight, window, hold, memory_need_bytes,
/// available_memory_bytes?, cpu_percent?, memory_pressure?}`; `hold` is
/// `window_full`, `memory_critical` or `memory` (waiting for memory to fit).
///
/// `orphaned`: the process was started by a previous daemon and still runs (its
/// identity verified). It is not supervised: no output, attach or readiness. `pid`,
/// `started_at_ms` (its original start) and `orphaned_since_ms` are set; its lane
/// reads `running`. `stop`, lane stop, `restart` and `lane clean` terminate it;
/// `start` refuses it with `unit_orphaned`. A stop result lists a terminated
/// orphan as `stopped` and keeps `orphaned_since_ms`, so clients can say so.
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
    /// Palette name from `devlanes_core::color::unit_color`; clients reserve red
    /// for failures. Absent from older daemons; omitted from JSON when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    /// Filled by the metrics sampler when it has a recent sample for the unit.
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// When this daemon found the process left running by a previous one (Unix
    /// ms); set only for `orphaned` units and orphans a stop just terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_since_ms: Option<u64>,
}

/// Unit state of a process left running by a previous daemon (see [`UnitSummary`]).
pub const ORPHANED: &str = "orphaned";

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
    /// First entry of `profiles`, for clients that show one profile.
    #[serde(default)]
    pub profile: Option<String>,
    /// Profiles of the lane's last start, in the order given.
    #[serde(default)]
    pub profiles: Vec<String>,
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
    /// Started containers of the lane with their host ports; connected clients mirror
    /// the ports of the non-hidden ones.
    #[serde(default)]
    pub containers: Vec<ContainerSummary>,
}

/// A container a lane uses (shared or per-lane) and its published host ports.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerSummary {
    pub name: String,
    pub ports: BTreeMap<String, u16>,
    /// Listed in `infra.hidden`: never mirrored to connected clients.
    #[serde(default)]
    pub hidden: bool,
    /// Palette name, as for [`UnitSummary::color`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
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

/// Messages a client daemon exchanges with a box's `devlanes tunnel` over the remote
/// mux. Remote is mirroring only: these are sync payloads and connection events,
/// never daemon requests, environments or secrets.
pub mod remote {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    /// Event sent to the CLI attached to `connect` when ssh asks for input:
    /// `{host, prompt, kind: secret|confirm|notify, stream_id}`. The CLI answers with
    /// one binary frame on `stream_id`; an empty frame cancels.
    pub const PROMPT_EVENT: &str = "remote.prompt";
    /// `{host, state, ...}` whenever a connection changes state.
    pub const STATE_EVENT: &str = "remote.state";
    /// `{host, code, ...}` for mirroring problems such as a skipped lane or a busy port.
    pub const WARNING_EVENT: &str = "remote.warning";

    /// Largest synced file. The box skips (and reports) bigger ones before reading
    /// them, and the client refuses bigger file headers.
    pub const MAX_SYNC_FILE: u64 = 256 * 1024 * 1024;

    /// A `services.<name>.sync` entry of a running lane, as listed by `sync.status`.
    ///
    /// The client always writes to its own `$DEVLANES_HOME/sync/<host>/<project>/
    /// <lane>/<leaf>`; the box's `to` never travels.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct SyncEntry {
        /// Canonical lane reference, `project@index`.
        pub lane: String,
        pub project: String,
        pub unit: String,
        /// Position within the service's `sync` list.
        pub index: usize,
        /// Source folder on the box, for display.
        pub from: PathBuf,
        /// Last component of `from`, the destination folder's name.
        pub leaf: String,
        #[serde(default)]
        pub ignore: Vec<String>,
    }

    /// One file the client already holds, so the box sends only what differs.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct SyncFile {
        /// `/`-separated path below the destination.
        pub path: String,
        pub size: u64,
        /// Permission bits.
        pub mode: u32,
        /// Lowercase hex SHA-256 of the contents.
        pub hash: String,
        /// Modification time on the client, kept for its own hash cache.
        #[serde(default)]
        pub mtime_ms: Option<u64>,
    }

    /// Sync subscription request. The box looks entries up itself and never takes
    /// paths from the client.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum SyncRequest {
        /// Answered with one `entries` manifest, then the stream ends.
        List,
        /// Streams one entry: what differs from `have`, then changes.
        Follow {
            lane: String,
            unit: String,
            index: usize,
            #[serde(default)]
            have: Vec<SyncFile>,
        },
    }

    /// Manifest messages of a sync stream; file bytes travel as transport chunks.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum SyncManifest {
        /// The answer to [`SyncRequest::List`].
        Entries { entries: Vec<SyncEntry> },
        /// Precedes the chunks of one file.
        File { path: String, mode: u32, size: u64 },
        /// The initial pass finished; these are every file the destination should
        /// hold, so anything else there is stale.
        Ready { files: Vec<String> },
    }

    /// A tunnel registered with its box daemon while it serves a client.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct RemoteClient {
        pub pid: u32,
        /// `$SSH_CONNECTION` of the tunnel: client address and port, box address and port.
        #[serde(default)]
        pub ssh: Option<String>,
        pub since_ms: u64,
    }
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
    /// `{lane, code, data}` for advice that never blocks a start. Code
    /// `memory_baseline`: `{peak_memory_bytes, available_memory_bytes, units}` when
    /// the units being started usually peak above the memory available now.
    pub const LANE_WARNING: &str = "lane.warning";
    /// `{stream_id, code, dropped_frames, dropped_bytes}`, sent only to the attached
    /// connection, when its terminal input was discarded. Code `input_overflow`:
    /// the unit was not reading and the oldest queued input was dropped.
    /// Otherwise the stream rejected input (`unknown_stream`, ...) and the rest of
    /// its input is dropped.
    pub const INPUT_DROPPED: &str = "input.dropped";
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
    fn unknown_methods_and_frames_still_decode() -> Result<(), serde_json::Error> {
        let request: Request = serde_json::from_value(serde_json::json!({
            "id": 7, "method": "lane.teleport", "params": {}
        }))?;
        assert_eq!(request.method, Method::Unknown);
        assert_eq!(request.id, 7, "the id survives, so the answer can name it");
        let frame: Control =
            serde_json::from_value(serde_json::json!({"type": "telemetry", "body": {"x": 1}}))?;
        assert!(matches!(frame, Control::Unknown));
        // Known names are unaffected.
        assert_eq!(
            serde_json::from_value::<Method>(serde_json::json!("daemon.shutdown"))?,
            Method::DaemonShutdown
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

    #[test]
    /// Old clients send only `profile`; new ones send `profiles`; both merge in order.
    fn selection_profiles_are_additive() -> Result<(), serde_json::Error> {
        let old: Selection = serde_json::from_value(serde_json::json!({"profile": "web"}))?;
        assert_eq!(old.profile_names(), ["web"]);
        let new: Selection = serde_json::from_value(serde_json::json!({
            "profile": "web", "profiles": ["api", "web", " ", "e2e"]
        }))?;
        assert_eq!(new.profile_names(), ["web", "api", "e2e"]);
        assert!(Selection::default().profile_names().is_empty());
        Ok(())
    }
}
