//! Makes a box's non-hidden endpoints local (plan 14.4): every allocated port of
//! its services and containers is bound on this machine's loopback addresses under
//! the same number, and its hostnames join the local proxy's routing table. Each
//! accepted connection becomes one mux data stream to the box.
//!
//! Listeners stay bound while the link reconnects; connections accepted meanwhile
//! wait briefly for the new session and the proxy shows a reconnecting page.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Weak},
    time::Duration,
};

use devlanes_proto::{events, remote as wire};
use devlanes_transport::{
    DialTarget, HostHeader, LaneKey, LinkState, Registry, UnitInfo, UnitKind,
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpSocket, TcpStream},
    sync::broadcast,
    task::JoinHandle,
};

use super::HostConn;
use crate::{
    engine::Inner,
    integrations::{RouteEntry, RouteStatus},
    model::lock,
};

/// How long a connection accepted during a reconnect waits for the new session.
const HOLD: Duration = Duration::from_secs(10);

/// What a host mirrors right now, for routes and status output.
#[derive(Debug, Clone, Default)]
pub(super) struct View {
    pub routes: Vec<RouteEntry>,
    pub ports: Vec<Value>,
    pub skipped: Vec<Value>,
}

/// One accept loop; aborted when the listener is no longer wanted.
struct Bound {
    address: IpAddr,
    task: JoinHandle<()>,
}

