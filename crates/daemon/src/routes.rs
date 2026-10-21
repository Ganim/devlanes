//! Routing snapshot for the proxy (plan 6.2): every lane's hostnames, with idle and
//! failed states so the proxy can render a page instead of a bare 502.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
};

use devlanes_proto::LaneWarning;
use serde_json::{Value, json};

use crate::{
    engine::Inner,
    integrations::{RouteEntry, RouteStatus},
    model::{LaneRt, LaneSt, ProjectRt},
    units::UnitState,
};

/// Coalesces route publications. Callers mark the snapshot dirty; one caller at
/// a time rebuilds it from the current state and hands it to the sinks until no
/// change arrived meanwhile, so the last snapshot published is never older than
/// the last change, and a burst of changes costs one rebuild per pass.
#[derive(Default)]
pub(crate) struct RoutePublisher {
    dirty: AtomicBool,
    running: Mutex<()>,
}

/// This machine's routes plus what the snapshot left out.
pub(crate) struct LocalRoutes {
    pub entries: Vec<RouteEntry>,
    /// `host_conflict` warnings, attached to both lanes of each conflict.
    pub lane_conflicts: Vec<(Arc<LaneRt>, LaneWarning)>,
    /// Every `host_conflict` once, then every `host_rejected`.
    pub warnings: Vec<LaneWarning>,
}

/// The owner of a claimed hostname.
struct Claim {
    reference: String,
    lane: Arc<LaneRt>,
    running: bool,
}

impl Inner {
    /// Builds the complete route list; local lanes come first, then hostnames
    /// mirrored from connected boxes, and the first claim of a host keeps it.
    pub(crate) fn route_entries(&self) -> Vec<RouteEntry> {
        self.with_remote_routes(self.local_routes().entries)
    }

    /// Appends mirrored routes whose hostnames no local route claims.
    fn with_remote_routes(&self, mut routes: Vec<RouteEntry>) -> Vec<RouteEntry> {
        let key = |route: &RouteEntry| {
            let host = route.host.trim_end_matches('.').to_ascii_lowercase();
            (host, route.fallback)
        };
        let mut seen: BTreeSet<(String, bool)> = routes.iter().map(key).collect();
        for route in self.remote.routes() {
            if seen.insert(key(&route)) {
                routes.push(route);
            }
        }
        routes
    }

    /// Routes of this machine's own lanes.
    pub(crate) fn local_route_entries(&self) -> Vec<RouteEntry> {
        self.local_routes().entries
    }

    /// Routes of this machine's own lanes. A hostname two lanes claim goes to the
    /// lane with running units, else to the first by project name, lane index and
    /// path; both lanes get a `host_conflict` warning. Names that are not valid
    /// DNS names are left out with a `host_rejected` warning.
    pub(crate) fn local_routes(&self) -> LocalRoutes {
        let mut lanes: Vec<_> = self
            .all_lanes()
            .into_iter()
            .map(|(project, lane)| {
                let name = project.name();
                let (running, index, path) = {
                    let st = lane.st();
                    (st.any_running(), st.index(), st.path.clone())
                };
                (
                    (!running, name, index.is_none(), index, path),
                    project,
                    lane,
                )
            })
            .collect();
        lanes.sort_by(|a, b| a.0.cmp(&b.0));
        let mut claims: BTreeMap<(String, bool), Claim> = BTreeMap::new();
        let mut routes = Vec::new();
        let mut lane_conflicts = Vec::new();
        let mut conflicts = Vec::new();
        let mut rejected = Vec::new();
        for (_, project, lane) in lanes {
            let name = project.name();
            let st = lane.st();
            let Some(view) = &st.view else { continue };
            let reference = st.reference(&name);
            let hint = format!("devlanes start --lane {reference}");
            let running = st.any_running();
            let service_routes = view.routes.iter().map(|route| {
                let status = route_status(&st, &route.unit);
                // A live unit keeps the ports it was spawned with; the view
                // follows the config, which may have changed since (plan 5.8).
                let port = st
                    .units
                    .get(&route.unit)
                    .filter(|rec| rec.host.is_alive())
                    .and_then(|rec| rec.ports.get(&route.port_name).copied())
                    .unwrap_or(route.port);
                (
                    route.host.as_str(),
                    &route.unit,
                    port,
                    route.fallback,
                    route.host_header_upstream,
                    status,
                )
            });
            let infra_routes = view.infra_routes.iter().map(|route| {
                (
                    route.host.as_str(),
                    &route.container,
                    route.port,
                    route.fallback,
                    route.host_header_upstream,
                    RouteStatus::Ready,
                )
            });
            for (host, unit, port, fallback, host_header_upstream, status) in
                service_routes.chain(infra_routes)
            {
                let host = match devlanes_config::normalize_host(host) {
                    Ok(host) => host,
                    Err(message) => {
                        rejected.push(LaneWarning {
                            code: "host_rejected".into(),
                            data: json!({
                                "lane": reference,
                                "unit": unit,
                                "host": host,
                                "message": format!("{message}; the route is left out"),
                            }),
                        });
                        continue;
                    }
                };
                if let Some(claim) = claims.get(&(host.clone(), fallback)) {
                    if claim.reference != reference {
                        let why = if claim.running && !running {
                            "it has running units"
                        } else {
                            "it comes first by project and lane index"
                        };
                        let warning = LaneWarning {
                            code: "host_conflict".into(),
                            data: json!({
                                "host": host,
                                "lane": claim.reference,
                                "other": reference,
                                "message": format!(
                                    "{host} is routed by {} and {reference}; {} keeps it because {why}",
                                    claim.reference, claim.reference
                                ),
                            }),
                        };
                        lane_conflicts.push((claim.lane.clone(), warning.clone()));
                        lane_conflicts.push((lane.clone(), warning.clone()));
                        conflicts.push(warning);
                    }
                    continue;
                }
                claims.insert(
                    (host.clone(), fallback),
                    Claim {
                        reference: reference.clone(),
                        lane: lane.clone(),
                        running,
                    },
                );
                routes.push(RouteEntry {
                    host,
                    port,
                    fallback,
                    host_header_upstream,
                    status,
                    start_hint: hint.clone(),
                    project: name.clone(),
                    lane: reference.clone(),
                    unit: unit.clone(),
                    certificate_names: vec![view.domain.clone()],
                });
            }
        }
        conflicts.extend(rejected);
        LocalRoutes {
            entries: routes,
            lane_conflicts,
            warnings: conflicts,
        }
    }

