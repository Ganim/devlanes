//! An in-memory daemon for TUI tests.

use std::{path::PathBuf, sync::Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_proto::{
    Actor, ActorKind, AttachParams, AttachResult, Event, LaneSummary, LogLine, LogQuery, Method,
    Page, RpcError, RunParams, Selection, UnitSummary,
};
use devlanes_tui::{
    ApiError, AttachEvent, AttachInput, AttachSession, DaemonApi, EventStream, Metrics,
    MetricsQuery,
    api::{ProfileInfo, ProfileList},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Records every call and serves canned lanes, metrics and terminal output.
#[derive(Default)]
pub struct FakeApi {
    pub lanes: Mutex<Vec<LaneSummary>>,
    pub metrics: Metrics,
    pub screen: Vec<u8>,
    /// When set, start/stop/restart fail with this daemon error.
    pub control_error: Option<RpcError>,
    /// When set, `lane.clean` fails with this daemon error.
    pub clean_error: Option<RpcError>,
    /// The daemon log the TUI tails.
    pub log_path: Option<PathBuf>,
    /// `DEVLANES_HOME`, where config.yaml is written.
    pub home: Option<PathBuf>,
    /// When set, `profile.list` fails with this daemon error.
    pub profiles_error: Option<RpcError>,
    pub calls: Mutex<Vec<String>>,
    pub events: Mutex<Option<mpsc::Sender<Result<Event, ApiError>>>>,
    /// Keeps attach output channels open so streams do not end on their own.
    outputs: Mutex<Vec<mpsc::Sender<AttachEvent>>>,
    inputs: Mutex<Vec<mpsc::Receiver<AttachInput>>>,
}

impl FakeApi {
    /// A fake with the given lanes.
    pub fn with_lanes(lanes: Vec<LaneSummary>) -> Self {
        Self {
            lanes: Mutex::new(lanes),
            ..Self::default()
        }
    }

    /// Records a call.
    fn record(&self, call: String) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call);
        }
    }

    /// Calls recorded so far.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Pushes an event to the open subscription, if any.
    pub async fn emit(&self, event: &str, data: Value) -> bool {
        let sender = self.events.lock().ok().and_then(|s| s.clone());
        match sender {
            Some(sender) => sender
                .send(Ok(Event {
                    seq: 1,
                    event: event.into(),
                    data,
                }))
                .await
                .is_ok(),
            None => false,
        }
    }
}

#[async_trait]
impl DaemonApi for FakeApi {
    async fn status(&self) -> Result<Vec<LaneSummary>, ApiError> {
        self.record("status".into());
        Ok(self.lanes.lock().map(|l| l.clone()).unwrap_or_default())
    }

    async fn subscribe(&self) -> Result<EventStream, ApiError> {
        self.record("subscribe".into());
        let (sender, receiver) = mpsc::channel(16);
        if let Ok(mut events) = self.events.lock() {
            *events = Some(sender);
        }
        Ok(EventStream::new(receiver))
    }

    async fn attach(&self, lane: String, params: AttachParams) -> Result<AttachSession, ApiError> {
        self.record(format!("attach {} {lane}", params.unit));
        let (output_tx, output_rx) = mpsc::channel(16);
        let (input_tx, input_rx) = mpsc::channel(16);
        if !self.screen.is_empty() {
            let screen = AttachEvent::Output(Bytes::from(self.screen.clone()));
            output_tx
                .send(screen)
                .await
                .map_err(|error| ApiError::Decode(error.to_string()))?;
        }
        if let (Ok(mut outputs), Ok(mut inputs)) = (self.outputs.lock(), self.inputs.lock()) {
            outputs.push(output_tx);
            inputs.push(input_rx);
        }
        Ok(AttachSession::new(
            AttachResult {
                stream_id: 7,
                unit: params.unit,
                lane,
            },
            output_rx,
            input_tx,
        ))
    }

