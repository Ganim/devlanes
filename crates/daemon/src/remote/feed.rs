//! Box side: `remote.registry`, the registry a connected client mirrors.
//!
//! The registry comes from the same lane summaries as `status --all` and the
//! same routes as `hosts` (this machine's own lanes only): assigned lanes, their
//! services, tasks and started containers, with ports, hostnames, hidden flags
//! and states, never environments or secrets. Tasks carry their state only.

use std::{sync::Arc, time::Duration};

use devlanes_proto::{
    Event, LaneSummary, RpcError, events,
    remote::{REGISTRY_CHANGE_EVENT, REGISTRY_SNAPSHOT_EVENT, RegistryParams, RemoteClient},
};
use devlanes_transport::{
    HostHeader, HostInfo, LaneInfo, LaneKey, PortInfo, Registry, RegistryEvent, RegistrySnapshot,
    UnitInfo, UnitKey, UnitKind,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::{ClientGuard, ClientKey};
use crate::{
    Session,
    engine::{Inner, params},
    integrations::RouteEntry,
    model::lock,
};

/// Changes arriving within this window cause one rebuild.
const DEBOUNCE: Duration = Duration::from_millis(100);

impl Inner {
    /// `remote.registry`: sends the registry, then each change, until the
    /// tunnel leaves or the daemon shuts down. A tunnel that reads too slowly
    /// never holds up the daemon: its bus position overflows instead, and it
    /// gets a fresh snapshot.
    pub(crate) async fn remote_registry(
        self: &Arc<Self>,
        value: Value,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: RegistryParams = params(value)?;
        let _listed = args.client.map(|client| ClientGuard::add(self, client));
        // Subscribed before the snapshot is built, so no change falls between them.
        let mut changes = self.bus.subscribe();
        let disconnected = session.disconnected();
        let mut current = self.registry_now();
        if send_snapshot(session, &current).await.is_err() {
            return Ok(json!({}));
        }
        loop {
            let mut lagged = tokio::select! {
                () = disconnected.cancelled() => return Ok(json!({})),
                () = self.shutdown.cancelled() => return Ok(json!({})),
                change = next_change(&mut changes) => match change {
                    Some(lagged) => lagged,
                    None => return Ok(json!({})),
                },
            };
            // A start or stop emits a burst of events; one rebuild covers it.
            let settle = tokio::time::sleep(DEBOUNCE);
            tokio::pin!(settle);
            loop {
                tokio::select! {
                    () = &mut settle => break,
                    () = disconnected.cancelled() => return Ok(json!({})),
                    () = self.shutdown.cancelled() => return Ok(json!({})),
                    next = changes.recv() => match next {
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => lagged = true,
                        Err(broadcast::error::RecvError::Closed) => return Ok(json!({})),
                    },
                }
            }
            let next = self.registry_now();
            let sent = if lagged {
                send_snapshot(session, &next).await
            } else {
                send_changes(session, current.diff(&next)).await
            };
            if sent.is_err() {
                return Ok(json!({}));
            }
            current = next;
        }
    }

    /// The registry as of now, from `status --all`'s lanes and this machine's routes.
    fn registry_now(&self) -> Registry {
        let lanes = self.summaries(&self.all_lanes(), None);
        let routes = self.local_route_entries();
        build(lanes, &routes)
    }
}

impl ClientGuard {
    /// Lists `client` in `status` until the guard drops, replacing an entry
    /// with the same id: a client that reconnects before the box noticed its
    /// old connection drop is listed once.
    fn add(engine: &Arc<Inner>, client: RemoteClient) -> Self {
        let serial = engine
            .remote
            .next_client
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let client = RemoteClient {
            id: clean(&client.id),
            name: clean(&client.name),
            ..client
        };
        let key = if client.id.is_empty() {
            ClientKey::Anonymous(serial)
        } else {
            ClientKey::Id(client.id.clone())
        };
        lock(&engine.remote.clients).insert(key.clone(), (serial, client));
        Self {
            engine: engine.clone(),
            key,
            serial,
        }
    }
}

/// Longest client id or name the box keeps, in bytes.
const MAX_CLIENT_TEXT: usize = 256;

/// `text` without control characters, cut to [`MAX_CLIENT_TEXT`] bytes. The
/// box shows what clients send in `status`, so a client must not be able to
/// fill it or put terminal escapes in it.
fn clean(text: &str) -> String {
    let mut cleaned = String::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        if cleaned.len() + c.len_utf8() > MAX_CLIENT_TEXT {
            break;
        }
        cleaned.push(c);
    }
    cleaned
}

