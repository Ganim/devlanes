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
    /// Builds the complete route list; local lanes come first, then hostnames
    /// mirrored from connected boxes, and the first claim of a host keeps it.
    pub(crate) fn route_entries(&self) -> Vec<RouteEntry> {
        let mut routes = self.local_route_entries();
        let mut seen: BTreeSet<(String, bool)> = routes
            .iter()
            .map(|route| (route.host.clone(), route.fallback))
            .collect();
        for route in self.remote.routes() {
            if seen.insert((route.host.clone(), route.fallback)) {
                routes.push(route);
            }
        }
        routes
    }

    /// Routes of this machine's own lanes.
    pub(crate) fn local_route_entries(&self) -> Vec<RouteEntry> {
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

    /// Hands the current snapshot to the proxy and the `/etc/hosts` fallback.
    pub(crate) fn publish_routes(&self) {
        let routes = self.route_entries();
        self.sync_hosts(&routes);
        self.integ.routes.publish(routes);
    }

    /// `hosts`: every route with its lane, unit, port and state.
    pub(crate) fn hosts(&self) -> Value {
        let https_port = self.integ.routes.https_port();
        let items: Vec<Value> = self
            .route_entries()
            .into_iter()
            .map(|route| {
                let state = match &route.status {
                    RouteStatus::Ready => "ready",
                    RouteStatus::Idle => "idle",
                    RouteStatus::Starting => "starting",
                    RouteStatus::Failed { .. } => "failed",
                    RouteStatus::Reconnecting => "reconnecting",
                };
                // Wildcards have no single address to open.
                let url = (!route.host.contains('*'))
                    .then(|| devlanes_resolve::https_url(&route.host, https_port));
                json!({
                    "host": route.host,
                    "url": url,
                    "project": route.project,
                    "lane": route.lane,
                    "unit": route.unit,
                    "port": route.port,
                    "fallback": route.fallback,
                    "host_header_upstream": route.host_header_upstream,
                    "state": state,
                })
            })
            .collect();
        json!({"items": items, "proxy": self.integ.routes.status(), "truncated": false, "next_cursor": null})
    }
}
