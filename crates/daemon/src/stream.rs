//! Lane events on the request that caused them. `start`, `restart`, `resume`,
//! `lane.new` with `start` and `wait` that ask for `events` forward their lanes'
//! `lane.phase` (only their own starts, by `start_id`), `task`, `unit.changed`
//! and `lane.changed` events to their own session, so a client needs no second
//! subscription. The subscription exists before the work begins, and everything
//! the work emitted is delivered before the response. A unit's own watcher task
//! emits its transitions, so its last one can trail the response.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
};

use devlanes_proto::{Event, LaneSummary, events};
use serde_json::{Value, json};
use tokio::sync::{
    broadcast::{self, error::RecvError, error::TryRecvError},
    oneshot,
};

use crate::{
    Session,
    engine::Inner,
    model::{LaneRt, ProjectRt},
};

/// The events one request streams.
pub(crate) struct LaneEvents {
    lanes: Vec<(Arc<ProjectRt>, Arc<LaneRt>)>,
    /// References the lanes had while the request ran; a lane moving to another
    /// index keeps matching its earlier events.
    references: BTreeSet<String>,
    /// `lane.phase` of these starts only; `None` forwards every start (`wait`).
    starts: Option<BTreeSet<u64>>,
    /// Per lane reference, the units whose `unit.changed` is forwarded: the units
    /// `wait` names, or those the own start's `services:started` planned.
    units: BTreeMap<String, BTreeSet<String>>,
    /// Sent first, before any bus event.
    snapshot: Vec<Event>,
}

impl LaneEvents {
    /// The events of `starts` on `lanes`.
    pub(crate) fn starts(lanes: Vec<(Arc<ProjectRt>, Arc<LaneRt>)>, starts: Vec<u64>) -> Self {
        Self {
            lanes,
            references: BTreeSet::new(),
            starts: Some(starts.into_iter().collect()),
            units: BTreeMap::new(),
            snapshot: Vec::new(),
        }
    }

    /// Every start of `lane`, and only `unit` when one is named (`wait`).
    pub(crate) fn lane(project: Arc<ProjectRt>, lane: Arc<LaneRt>, unit: Option<String>) -> Self {
        let mut units = BTreeMap::new();
        if let Some(unit) = unit {
            let reference = lane.st().reference(&project.name());
            units.insert(reference, BTreeSet::from([unit]));
        }
        Self {
            lanes: vec![(project, lane)],
            references: BTreeSet::new(),
            starts: None,
            units,
            snapshot: Vec::new(),
        }
    }

    /// Starts the stream with the lane as it is now: a `lane.changed`, or one
    /// `unit.changed` per unit when only some are followed. A start that began
    /// earlier then shows its share at once.
    pub(crate) fn with_snapshot(mut self, lane: LaneSummary) -> Self {
        self.snapshot = match self.units.get(&lane.reference) {
            Some(names) => lane
                .units
                .iter()
                .filter(|unit| names.contains(&unit.name))
                .map(|unit| event(events::UNIT_CHANGED, json!({ "unit": unit })))
                .collect(),
            None => vec![event(events::LANE_CHANGED, json!({ "lane": lane }))],
        };
        self
    }

    /// Whether `event` belongs to the request; learns the planned units of its
    /// own starts on the way.
    fn wants(&mut self, event: &Event) -> bool {
        let data = &event.data;
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
        let lane = match event.event.as_str() {
            events::PHASE => text(data.get("lane")),
            events::TASK => text(data.get("lane")),
            events::UNIT_CHANGED => text(data.get("unit").and_then(|unit| unit.get("lane"))),
            events::LANE_CHANGED => text(data.get("lane").and_then(|lane| lane.get("reference"))),
            _ => None,
        };
        let Some(lane) = lane else {
            return false;
        };
        for (project, rt) in &self.lanes {
            let reference = rt.st().reference(&project.name());
            self.references.insert(reference);
        }
        if !self.references.contains(&lane) {
            return false;
        }
        match event.event.as_str() {
            events::PHASE => {
                let Some(starts) = &self.starts else {
                    return true;
                };
                let own = data
                    .get("start_id")
                    .and_then(Value::as_u64)
                    .is_some_and(|id| starts.contains(&id));
                if own && let Some(planned) = data.get("units").and_then(Value::as_array) {
                    let planned = planned
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    self.units.insert(lane, planned);
                }
                own
            }
            events::UNIT_CHANGED => {
                let name = data
                    .get("unit")
                    .and_then(|unit| unit.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.units
                    .get(&lane)
                    .is_none_or(|units| units.contains(name))
            }
            _ => true,
        }
    }
}

impl Inner {
    /// Runs `work`, streaming `stream`'s events to `session` meanwhile; without a
    /// `stream` it only runs `work`. Returns once every event `work` emitted was
    /// sent, so none arrives after the response. A client that leaves stops the
    /// stream, never the work.
    pub(crate) async fn with_lane_events<T>(
        &self,
        session: &Session,
        stream: Option<LaneEvents>,
        work: impl Future<Output = T>,
    ) -> T {
        let Some(filter) = stream else {
            return work.await;
        };
        let receiver = self.bus.subscribe();
        let (end, ended) = oneshot::channel();
        let forwarder = tokio::spawn(forward(receiver, session.clone(), filter, ended));
        let result = work.await;
        let _ = end.send(());
        let _ = forwarder.await;
        result
    }
}

/// Forwards wanted events until `ended`, then the ones already queued.
async fn forward(
    mut receiver: broadcast::Receiver<Event>,
    session: Session,
    mut filter: LaneEvents,
    mut ended: oneshot::Receiver<()>,
) {
    for event in std::mem::take(&mut filter.snapshot) {
        if session.event(event).await.is_err() {
            return;
        }
    }
    loop {
        let next = tokio::select! {
            next = receiver.recv() => match next {
                Ok(event) => Ok(event),
                Err(RecvError::Lagged(missed)) => Err(missed),
                Err(RecvError::Closed) => return,
            },
            _ = &mut ended => break,
        };
        if !deliver(&session, &mut filter, next).await {
            return;
        }
    }
    // Emitting is synchronous, so what the work emitted is queued by now.
    loop {
        let next = match receiver.try_recv() {
            Ok(event) => Ok(event),
            Err(TryRecvError::Lagged(missed)) => Err(missed),
            Err(_) => return,
        };
        if !deliver(&session, &mut filter, next).await {
            return;
        }
    }
}

/// Sends a bus event the filter wants, or one `resync` for `Err(missed)`
/// events this stream fell behind on; false once the client is gone.
async fn deliver(session: &Session, filter: &mut LaneEvents, next: Result<Event, u64>) -> bool {
    let event = match next {
        Ok(event) if filter.wants(&event) => event,
        Ok(_) => return true,
        Err(missed) => event(events::RESYNC, json!({ "missed": missed })),
    };
    session.event(event).await.is_ok()
}

/// An event made for this stream only, outside the bus sequence.
fn event(name: &str, data: Value) -> Event {
    Event {
        seq: 0,
        event: name.into(),
        data,
    }
}
