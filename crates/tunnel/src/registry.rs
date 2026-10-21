//! The box registry, built from the daemon's `status --all` and `hosts`.

use std::time::Duration;

use async_trait::async_trait;
use devlanes_client::{Client, Frame, Paths};
use devlanes_proto::{
    Context, Control, Event, LaneSummary, Method, Page, events, remote::RemoteClient,
};
use devlanes_transport::{
    BoxError, HostHeader, HostInfo, LaneInfo, LaneKey, PortInfo, Registry, RegistryFeed,
    RegistrySnapshot, RegistrySource, RegistryUpdate, UnitInfo, UnitKey, UnitKind,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Events arriving within this window cause one refresh.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// [`RegistrySource`] over the box daemon's socket.
pub(crate) struct DaemonRegistry {
    paths: Paths,
    client: RemoteClient,
}

impl DaemonRegistry {
    /// Reads the daemon behind `paths`; `client` describes this tunnel to `status`.
    pub(crate) fn new(paths: Paths, client: RemoteClient) -> Self {
        Self { paths, client }
    }
}

#[async_trait]
impl RegistrySource for DaemonRegistry {
    /// Subscribes to events first, so no change falls between the snapshot and them.
    async fn subscribe(&self) -> Result<RegistryFeed, BoxError> {
        let mut events = Client::connect(&self.paths).await?;
        let params = json!({"follow": true, "remote_client": self.client});
        events
            .begin_request(Method::Subscribe, params, Context::default())
            .await?;
        let mut query = Client::connect(&self.paths).await?;
        let mut current = fetch(&mut query).await?;
        let snapshot = current.snapshot();
        let (updates, receiver) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(()) = changed(&mut events).await {
                let next = match fetch(&mut query).await {
                    Ok(next) => next,
                    Err(error) => {
                        tracing::debug!(%error, "registry refresh failed");
                        return;
                    }
                };
                for event in current.diff(&next) {
                    if updates.send(RegistryUpdate::Event(event)).await.is_err() {
                        return;
                    }
                }
                current = next;
            }
            // The daemon connection ended: closing the feed ends the session, and
            // the client reconnects to a fresh tunnel.
        });
        let updates = futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|update| (update, receiver))
        })
        .boxed();
        Ok(RegistryFeed { snapshot, updates })
    }
}

/// Waits for an event that may change the registry, then lets a burst settle.
/// `None` once the subscription ended.
async fn changed(events: &mut Client) -> Option<()> {
    loop {
        let event = next_event(events).await?;
        if relevant(&event) {
            break;
        }
    }
    let settle = tokio::time::sleep(DEBOUNCE);
    tokio::pin!(settle);
    loop {
        tokio::select! {
            () = &mut settle => return Some(()),
            event = next_event(events) => {
                event?;
            }
        }
    }
}

/// Log lines and remote bookkeeping never change lanes, units or ports.
fn relevant(event: &Event) -> bool {
    event.event != events::LOG && !event.event.starts_with("remote.")
}

/// The next event of the subscription; `None` when it ended.
async fn next_event(events: &mut Client) -> Option<Event> {
    loop {
        match events.next_frame().await {
            Ok(Frame::Control(Control::Event(event))) => return Some(event),
            Ok(Frame::Control(Control::Response(_))) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// One `hosts` row.
#[derive(Debug, Deserialize)]
struct HostRow {
    host: String,
    lane: String,
    unit: String,
    port: u16,
    #[serde(default)]
    fallback: bool,
    #[serde(default)]
    host_header_upstream: bool,
}

/// Queries lanes and routes and maps them to transport types.
async fn fetch(client: &mut Client) -> Result<Registry, devlanes_client::Error> {
    let status = client
        .request(Method::Status, json!({"all": true}), Context::default())
        .await?;
    let hosts = client
        .request(Method::Hosts, json!({}), Context::default())
        .await?;
    Ok(build(status, &hosts))
}

/// Maps daemon output to the registry: assigned lanes, their services and started
/// containers with ports, hosts and hidden flags.
pub(crate) fn build(status: Value, hosts: &Value) -> Registry {
    let lanes = serde_json::from_value::<Page<LaneSummary>>(status)
        .map(|page| page.items)
        .unwrap_or_default();
    let rows: Vec<HostRow> = hosts
        .get("items")
        .cloned()
        .and_then(|items| serde_json::from_value(items).ok())
        .unwrap_or_default();
    let hosts_of = |lane: &str, unit: &str| -> Vec<HostInfo> {
        rows.iter()
            .filter(|row| row.lane == lane && row.unit == unit)
            .map(|row| HostInfo {
                host: row.host.clone(),
                port: row.port,
                fallback: row.fallback,
                host_header: if row.host_header_upstream {
                    HostHeader::Upstream
                } else {
                    HostHeader::Keep
                },
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
            if !(unit.kind.is_empty() || unit.kind == "service") {
                continue;
            }
            snapshot.units.push(UnitInfo {
                key: UnitKey {
                    lane: key.clone(),
                    kind: UnitKind::Service,
                    name: unit.name.clone(),
                },
                hidden: unit.hidden,
                state: unit.state.clone(),
                ports: unit
                    .ports
                    .iter()
                    .map(|(name, port)| PortInfo {
                        name: name.clone(),
                        port: *port,
                    })
                    .collect(),
                hosts: hosts_of(&lane.reference, &unit.name),
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
                ports: container
                    .ports
                    .iter()
                    .map(|(name, port)| PortInfo {
                        name: name.clone(),
                        port: *port,
                    })
                    .collect(),
                hosts: hosts_of(&lane.reference, &container.name),
            });
        }
    }
    Registry::from_snapshot(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_services_containers_hosts_and_hidden_flags() {
        let status = json!({"items": [{
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
                {"name": "migrate", "lane": "app@1", "state": "exited", "pid": null, "exit_code": 0,
                 "ports": {}, "urls": [], "actor": {"kind": "human"}, "reason": null, "kind": "task"}
            ],
            "containers": [{"name": "db", "ports": {"5432": 4102}, "hidden": false}]
        }, {
            "reference": "app@feature", "project": "app", "index": null, "branch": null,
            "path": "/v", "state": "unassigned", "config_changed": false, "units": []
        }], "truncated": false, "next_cursor": null});
        let hosts = json!({"items": [
            {"host": "web.app-1.localhost", "lane": "app@1", "unit": "web", "port": 4100,
             "fallback": false, "host_header_upstream": true, "state": "ready"}
        ]});
        let registry = build(status, &hosts);
        assert_eq!(registry.lanes().count(), 1);
        let units: Vec<_> = registry.units().collect();
        assert_eq!(units.len(), 3, "task skipped: {units:?}");
        let web = units
            .iter()
            .find(|u| u.key.name == "web")
            .expect("web is listed");
        assert_eq!(web.hosts.len(), 1);
        assert_eq!(web.hosts[0].host_header, HostHeader::Upstream);
        let mirrored: Vec<u16> = registry.mirrored_ports().keys().copied().collect();
        assert_eq!(mirrored, [4100, 4102], "hidden admin is not mirrored");
    }
}