    async fn control(
        &self,
        method: Method,
        lane: String,
        selection: Selection,
    ) -> Result<Value, ApiError> {
        if selection.profiles.is_empty() {
            self.record(format!("{method:?} {lane} {:?}", selection.units));
        } else {
            self.record(format!(
                "{method:?} {lane} {:?} profiles={:?}",
                selection.units, selection.profiles
            ));
        }
        match &self.control_error {
            Some(error) => Err(ApiError::Rpc(error.clone())),
            None => Ok(json!({})),
        }
    }

    async fn run_task(
        &self,
        lane: String,
        params: RunParams,
        yes: bool,
    ) -> Result<Value, ApiError> {
        let yes = if yes { " yes" } else { "" };
        self.record(format!("run {} {lane}{yes}", params.task));
        Ok(json!({}))
    }

    async fn logs(&self, lane: String, query: LogQuery) -> Result<Page<LogLine>, ApiError> {
        self.record(format!("logs {lane} {:?}", query.units));
        Ok(Page::complete(Vec::new()))
    }

    async fn metrics(&self, query: MetricsQuery) -> Result<Metrics, ApiError> {
        self.record(format!("metrics {}", query.range_ms));
        Ok(self.metrics.clone())
    }

    async fn daemon_status(&self) -> Result<Value, ApiError> {
        self.record("daemon.status".into());
        Ok(daemon_status())
    }

    async fn doctor(&self) -> Result<Value, ApiError> {
        self.record("doctor".into());
        Ok(doctor_facts())
    }

    async fn clean_lane(&self, lane: String) -> Result<Value, ApiError> {
        self.record(format!("lane.clean {lane}"));
        if let Some(error) = &self.clean_error {
            return Err(ApiError::Rpc(error.clone()));
        }
        // A cleaned lane keeps its worktree but loses its units and records.
        let mut stopped = Vec::new();
        if let Ok(mut lanes) = self.lanes.lock()
            && let Some(found) = lanes.iter_mut().find(|l| l.reference == lane)
        {
            stopped = found.units.drain(..).map(|u| u.name).collect();
            found.state = "idle".into();
        }
        Ok(json!({"lane": lane, "stopped": stopped, "tasks": [], "teardown": [], "infra": null}))
    }

    async fn lane_gc(&self) -> Result<Value, ApiError> {
        self.record("lane.gc".into());
        Ok(json!({"cleaned": ["acme@4"], "offsets_removed": 0}))
    }

    async fn stop_daemon(&self) -> Result<Value, ApiError> {
        self.record("daemon.shutdown".into());
        Ok(json!({"stopping": true, "processes": 3}))
    }

    async fn restart_daemon(&self) -> Result<Value, ApiError> {
        self.record("daemon.restart".into());
        Ok(daemon_status())
    }

    async fn start_daemon(&self) -> Result<Value, ApiError> {
        self.record("daemon.start".into());
        Ok(daemon_status())
    }

    async fn profile_list(&self, lane: String) -> Result<ProfileList, ApiError> {
        self.record(format!("profile.list {lane}"));
        if let Some(error) = &self.profiles_error {
            return Err(ApiError::Rpc(error.clone()));
        }
        Ok(ProfileList {
            lane,
            items: profiles(),
        })
    }

    fn daemon_log(&self) -> Option<PathBuf> {
        self.log_path.clone()
    }