/// Waits for a bus event that may change the registry: `Some(true)` when
/// events were lost to an overflow, `None` once the bus closed.
async fn next_change(changes: &mut broadcast::Receiver<Event>) -> Option<bool> {
    loop {
        match changes.recv().await {
            Ok(event) if relevant(&event) => return Some(false),
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => return Some(true),
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// Log lines and remote bookkeeping never change lanes, units or ports.
fn relevant(event: &Event) -> bool {
    event.event != events::LOG && !event.event.starts_with("remote.")
}

/// Sends the whole registry.
async fn send_snapshot(session: &Session, registry: &Registry) -> Result<(), RpcError> {
    session
        .event(Event {
            seq: 0,
            event: REGISTRY_SNAPSHOT_EVENT.into(),
            data: json!({"snapshot": registry.snapshot()}),
        })
        .await
}

/// Sends each change in order.
async fn send_changes(session: &Session, changes: Vec<RegistryEvent>) -> Result<(), RpcError> {
    for change in changes {
        session
            .event(Event {
                seq: 0,
                event: REGISTRY_CHANGE_EVENT.into(),
                data: json!({"change": change}),
            })
            .await?;
    }
    Ok(())
}

/// Maps lane summaries and routes to the registry: assigned lanes, their
/// services and started containers with ports, hostnames and hidden flags, and
/// their tasks with state only. Exec units are left out.
pub(crate) fn build(lanes: Vec<LaneSummary>, routes: &[RouteEntry]) -> Registry {
    let hosts_of = |lane: &str, unit: &str| -> Vec<HostInfo> {
        routes
            .iter()
            .filter(|route| route.lane == lane && route.unit == unit)
            .map(|route| HostInfo {
                host: route.host.clone(),
                port: route.port,
                fallback: route.fallback,
                host_header: if route.host_header_upstream {
                    HostHeader::Upstream
                } else {
                    HostHeader::Keep
                },
            })
            .collect()
    };
    let ports = |ports: &std::collections::BTreeMap<String, u16>| -> Vec<PortInfo> {
        ports
            .iter()
            .map(|(name, port)| PortInfo {
                name: name.clone(),
                port: *port,
            })
            .collect()
    };
    let mut snapshot = RegistrySnapshot::default();
    for lane in lanes {
        // Unassigned lanes have no ports or hostnames to mirror.
        let Some(index) = lane.index else { continue };
        let key = LaneKey {
            project: lane.project.clone(),
            lane: index.to_string(),
        };
        snapshot.lanes.push(LaneInfo {
            key: key.clone(),
            index,
            domain: lane.domain.clone(),
        });
        for unit in &lane.units {
            let kind = match unit.kind.as_str() {
                "" | "service" => UnitKind::Service,
                "task" => UnitKind::Task,
                _ => continue,
            };
            let endpoints = kind.has_endpoints();
            snapshot.units.push(UnitInfo {
                key: UnitKey {
                    lane: key.clone(),
                    kind,
                    name: unit.name.clone(),
                },
                hidden: unit.hidden,
                state: unit.state.clone(),
                ports: if endpoints {
                    ports(&unit.ports)
                } else {
                    Vec::new()
                },
                hosts: if endpoints {
                    hosts_of(&lane.reference, &unit.name)
                } else {
                    Vec::new()
                },
            });
        }
        for container in &lane.containers {
            snapshot.units.push(UnitInfo {
                key: UnitKey {
                    lane: key.clone(),
                    kind: UnitKind::Container,
                    name: container.name.clone(),
                },
                hidden: container.hidden,
                state: "running".into(),
                ports: ports(&container.ports),
                hosts: hosts_of(&lane.reference, &container.name),
            });
        }
    }
    Registry::from_snapshot(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::RouteStatus;

    /// Client ids and names lose control characters and are cut on a
    /// character boundary.
    #[test]
    fn client_text_is_cleaned_and_capped() {
        assert_eq!(clean("lap\u{1b}[31mtop\n"), "lap[31mtop");
        let long = "é".repeat(MAX_CLIENT_TEXT);
        assert_eq!(
            clean(&long).len(),
            MAX_CLIENT_TEXT,
            "128 two-byte characters fit"
        );
        assert_eq!(
            clean(&format!("{}x", "a".repeat(MAX_CLIENT_TEXT))).len(),
            MAX_CLIENT_TEXT
        );
    }

    #[test]
    fn maps_services_tasks_containers_hosts_and_hidden_flags() {
        let lanes: Vec<LaneSummary> = serde_json::from_value(json!([{
            "reference": "app@1", "project": "app", "index": 1, "branch": "x",
            "path": "/w", "state": "running", "config_changed": false,
            "domain": "app-1.localhost",
            "units": [
                {"name": "web", "lane": "app@1", "state": "ready", "pid": 1, "exit_code": null,
                 "ports": {"default": 4100}, "urls": [], "actor": {"kind": "human"},
                 "reason": null, "kind": "service"},
                {"name": "admin", "lane": "app@1", "state": "ready", "pid": 2, "exit_code": null,
                 "ports": {"default": 4101}, "urls": [], "actor": {"kind": "human"},
                 "reason": null, "kind": "service", "hidden": true},
                {"name": "worker", "lane": "app@1", "state": "stopped", "pid": null, "exit_code": null,
                 "ports": {"default": 4103}, "urls": [], "actor": {"kind": "human"},
                 "reason": null, "kind": "service"},
                {"name": "migrate", "lane": "app@1", "state": "running", "pid": 3, "exit_code": null,
                 "ports": {"default": 4104}, "urls": [], "actor": {"kind": "human"}, "reason": null,
                 "kind": "task"},
                {"name": "shell", "lane": "app@1", "state": "running", "pid": 4, "exit_code": null,
                 "ports": {}, "urls": [], "actor": {"kind": "human"}, "reason": null, "kind": "exec"}
            ],
            "containers": [{"name": "db", "ports": {"5432": 4102}, "hidden": false}]
        }, {
            "reference": "app@feature", "project": "app", "index": null, "branch": null,
            "path": "/v", "state": "unassigned", "config_changed": false, "units": []
        }]))
        .expect("lane summaries");
        let route = |host: &str, unit: &str, port: u16| RouteEntry {
            host: host.into(),
            port,
            fallback: false,
            host_header_upstream: true,
            status: RouteStatus::Ready,
            start_hint: String::new(),
            project: "app".into(),
            lane: "app@1".into(),
            unit: unit.into(),
            certificate_names: Vec::new(),
            remote: false,
            box_https_port: None,
        };
        let routes = [
            route("web.app-1.localhost", "web", 4100),
            route("migrate.app-1.localhost", "migrate", 4104),
        ];
        let registry = build(lanes, &routes);
        assert_eq!(registry.lanes().count(), 1);
        let units: Vec<_> = registry.units().collect();
        assert_eq!(units.len(), 5, "exec skipped: {units:?}");
        let web = units
            .iter()
            .find(|u| u.key.name == "web")
            .expect("web is listed");
        assert_eq!(web.hosts.len(), 1);
        assert_eq!(web.hosts[0].host_header, HostHeader::Upstream);
        let migrate = units
            .iter()
            .find(|u| u.key.name == "migrate")
            .expect("the task is listed");
        assert_eq!(migrate.key.kind, UnitKind::Task);
        assert_eq!(migrate.state, "running");
        assert!(
            migrate.ports.is_empty() && migrate.hosts.is_empty(),
            "tasks carry their state only: {migrate:?}"
        );
        let mirrored: Vec<u16> = registry.mirrored_ports().keys().copied().collect();
        assert_eq!(
            mirrored,
            [4100, 4102],
            "hidden admin, stopped worker and the task are not mirrored"
        );
        let advertised: Vec<u16> = registry.advertised_ports().into_iter().collect();
        assert_eq!(
            advertised,
            [4100, 4102, 4103],
            "the idle worker's port is kept free"
        );
    }
}
