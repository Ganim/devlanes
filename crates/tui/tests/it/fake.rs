//! An in-memory daemon for TUI tests.

use std::{path::PathBuf, sync::Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_proto::{
    Actor, ActorKind, AttachParams, AttachResult, Event, LaneSummary, LogLine, LogQuery, Method,
    Page, RunParams, Selection, UnitSummary,
};
use devlanes_tui::{ApiError, AttachInput, AttachSession, DaemonApi, EventStream, UnitMetrics};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Records every call and serves canned lanes, metrics and terminal output.
#[derive(Default)]
pub struct FakeApi {
    pub lanes: Mutex<Vec<LaneSummary>>,
    pub metrics: Vec<UnitMetrics>,
    pub screen: Vec<u8>,
    pub calls: Mutex<Vec<String>>,
    pub events: Mutex<Option<mpsc::Sender<Event>>>,
    /// Keeps attach output channels open so streams do not end on their own.
    outputs: Mutex<Vec<mpsc::Sender<Bytes>>>,
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
                .send(Event {
                    seq: 1,
                    event: event.into(),
                    data,
                })
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
            let _ = output_tx.send(Bytes::from(self.screen.clone())).await;
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
        self.record(format!("{method:?} {lane} {:?}", selection.units));
        Ok(json!({}))
    }

    async fn run_task(&self, lane: String, params: RunParams) -> Result<Value, ApiError> {
        self.record(format!("run {} {lane}", params.task));
        Ok(json!({}))
    }

    async fn logs(&self, lane: String, query: LogQuery) -> Result<Page<LogLine>, ApiError> {
        self.record(format!("logs {lane} {:?}", query.units));
        Ok(Page::complete(Vec::new()))
    }

    async fn metrics(&self) -> Result<Vec<UnitMetrics>, ApiError> {
        self.record("metrics".into());
        Ok(self.metrics.clone())
    }
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