    fn home(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    /// Fixed: a source build with nothing dismissed, whatever binary runs
    /// the tests and whatever its home holds.
    fn notices(&self) -> (devlanes_client::update::InstallMethod, Vec<String>) {
        (devlanes_client::update::InstallMethod::Source, Vec::new())
    }
}

/// Profiles as `profile.list` describes them: a default, an extra and one without infra.
pub fn profiles() -> Vec<ProfileInfo> {
    let profile = |name: &str, include: &[&str], skips_infra, is_default| ProfileInfo {
        name: name.into(),
        include: include.iter().map(|s| (*s).to_owned()).collect(),
        exclude: Vec::new(),
        skips_infra,
        is_default,
    };
    vec![
        profile("web", &["web", "api"], false, true),
        profile("e2e", &["+e2e", "web", "api", "worker"], false, false),
        profile("frontend", &["web"], true, false),
    ]
}

/// The fake daemon's version: this build's, so no version-skew notice shows
/// unless a test sets another version on purpose.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `daemon.status` as the daemon sends it.
pub fn daemon_status() -> Value {
    json!({
        "version": VERSION,
        "pid": 4242,
        "uptime_ms": 7_500_000,
        "managed_processes": 3,
        "socket": "/home/acme/.devlanes/run/daemon.sock",
    })
}

/// `doctor` facts with a fallback port, a missing CA and an applied hosts file.
pub fn doctor_facts() -> Value {
    json!({
        "version": VERSION,
        "pid": 4242,
        "home": "/home/acme/.devlanes",
        "db_path": "/home/acme/.devlanes/state.db",
        "schema_version": 7,
        "supported_schema_version": 7,
        "socket": "/home/acme/.devlanes/run/daemon.sock",
        "sandbox": {"codex_sandbox": false, "app_sandbox": false},
        "infra": {
            "docker": {"reachable": true, "version": "27.1.1"},
            "compose": {"version": "2.29.1", "supports_lane_overrides": true}
        },
        "proxy": {
            "http": {"bound": true, "port": 80},
            "https": {"bound": true, "port": 8443, "fallback_reason": "port 443 needs root"}
        },
        "ca": {"present": false, "error": "no CA has been created yet"},
        "projects": 2,
        "subscribers": 4,
        "hosts": {"mode": "auto", "probe": "localhost_resolves", "state": "applied", "names": 6}
    })
}

/// A unit in `lane` with `state`.
pub fn unit(lane: &str, name: &str, state: &str) -> UnitSummary {
    UnitSummary {
        name: name.into(),
        lane: lane.into(),
        state: state.into(),
        kind: "service".into(),
        ..UnitSummary::default()
    }
}

/// A lane of `project` with an index, branch and units.
pub fn lane(
    project: &str,
    index: Option<u32>,
    branch: &str,
    state: &str,
    units: Vec<UnitSummary>,
) -> LaneSummary {
    let reference = match index {
        Some(index) => format!("{project}@{index}"),
        None => format!("{project}@{branch}"),
    };
    LaneSummary {
        reference,
        project: project.into(),
        index,
        branch: Some(branch.into()),
        path: PathBuf::from(format!("/work/{project}/{branch}")),
        state: state.into(),
        units,
        ..LaneSummary::default()
    }
}

/// Two projects with lanes in every state the tree distinguishes.
pub fn mixed_lanes() -> Vec<LaneSummary> {
    let mut worker = unit("acme@0", "worker", "failed");
    worker.exit_code = Some(1);
    worker.actor = Actor {
        kind: ActorKind::Agent,
        agent_id: Some("claude".into()),
        session_id: None,
    };
    let mut api = unit("acme@0", "api", "ready");
    api.urls = vec!["https://api.acme.localhost".into()];
    api.cpu_percent = Some(12.0);
    api.memory_bytes = Some(256 * 1024 * 1024);
    let mut migrate = unit("acme@0", "migrate", "exited");
    migrate.kind = "task".into();
    migrate.exit_code = Some(0);
    let main = lane(
        "acme",
        Some(0),
        "main",
        "running",
        vec![api, unit("acme@0", "web", "starting"), worker, migrate],
    );
    let idle = lane(
        "acme",
        Some(1),
        "feat/login",
        "idle",
        vec![unit("acme@1", "api", "idle")],
    );
    let mut moving = lane("acme", Some(2), "fix/cache", "idle", Vec::new());
    moving.moves_to = Some(3);
    moving.config_changed = true;
    let unassigned = lane("acme", None, "spike", "unassigned", Vec::new());
    let shop = lane(
        "shop",
        Some(0),
        "main",
        "running",
        vec![
            unit("shop@0", "storefront", "ready"),
            unit("shop@0", "admin", "stopped"),
        ],
    );
    vec![main, idle, moving, unassigned, shop]
}
