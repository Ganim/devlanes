//! Routing snapshot for the proxy (plan 6.2): every lane's hostnames, with idle and
//! failed states so the proxy can render a page instead of a bare 502.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::{
    engine::Inner,
    integrations::{RouteEntry, RouteStatus},
    units::UnitState,
};

impl Inner {
    /// Builds the complete route list; the first lane claiming a host keeps it.
    pub(crate) fn route_entries(&self) -> Vec<RouteEntry> {
        let mut seen = BTreeSet::new();
        let mut routes = Vec::new();
        for (project, lane) in self.all_lanes() {
            let name = project.name();
            let st = lane.st();
            let Some(view) = &st.view else { continue };
            let reference = st.reference(&name);
            let hint = format!("devlanes start --lane {reference}");
            for route in &view.routes {
                let host = route.host.to_ascii_lowercase();
                if !seen.insert((host.clone(), route.fallback)) {
                    continue;
                }
                let status = match st.units.get(&route.unit) {
                    Some(rec) => match rec.host.state() {
                        UnitState::Ready => RouteStatus::Ready,
                        UnitState::Starting => RouteStatus::Starting,
                        UnitState::Failed(_) => RouteStatus::Failed {
                            log_tail: rec.failure_tail.join("\n"),
                        },
                        UnitState::Stopping | UnitState::Exited(_) => RouteStatus::Idle,
                    },
                    None => RouteStatus::Idle,
                };
                routes.push(RouteEntry {
                    host,
                    port: route.port,
                    fallback: route.fallback,
                    host_header_upstream: route.host_header_upstream,
                    status,
                    start_hint: hint.clone(),
                    project: name.clone(),
                    lane: reference.clone(),
                    unit: route.unit.clone(),
                    certificate_names: vec![view.domain.clone()],
                });
            }
            for (host, container, port) in &view.infra_routes {
                let host = host.to_ascii_lowercase();
                if !seen.insert((host.clone(), false)) {
                    continue;
                }
                routes.push(RouteEntry {
                    host,
                    port: *port,
                    fallback: false,
                    host_header_upstream: false,
                    status: RouteStatus::Ready,
                    start_hint: hint.clone(),
                    project: name.clone(),
                    lane: reference.clone(),
                    unit: container.clone(),
                    certificate_names: vec![view.domain.clone()],
                });
            }
        }
        routes
    }

    /// Hands the current snapshot to the proxy.
    pub(crate) fn publish_routes(&self) {
        self.integ.routes.publish(self.route_entries());
    }

    /// `hosts`: every route with its lane, unit, port and state.
    pub(crate) fn hosts(&self) -> Value {
        let items: Vec<Value> = self
            .route_entries()
            .into_iter()
            .map(|route| {
                let state = match &route.status {
                    RouteStatus::Ready => "ready",
                    RouteStatus::Idle => "idle",
                    RouteStatus::Starting => "starting",
                    RouteStatus::Failed { .. } => "failed",
                };
                json!({
                    "host": route.host,
                    "project": route.project,
                    "lane": route.lane,
                    "unit": route.unit,
                    "port": route.port,
                    "fallback": route.fallback,
                    "state": state,
                })
            })
            .collect();
        json!({"items": items, "proxy": self.integ.routes.status(), "truncated": false, "next_cursor": null})
    }
}
