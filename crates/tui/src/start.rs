//! A start that runs inside the TUI, the way mprocs works: `devlanes start` on
//! a terminal sends the start request, then the TUI opens and follows the
//! request's own events (`"events": true`) until its result arrives.
//!
//! The request is sent before the TUI opens, so a start the daemon refuses at
//! once (no config, unknown unit) ends as a normal CLI error instead.

use std::time::Duration;

use devlanes_client::{Client, Frame};
use devlanes_proto::{
    Control, Event, LaneSummary, Outcome, RpcError, StartResult, UnitFailureData, events,
};
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};

use crate::{api::ApiError, text};

/// Frames buffered between the request's connection and the TUI.
const START_QUEUE: usize = 1024;

/// One frame of the start request.
#[derive(Debug)]
pub enum StartFrame {
    /// A lane, unit, phase or task event of this start.
    Event(Event),
    /// The response: a start result (`resume`: one per lane), or why it failed.
    Done(Result<Value, ApiError>),
}

/// A start the daemon accepted, handed to the TUI.
#[derive(Debug)]
pub struct StartHandoff {
    /// The command's verb: `start`, `restart`, `resume` or `new lane`.
    pub label: String,
    /// Frames that arrived before the TUI opened.
    pub frames: Vec<StartFrame>,
    /// The rest of the request.
    pub stream: mpsc::Receiver<StartFrame>,
}

/// How the start stood when the user quit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartExit {
    /// Every selected service became ready.
    Ready,
    /// The start failed, timed out or a service failed.
    Failed,
    /// No result yet; the daemon keeps starting.
    Pending,
}

/// Reads `client`'s answer to request `id` into a channel. A lost connection
/// ends the stream with a disconnect error.
pub fn follow_start(mut client: Client, id: u64) -> mpsc::Receiver<StartFrame> {
    let (sender, receiver) = mpsc::channel(START_QUEUE);
    tokio::spawn(async move {
        loop {
            let frame = match client.next_frame().await {
                Ok(Frame::Control(Control::Event(event))) => StartFrame::Event(event),
                Ok(Frame::Control(Control::Response(response))) if response.id == id => {
                    StartFrame::Done(match response.result {
                        Outcome::Ok { data } => Ok(data),
                        Outcome::Error { error } => Err(ApiError::Rpc(error)),
                    })
                }
                Ok(_) => continue,
                Err(error) => StartFrame::Done(Err(ApiError::from(error))),
            };
            let done = matches!(frame, StartFrame::Done(_));
            if sender.send(frame).await.is_err() || done {
                return;
            }
        }
    });
    receiver
}

/// Waits until the start is underway: its first phase, task or unit event,
/// its result, or `wait` without an answer. A failure before that is returned
/// as the error a CLI would print. A `lane.changed` alone does not count: the
/// daemon sends the lane's snapshot before it checks the config.
pub async fn await_start(
    label: String,
    mut stream: mpsc::Receiver<StartFrame>,
    wait: Duration,
) -> Result<StartHandoff, ApiError> {
    let deadline = Instant::now() + wait;
    let mut frames = Vec::new();
    loop {
        let frame = match tokio::time::timeout_at(deadline, stream.recv()).await {
            Err(_) => break,
            Ok(None) => {
                return Err(ApiError::Disconnected(
                    "the daemon closed the connection".into(),
                ));
            }
            Ok(Some(frame)) => frame,
        };
        let frame = match frame {
            StartFrame::Done(Err(error)) => return Err(error),
            frame => frame,
        };
        let underway = match &frame {
            StartFrame::Done(_) => true,
            StartFrame::Event(event) => matches!(
                event.event.as_str(),
                events::PHASE | events::TASK | events::UNIT_CHANGED
            ),
        };
        frames.push(frame);
        if underway {
            break;
        }
    }
    Ok(StartHandoff {
        label,
        frames,
        stream,
    })
}

/// How the start ended, for the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOutcome {
    pub ok: bool,
    /// `acme@1 ready · 5 of 5 services`, or what failed.
    pub line: String,
}

/// The start the TUI was opened for.
#[derive(Debug, Default)]
pub struct StartRun {
    /// The command's verb, used in the header and in error reports.
    pub label: String,
    /// Lanes the start's events named, in order; the first gets the focus.
    pub lanes: Vec<String>,
    /// Whether the TUI already moved the selection to the first lane.
    pub focused: bool,
    /// The phase running now: `infra`, `setup`, `tasks` or `services`.
    phase: Option<String>,
    /// A task running in it.
    task: Option<String>,
    /// What the phase waits on, from its latest `progress` event:
    /// `waiting for postgres (healthy 1 of 2)`, `migrate (2 of 3)`.
    detail: Option<String>,
    /// Services this start spawns, from `services:started`.
    units: Vec<String>,
    pub outcome: Option<StartOutcome>,
}