impl Drop for Bound {
    /// Stops accepting; the listener closes with the task.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A mirrored box port and the local addresses it holds.
struct PortMirror {
    unit: String,
    lane: String,
    kind: UnitKind,
    bound: Vec<Bound>,
    /// Addresses whose bind failed, with what the port inspector found.
    conflicts: BTreeMap<IpAddr, Value>,
}

impl PortMirror {
    /// Whether `127.0.0.1:<port>` reaches the box, which the proxy dials first.
    fn owns_v4(&self) -> bool {
        self.bound
            .iter()
            .any(|bound| bound.address == IpAddr::V4(Ipv4Addr::LOCALHOST))
    }
}

/// Mirror state owned by one host's mirror task.
struct Mirror {
    conn: Arc<HostConn>,
    addresses: Vec<IpAddr>,
    ports: BTreeMap<u16, PortMirror>,
    /// Ephemeral `127.0.0.1` listeners for routed box ports this machine could not
    /// bind under their own number: box port -> listener.
    relays: BTreeMap<u16, (u16, Bound)>,
    skipped: BTreeSet<LaneKey>,
}

/// Keeps the host's listeners and routes in step with its registry, the link
/// state and local lanes until the host is disconnected.
pub(super) async fn run(engine: Weak<Inner>, conn: Arc<HostConn>) {
    let Some(inner) = engine.upgrade() else {
        return;
    };
    let mut mirror = Mirror {
        conn: conn.clone(),
        addresses: inner.integ.remote.mirror_addresses(),
        ports: BTreeMap::new(),
        relays: BTreeMap::new(),
        skipped: BTreeSet::new(),
    };
    let mut bus = inner.bus.subscribe();
    drop(inner);
    let mut registry = conn.registry.subscribe();
    let mut state = conn.state.clone();
    loop {
        let Some(inner) = engine.upgrade() else { break };
        let current = registry.borrow_and_update().clone();
        let connected = matches!(&*state.borrow_and_update(), LinkState::Connected(_));
        mirror.reconcile(&inner, &current, connected).await;
        drop(inner);
        // Local lanes matter because their hostnames win over the box's.
        let wake = async {
            loop {
                match bus.recv().await {
                    Ok(event)
                        if event.event == events::LANE_CHANGED
                            || event.event == events::LANE_REMOVED =>
                    {
                        return true;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => return true,
                    Err(broadcast::error::RecvError::Closed) => return false,
                }
            }
        };
        let go_on = tokio::select! {
            () = conn.cancel.cancelled() => false,
            changed = registry.changed() => changed.is_ok(),
            changed = state.changed() => changed.is_ok(),
            more = wake => more,
        };
        if !go_on {
            break;
        }
    }
    // Listeners and relays close as `mirror` drops.
}

impl Mirror {
    /// Binds new ports, drops stale ones, retries conflicts and republishes routes.
    async fn reconcile(&mut self, engine: &Arc<Inner>, registry: &Registry, connected: bool) {
        let local_hosts: BTreeSet<String> = engine
            .local_route_entries()
            .into_iter()
            .map(|route| route.host)
            .collect();
        let visible: Vec<&UnitInfo> = registry.units().filter(|unit| !unit.hidden).collect();

        // A box lane whose hostname this machine already serves is skipped whole:
        // renaming it would break the CORS origins and cookies the box chose.
        let mut collisions: BTreeMap<LaneKey, BTreeSet<String>> = BTreeMap::new();
        for unit in &visible {
            for host in &unit.hosts {
                let name = host.host.to_ascii_lowercase();
                if local_hosts.contains(&name) {
                    collisions
                        .entry(unit.key.lane.clone())
                        .or_default()
                        .insert(name);
                }
            }
        }
        for (lane, hosts) in &collisions {
            if !self.skipped.contains(lane) {
                tracing::warn!(host = %self.conn.name, lane = %lane_ref(lane), ?hosts, "lane skipped: its hostnames are served locally");
                engine.bus.emit(
                    wire::WARNING_EVENT,
                    json!({
                        "host": self.conn.name,
                        "code": "lane_skipped",
                        "lane": lane_ref(lane),
                        "hosts": hosts,
                    }),
                );
            }
        }
        self.skipped = collisions.keys().cloned().collect();
        let mirrored: Vec<&UnitInfo> = visible
            .into_iter()
            .filter(|unit| !self.skipped.contains(&unit.key.lane))
            .collect();

        let mut wanted: BTreeMap<u16, &UnitInfo> = BTreeMap::new();
        for unit in &mirrored {
            for port in &unit.ports {
                wanted.entry(port.port).or_insert(unit);
            }
        }
        self.ports.retain(|port, _| wanted.contains_key(port));
        for (port, unit) in &wanted {
            let mirror = self.ports.entry(*port).or_insert_with(|| PortMirror {
                unit: unit.key.name.clone(),
                lane: lane_ref(&unit.key.lane),
                kind: unit.key.kind,
                bound: Vec::new(),
                conflicts: BTreeMap::new(),
            });
            let missing: Vec<IpAddr> = self
                .addresses
                .iter()
                .copied()
                .filter(|address| !mirror.bound.iter().any(|b| b.address == *address))
                .collect();
            for address in missing {
                match bind(address, *port) {
                    Ok(listener) => {
                        mirror.conflicts.remove(&address);
                        mirror.bound.push(Bound {
                            address,
                            task: tokio::spawn(accept_loop(listener, *port, self.conn.clone())),
                        });
                    }
                    // No IPv6 loopback on this machine: nothing to mirror there.
                    Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => {}
                    Err(error) => {
                        let pid = if error.kind() == std::io::ErrorKind::AddrInUse {
                            engine.integ.ports.holder(*port).await
                        } else {
                            None
                        };
                        if !mirror.conflicts.contains_key(&address) {
                            tracing::warn!(host = %self.conn.name, port, %address, ?pid, %error, "cannot mirror port");
                            engine.bus.emit(
                                wire::WARNING_EVENT,
                                json!({
                                    "host": self.conn.name,
                                    "code": "port_in_use",
                                    "port": port,
                                    "address": address,
                                    "pid": pid,
                                    "unit": mirror.unit,
                                    "lane": mirror.lane,
                                }),
                            );
                        }
                        mirror.conflicts.insert(
                            address,
                            json!({"address": address, "pid": pid, "error": error.to_string()}),
                        );
                    }
                }
            }
        }

        // Hostnames: the proxy dials 127.0.0.1 first, so a routed port this machine
        // does not hold there gets a relay on an ephemeral port instead.
        let mut routes = Vec::new();
        let mut relayed = BTreeSet::new();
        for unit in &mirrored {
            let lane = registry
                .lanes()
                .find(|lane| lane.key == unit.key.lane)
                .cloned();
            for host in &unit.hosts {
                let owned = self.ports.get(&host.port).is_some_and(PortMirror::owns_v4);
                let local_port = if owned {
                    host.port
                } else {
                    relayed.insert(host.port);
                    match self.relay(host.port) {
                        Some(port) => port,
                        None => continue,
                    }
                };
                let reference = lane_ref(&unit.key.lane);
                routes.push(RouteEntry {
                    host: host.host.to_ascii_lowercase(),
                    port: local_port,
                    fallback: host.fallback,
                    host_header_upstream: host.host_header == HostHeader::Upstream,
                    status: route_status(&unit.state, connected),
                    start_hint: format!(
                        "devlanes --host {} start --lane {reference}",
                        self.conn.name
                    ),
                    project: unit.key.lane.project.clone(),
                    lane: reference,
                    unit: unit.key.name.clone(),
                    certificate_names: lane
                        .as_ref()
                        .and_then(|lane| lane.domain.clone())
                        .into_iter()
                        .collect(),
                });
            }
        }
        self.relays.retain(|port, _| relayed.contains(port));

        let view = View {
            routes,
            ports: self
                .ports
                .iter()
                .map(|(port, mirror)| {
                    json!({
                        "port": port,
                        "lane": mirror.lane,
                        "unit": mirror.unit,
                        "kind": mirror.kind,
                        "addresses": mirror.bound.iter().map(|b| b.address).collect::<Vec<_>>(),
                        "conflicts": mirror.conflicts.values().collect::<Vec<_>>(),
                        "relay": self.relays.get(port).map(|(local, _)| local),
                    })
                })
                .collect(),
            skipped: collisions
                .iter()
                .map(|(lane, hosts)| json!({"lane": lane_ref(lane), "hosts": hosts}))
                .collect(),
        };
        let routes_changed = {
            let mut current = lock(&self.conn.view);
            let changed = current.routes != view.routes;
            *current = view;
            changed
        };
        if routes_changed {
            engine.publish_routes();
        }
    }

    /// The local relay port for box port `port`, binding one on first use.
    fn relay(&mut self, port: u16) -> Option<u16> {
        if let Some((local, _)) = self.relays.get(&port) {
            return Some(*local);
        }
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let listener = match bind(address, 0) {
            Ok(listener) => listener,
            Err(error) => {
                tracing::warn!(host = %self.conn.name, port, %error, "cannot bind a route relay");
                return None;
            }
        };
        let local = listener.local_addr().ok()?.port();
        let task = tokio::spawn(accept_loop(listener, port, self.conn.clone()));
        self.relays.insert(port, (local, Bound { address, task }));
        Some(local)
    }
}

/// `project@lane` for a box lane key.
fn lane_ref(lane: &LaneKey) -> String {
    format!("{}@{}", lane.project, lane.lane)
}

/// Proxy state for a box unit: reconnecting while the link is down.
fn route_status(state: &str, connected: bool) -> RouteStatus {
    if !connected {
        return RouteStatus::Reconnecting;
    }
    match state {
        "ready" | "running" => RouteStatus::Ready,
        "starting" => RouteStatus::Starting,
        "failed" => RouteStatus::Failed {
            log_tail: String::new(),
        },
        _ => RouteStatus::Idle,
    }
}

/// Binds without `SO_REUSEADDR`: on macOS that option would let this listener
/// shadow another process bound to the wildcard address instead of reporting it.
fn bind(address: IpAddr, port: u16) -> std::io::Result<TcpListener> {
    let socket = match address {
        IpAddr::V4(_) => TcpSocket::new_v4()?,
        IpAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(false)?;
    socket.bind(SocketAddr::new(address, port))?;
    socket.listen(1024)
}

/// Accepts local connections and splices each into a data stream to the box.
async fn accept_loop(listener: TcpListener, port: u16, conn: Arc<HostConn>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(splice(stream, port, conn.clone()));
            }
            Err(error) => {
                // Out of descriptors and similar: back off instead of spinning.
                tracing::debug!(%error, port, "mirror accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One local connection to box port `port`.
async fn splice(mut stream: TcpStream, port: u16, conn: Arc<HostConn>) {
    let session = match conn.session() {
        Some(session) => session,
        None => match conn.session_within(HOLD).await {
            Some(session) => session,
            None => return,
        },
    };
    match session.open(DialTarget::Port { port }).await {
        Ok(mut remote) => {
            if let Err(error) = tokio::io::copy_bidirectional(&mut stream, &mut remote).await {
                tracing::debug!(%error, port, "mirrored connection ended");
            }
        }
        Err(error) => {
            tracing::debug!(%error, port, host = %conn.name, "box refused a mirrored connection")
        }
    }
}