    /// Marks the route snapshot dirty and publishes it unless another caller is
    /// publishing right now, in which case that caller picks the change up.
    pub(crate) fn publish_routes(&self) {
        let publisher = &self.route_publisher;
        publisher.dirty.store(true, Ordering::SeqCst);
        loop {
            {
                let _running = match publisher.running.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => return,
                };
                while publisher.dirty.swap(false, Ordering::SeqCst) {
                    self.publish_snapshot();
                }
            }
            // A change marked after the last pass, while this caller still held
            // the lock, was left to this caller.
            if !publisher.dirty.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    /// Rebuilds the snapshot from current state, records each lane's host
    /// conflicts and hands the routes to `/etc/hosts` and the proxy.
    fn publish_snapshot(&self) {
        let local = self.local_routes();
        let mut changed: Vec<(Arc<ProjectRt>, Arc<LaneRt>)> = Vec::new();
        for (project, lane) in self.all_lanes() {
            let conflicts: Vec<LaneWarning> = local
                .lane_conflicts
                .iter()
                .filter(|(owner, _)| Arc::ptr_eq(owner, &lane))
                .map(|(_, warning)| warning.clone())
                .collect();
            let mut st = lane.st();
            if st.host_conflicts != conflicts {
                st.host_conflicts = conflicts;
                drop(st);
                changed.push((project, lane));
            }
        }
        for (project, lane) in changed {
            self.emit_lane_event(&project, &lane);
        }
        let routes = self.with_remote_routes(local.entries);
        self.sync_hosts(&routes);
        self.integ.routes.publish(routes);
    }

    /// `host_conflict` and `host_rejected` warnings of the local routes, for
    /// `hosts` and `doctor`.
    pub(crate) fn route_warnings(&self) -> Vec<LaneWarning> {
        self.local_routes().warnings
    }

    /// The lane's `ports_pending` and `host_conflict` warnings shaped for
    /// `config check`: `{path, code, message, ...data}`.
    pub(crate) fn config_check_warnings(&self, reference: &str, lane: &LaneSt) -> Vec<Value> {
        lane.warnings(reference)
            .into_iter()
            .map(|warning| {
                let path = match warning.code.as_str() {
                    "host_conflict" => lane
                        .view
                        .as_ref()
                        .and_then(|view| {
                            view.routes
                                .iter()
                                .find(|route| route.host == warning.data["host"])
                                .map(|route| format!("services.{}.hosts", route.service))
                        })
                        .unwrap_or_else(|| "infra.hosts".into()),
                    _ => "lanes".into(),
                };
                let mut item = json!({"path": path, "code": warning.code});
                if let (Some(item), Some(data)) = (item.as_object_mut(), warning.data.as_object()) {
                    for (key, value) in data {
                        item.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
                item
            })
            .collect()
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
        json!({
            "items": items,
            "warnings": self.route_warnings(),
            "proxy": self.integ.routes.status(),
            "truncated": false,
            "next_cursor": null,
        })
    }
}

/// What the proxy shows for a unit's route.
fn route_status(st: &LaneSt, unit: &str) -> RouteStatus {
    match st.units.get(unit) {
        Some(rec) => match rec.host.state() {
            UnitState::Ready => RouteStatus::Ready,
            UnitState::Starting => RouteStatus::Starting,
            UnitState::Failed(_) => RouteStatus::Failed {
                log_tail: rec.failure_tail.join("\n"),
            },
            UnitState::Stopping | UnitState::Exited(_) => RouteStatus::Idle,
        },
        // A previous daemon's process may still serve the port.
        None if st.orphans.contains_key(unit) => RouteStatus::Ready,
        None => RouteStatus::Idle,
    }
}