impl StartRun {
    /// A run for the command `label`.
    pub fn new(label: String) -> Self {
        Self {
            label,
            ..Self::default()
        }
    }

    /// Applies one event; returns a lane seen for the first time.
    pub fn event(&mut self, event: &Event) -> Option<String> {
        let data = &event.data;
        let field = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
        let lane = match event.event.as_str() {
            events::LANE_CHANGED => data
                .get("lane")
                .and_then(|lane| lane.get("reference"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            events::UNIT_CHANGED => data
                .get("unit")
                .and_then(|unit| unit.get("lane"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            events::PHASE | events::TASK => field("lane"),
            _ => None,
        };
        match event.event.as_str() {
            events::PHASE => match field("state").as_deref() {
                Some("progress") => {
                    if self.phase != field("phase") {
                        self.phase = field("phase");
                        self.task = None;
                    }
                    // Detail this client cannot read keeps the last one.
                    if let Some(detail) = devlanes_term::phase_detail(data) {
                        self.detail = Some(detail);
                    }
                }
                Some("started") => {
                    self.phase = field("phase");
                    self.task = None;
                    self.detail = None;
                    if self.phase.as_deref() == Some("services") {
                        self.units = data
                            .get("units")
                            .and_then(Value::as_array)
                            .map(|units| {
                                units
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                }
                Some("done" | "failed" | "skipped") if self.phase == field("phase") => {
                    self.phase = None;
                    self.task = None;
                    self.detail = None;
                }
                _ => {}
            },
            events::TASK => match field("state").as_deref() {
                Some("started" | "running") => self.task = field("task"),
                _ if self.task == field("task") => self.task = None,
                _ => {}
            },
            _ => {}
        }
        let lane = lane.map(|lane| text::clean(&lane))?;
        if self.lanes.contains(&lane) {
            return None;
        }
        self.lanes.push(lane.clone());
        Some(lane)
    }

    /// The header line while the start runs: `start acme@1 · tasks: migrate`.
    pub fn running_line(&self, lanes: &[LaneSummary]) -> String {
        let mut line = self.label.clone();
        if let Some(lane) = self.lanes.first() {
            line.push(' ');
            line.push_str(lane);
            if self.lanes.len() > 1 {
                line.push_str(&format!(" and {} more", self.lanes.len() - 1));
            }
        }
        let Some(phase) = &self.phase else {
            return line;
        };
        line.push_str(" · ");
        line.push_str(&text::clean(phase));
        // The same words as the CLI's progress lines.
        if let Some(detail) = &self.detail {
            line.push_str(&format!(": {}", text::clean(detail)));
        } else if let Some(task) = &self.task {
            line.push_str(&format!(": {}", text::clean(task)));
        } else if phase == "infra" {
            line.push_str(": starting containers, waiting until they are healthy");
        } else if phase == "setup" {
            line.push_str(": running infra.setup");
        } else if phase == "services" && !self.units.is_empty() {
            let ready = self
                .units
                .iter()
                .filter(|unit| unit_ready(lanes, self.lanes.first(), unit))
                .count();
            line.push_str(&format!(" {ready} of {} ready", self.units.len()));
        }
        line
    }

    /// Records the result and returns what to report: the services that
    /// failed, and an error that ended the start (or one lane of a resume).
    pub fn finish(
        &mut self,
        result: &Result<Value, ApiError>,
    ) -> (Vec<UnitFailureData>, Option<ApiError>) {
        self.phase = None;
        self.task = None;
        self.detail = None;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                self.outcome = Some(StartOutcome {
                    ok: false,
                    line: format!(
                        "{} failed: {}",
                        self.running_line(&[]),
                        crate::words::api(error).one_line()
                    ),
                });
                return (Vec::new(), Some(error.clone()));
            }
        };
        let results = start_results(value);
        let mut failures = Vec::new();
        let mut errors = Vec::new();
        let mut parts = Vec::new();
        let mut ok = true;
        for result in results {
            match result {
                Ok(start) => {
                    ok &= start.ready && start.failures.is_empty() && !start.timed_out;
                    if !self.lanes.contains(&start.lane) {
                        self.lanes.push(start.lane.clone());
                    }
                    parts.push(result_line(&start));
                    failures.extend(start.failures);
                }
                Err((lane, error)) => {
                    ok = false;
                    parts.push(format!("{lane}: {}", crate::words::rpc(&error).one_line()));
                    errors.push(error);
                }
            }
        }
        if parts.is_empty() {
            parts.push(format!("{} done", self.label));
        }
        self.outcome = Some(StartOutcome {
            ok,
            line: text::clean(&parts.join("; ")),
        });
        (failures, errors.into_iter().next().map(ApiError::Rpc))
    }
}

/// `acme@1 ready · 5 of 5 services`, `acme@1: web failed · 4 of 5 ready`.
fn result_line(start: &StartResult) -> String {
    let services: Vec<_> = start
        .units
        .iter()
        .filter(|unit| matches!(unit.kind.as_str(), "service" | ""))
        .collect();
    let ready = services
        .iter()
        .filter(|unit| matches!(unit.state.as_str(), "ready" | "running"))
        .count();
    let count = format!("{ready} of {} ready", services.len());
    let failed: Vec<&str> = start.failures.iter().map(|f| f.unit.as_str()).collect();
    if !failed.is_empty() {
        format!("{}: {} failed · {count}", start.lane, failed.join(", "))
    } else if start.timed_out {
        format!("{} timed out · {count}", start.lane)
    } else if !start.ready {
        format!("{} not ready · {count}", start.lane)
    } else if services.is_empty() {
        format!("{} ready", start.lane)
    } else {
        format!("{} ready · {} services", start.lane, services.len())
    }
}

/// The start results in a response: `start` and `restart` answer one,
/// `lane.new` nests it under `start`, `resume` lists one per lane (or its error).
fn start_results(value: &Value) -> Vec<Result<StartResult, (String, RpcError)>> {
    let parse = |value: &Value| serde_json::from_value::<StartResult>(value.clone()).ok();
    if value.get("ready").is_some() {
        return parse(value).map(Ok).into_iter().collect();
    }
    if let Some(start) = value.get("start") {
        return parse(start).map(Ok).into_iter().collect();
    }
    value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item.get("error") {
                    Some(error) => {
                        let error: RpcError = serde_json::from_value(error.clone()).ok()?;
                        let lane = item
                            .get("lane")
                            .and_then(Value::as_str)
                            .unwrap_or("lane")
                            .to_owned();
                        Some(Err((lane, error)))
                    }
                    None => parse(item).map(Ok),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `unit` (`api` or `acme@1:api`) is ready in the TUI's lanes.
fn unit_ready(lanes: &[LaneSummary], default_lane: Option<&String>, unit: &str) -> bool {
    let (lane, name) = match unit.split_once(':') {
        Some((lane, name)) => (lane, name),
        None => match default_lane {
            Some(lane) => (lane.as_str(), unit),
            None => return false,
        },
    };
    lanes
        .iter()
        .filter(|summary| summary.reference == lane)
        .flat_map(|summary| &summary.units)
        .any(|summary| {
            summary.name == name && matches!(summary.state.as_str(), "ready" | "running")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, data: Value) -> Event {
        Event {
            seq: 1,
            event: name.into(),
            data,
        }
    }

    #[tokio::test]
    async fn a_refused_start_never_reaches_the_tui() {
        let (sender, receiver) = mpsc::channel(8);
        // The lane snapshot comes first, then the config check fails.
        let snapshot = event(
            events::LANE_CHANGED,
            json!({"lane": {"reference": "acme@1"}}),
        );
        sender.send(StartFrame::Event(snapshot)).await.ok();
        let refused = RpcError::new("no_config", json!({}), 3);
        sender
            .send(StartFrame::Done(Err(ApiError::Rpc(refused.clone()))))
            .await
            .ok();
        let result = await_start("start".into(), receiver, Duration::from_secs(5)).await;
        assert!(matches!(result, Err(ApiError::Rpc(error)) if error == refused));
    }

    #[tokio::test]
    async fn the_first_phase_or_a_quiet_daemon_opens_the_tui() {
        let (sender, receiver) = mpsc::channel(8);
        let phase = event(
            events::PHASE,
            json!({"lane": "acme@1", "phase": "infra", "state": "started"}),
        );
        sender.send(StartFrame::Event(phase)).await.ok();
        let handoff = await_start("start".into(), receiver, Duration::from_secs(5)).await;
        assert!(matches!(handoff, Ok(h) if h.frames.len() == 1));
        // Nothing at all: after the wait the start counts as underway.
        let (_sender, receiver) = mpsc::channel(8);
        let handoff = await_start("start".into(), receiver, Duration::from_millis(20)).await;
        assert!(matches!(handoff, Ok(h) if h.frames.is_empty()));
    }

    #[test]
    fn the_line_follows_phases_tasks_and_ready_services() {
        let mut run = StartRun::new("start".into());
        assert_eq!(run.running_line(&[]), "start");
        let phase = |phase, state| {
            event(
                events::PHASE,
                json!({"lane": "acme@1", "phase": phase, "state": state, "units": ["api", "web"]}),
            )
        };
        assert_eq!(run.event(&phase("infra", "started")), Some("acme@1".into()));
        assert_eq!(
            run.running_line(&[]),
            "start acme@1 · infra: starting containers, waiting until they are healthy"
        );
        run.event(&phase("infra", "done"));
        run.event(&phase("tasks", "started"));
        run.event(&event(
            events::TASK,
            json!({"lane": "acme@1", "task": "migrate", "state": "started"}),
        ));
        assert_eq!(run.running_line(&[]), "start acme@1 · tasks: migrate");
        run.event(&phase("tasks", "done"));
        run.event(&phase("services", "started"));
        let lanes = [LaneSummary {
            reference: "acme@1".into(),
            units: vec![devlanes_proto::UnitSummary {
                name: "api".into(),
                lane: "acme@1".into(),
                state: "ready".into(),
                ..Default::default()
            }],
            ..LaneSummary::default()
        }];
        assert_eq!(
            run.running_line(&lanes),
            "start acme@1 · services 1 of 2 ready"
        );
    }

    #[test]
    fn the_line_says_what_infra_and_setup_wait_on() {
        let mut run = StartRun::new("start".into());
        let phase = |phase: &str, state: &str, detail: Value| {
            let mut data = json!({"lane": "acme@1", "phase": phase, "state": state, "start_id": 2});
            if let (Some(data), Value::Object(detail)) = (data.as_object_mut(), detail) {
                data.extend(detail);
            }
            event(events::PHASE, data)
        };
        run.event(&phase("infra", "started", json!({})));
        run.event(&phase(
            "infra",
            "progress",
            json!({"containers": [
                {"name": "redis", "state": "healthy"},
                {"name": "postgres", "state": "waiting_healthy", "health": "starting"},
            ]}),
        ));
        assert_eq!(
            run.running_line(&[]),
            "start acme@1 · infra: waiting for postgres (healthy 1 of 2)"
        );
        // The next phase starts without detail of its own.
        run.event(&phase("infra", "done", json!({})));
        run.event(&phase("setup", "started", json!({})));
        assert_eq!(
            run.running_line(&[]),
            "start acme@1 · setup: running infra.setup"
        );
        run.event(&phase(
            "setup",
            "progress",
            json!({"step": {"name": "database", "index": 2, "count": 3}}),
        ));
        assert_eq!(
            run.running_line(&[]),
            "start acme@1 · setup: database (2 of 3)"
        );
        run.event(&phase("setup", "done", json!({})));
        assert_eq!(run.running_line(&[]), "start acme@1");
    }

    #[test]
    fn results_become_one_line() {
        let unit = |name: &str, state: &str| json!({"name": name, "lane": "acme@1", "state": state, "kind": "service", "pid": null, "exit_code": null, "ports": {}, "urls": [], "actor": {"kind": "human"}, "reason": null});
        let mut run = StartRun::new("start".into());
        let ok = json!({"lane": "acme@1", "ready": true, "units": [unit("api", "ready"), unit("web", "ready")]});
        let (failures, error) = run.finish(&Ok(ok));
        assert!(failures.is_empty() && error.is_none());
        assert_eq!(
            run.outcome,
            Some(StartOutcome {
                ok: true,
                line: "acme@1 ready · 2 services".into()
            })
        );
        let failed = json!({"lane": "acme@1", "ready": false, "units": [unit("api", "ready"), unit("web", "failed")],
            "failures": [{"lane": "acme@1", "unit": "web", "exit_code": 1, "log_tail": ["boom"]}]});
        let (failures, _) = run.finish(&Ok(failed));
        assert_eq!(failures.len(), 1);
        assert_eq!(
            run.outcome.map(|o| (o.ok, o.line)),
            Some((false, "acme@1: web failed · 1 of 2 ready".into()))
        );
    }
}
