//! Detail on a running start phase: `lane.phase` events with `state: progress`
//! carrying the infra containers' state, or the setup entry or task that runs.
//! One event per change, never per log line.

use std::{future::Future, time::Duration};

use devlanes_proto::{PhaseContainer, PhaseStep, events};
use serde_json::{Value, json};

use crate::{engine::Inner, integrations::InfraSpec, lifecycle::StartCx};

/// How often the containers are looked at while infra comes up. Each look runs
/// `docker compose ps`, so it is no faster than a person can read the line.
const INFRA_POLL: Duration = Duration::from_secs(1);

impl Inner {
    /// Emits a `progress` event of `phase` in the start `cx`, with `detail`'s
    /// fields added.
    fn phase_progress(&self, cx: &StartCx, phase: &str, detail: Value) {
        let mut data = json!({
            "lane": cx.reference,
            "phase": phase,
            "state": "progress",
            "start_id": cx.start_id,
        });
        if let (Some(data), Value::Object(detail)) = (data.as_object_mut(), detail) {
            data.extend(detail);
        }
        self.bus.emit(events::PHASE, data);
    }

    /// Emits that `phase` of `cx` now runs `name`, step `index` (1-based) of `count`.
    pub(crate) fn phase_step(
        &self,
        cx: &StartCx,
        phase: &str,
        name: &str,
        index: usize,
        count: usize,
    ) {
        let step = PhaseStep {
            name: name.to_owned(),
            index,
            count,
        };
        self.phase_progress(cx, phase, json!({ "step": step }));
    }

    /// Runs `work`, meanwhile emitting the infra containers of `spec` each time
    /// what the provider reports changes. `last` is what was emitted before, so
    /// several calls in one phase stay one sequence of changes.
    pub(crate) async fn with_container_progress<T>(
        &self,
        cx: &StartCx,
        spec: &InfraSpec,
        last: &mut Vec<PhaseContainer>,
        work: impl Future<Output = T>,
    ) -> T {
        tokio::pin!(work);
        let mut tick = tokio::time::interval(INFRA_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The work decides when the phase ends; a look in flight is dropped.
            tokio::select! {
                biased;
                output = &mut work => return output,
                _ = tick.tick() => {}
            }
            let containers = tokio::select! {
                biased;
                output = &mut work => return output,
                containers = self.integ.infra.progress(spec) => containers,
            };
            if !containers.is_empty() && containers != *last {
                self.phase_progress(cx, "infra", json!({ "containers": containers }));
                *last = containers;
            }
        }
    }
}
