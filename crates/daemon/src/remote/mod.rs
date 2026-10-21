//! Remote operation (plan M14). Remote is mirroring only: a client never runs
//! commands on a box, and nothing in this path carries environments or secrets.
//!
//! Client side: `connect` keeps one reconnecting [`Link`] per box in the daemon,
//! mirrors the box's registry ([`mirror`]: same-number loopback listeners and proxy
//! routes) and applies its folder syncs ([`sync`]) below
//! `$DEVLANES_HOME/sync/<host>/`. Box side: `status` lists the tunnels serving
//! clients and `sync.status` lists the sync entries a tunnel may stream.

mod mirror;
mod sync;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_proto::{
    Event, RpcError,
    remote::{self as wire, RemoteClient, SyncEntry},
};
use devlanes_sync::{HashCache, RelPath};
use devlanes_transport::{ClientRegistry, Link, LinkState, Session as BoxSession, SessionOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    Session,
    engine::{Inner, required},
    integrations::{ConnectOptions, RemotePrompt, RemotePrompter, RouteEntry},
    model::lock,
};

/// `connect --background` returns once connected, failed, or after this long.
const CONNECT_WAIT: Duration = Duration::from_secs(150);
/// Unanswered ssh prompts are cancelled after this long.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a local spawn waits for box mirrors to release its ports.
const RELEASE_WAIT: Duration = Duration::from_secs(3);

/// Stream ids for prompts start far above the engine's own attach ids, so both
/// can share one client connection.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1 << 40);

/// Remote state owned by the engine.
#[derive(Default)]
pub(crate) struct Remotes {
    hosts: Mutex<BTreeMap<String, Arc<HostConn>>>,
    /// Serializes connects so a replaced connection is gone before its successor
    /// binds the same ports.
    connecting: tokio::sync::Mutex<()>,
    /// Box side: tunnels currently serving a client, for `status`.
    clients: Mutex<BTreeMap<u64, RemoteClient>>,
    next_client: AtomicU64,
}

impl Remotes {
    /// Mirrored routes of every connected box, in host order.
    pub(crate) fn routes(&self) -> Vec<RouteEntry> {
        self.all()
            .iter()
            .flat_map(|conn| lock(&conn.view).routes.clone())
            .collect()
    }

    /// Ports of every live box's non-hidden units, idle ones included, which new
    /// local port blocks keep clear of: both machines allocate from the same
    /// default pool, so their first lanes would otherwise share numbers.
    pub(crate) fn advertised_ports(&self) -> BTreeSet<u16> {
        self.all()
            .iter()
            .filter(|conn| !conn.ended())
            .flat_map(|conn| conn.registry.current().advertised_ports())
            .collect()
    }

    /// Every connection.
    fn all(&self) -> Vec<Arc<HostConn>> {
        lock(&self.hosts).values().cloned().collect()
    }
}

/// Where a prompt's answer goes; `None` means cancelled.
type Answer = oneshot::Sender<Option<String>>;

/// CLIs attached to `connect` and the prompts waiting for them.
#[derive(Default)]
struct PromptBoard {
    watchers: Mutex<Vec<Session>>,
    /// Pending prompts by `(session, stream)`.
    pending: Mutex<HashMap<(u64, u64), Answer>>,
}

impl PromptBoard {
    /// Registers `session` as a prompt target until the guard drops.
    fn watch(self: &Arc<Self>, session: &Session) -> WatchGuard {
        lock(&self.watchers).push(session.clone());
        WatchGuard {
            board: self.clone(),
            id: session.id(),
        }
    }

    /// Delivers an answer frame; an empty frame cancels. False when not a prompt.
    fn answer(&self, session: u64, stream: u64, data: &[u8]) -> bool {
        let Some(waiting) = lock(&self.pending).remove(&(session, stream)) else {
            return false;
        };
        let answer = (!data.is_empty()).then(|| {
            String::from_utf8_lossy(data)
                .trim_end_matches(['\r', '\n'])
                .to_owned()
        });
        let _ = waiting.send(answer);
        true
    }
}

/// Removes a `connect` CLI from the prompt targets.
struct WatchGuard {
    board: Arc<PromptBoard>,
    id: u64,
}

impl Drop for WatchGuard {
    /// Forgets the session.
    fn drop(&mut self) {
        lock(&self.board.watchers).retain(|session| session.id() != self.id);
    }
}

/// Sends ssh's login prompts to the most recently attached `connect` CLI.
struct HostPrompter {
    board: Arc<PromptBoard>,
}

#[async_trait]
impl RemotePrompter for HostPrompter {
    /// Emits a prompt event and waits for the CLI's answer frame.
    async fn ask(&self, prompt: RemotePrompt) -> Option<String> {
        let session = lock(&self.board.watchers).last().cloned()?;
        let stream_id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
        let (answer_tx, answer) = oneshot::channel();
        lock(&self.board.pending).insert((session.id(), stream_id), answer_tx);
        let event = Event {
            seq: 0,
            event: wire::PROMPT_EVENT.into(),
            data: json!({
                "host": prompt.host,
                "prompt": prompt.prompt,
                "kind": prompt.kind,
                "stream_id": stream_id,
            }),
        };
        let reply = if session.event(event).await.is_ok() {
            let disconnected = session.disconnected();
            tokio::select! {
                reply = answer => reply.ok().flatten(),
                () = disconnected.cancelled() => None,
                () = tokio::time::sleep(PROMPT_TIMEOUT) => None,
            }
        } else {
            None
        };
        lock(&self.board.pending).remove(&(session.id(), stream_id));
        reply
    }
}

/// One `connect`ed box: the link, its mirror and its syncs.
pub(crate) struct HostConn {
    name: String,
    kind: &'static str,
    state: watch::Receiver<LinkState>,
    registry: ClientRegistry,
    link: tokio::sync::Mutex<Option<Link>>,
    cancel: CancellationToken,
    board: Arc<PromptBoard>,
    view: Mutex<mirror::View>,
    /// Makes the mirror reconcile now, for a local spawn waiting on a port.
    wake: tokio::sync::Notify,
    /// Waited for on close, so this host's listeners are gone afterwards.
    mirror_task: Mutex<Option<JoinHandle<()>>>,
    /// Sync status by `lane:unit#index`.
    syncs: Mutex<BTreeMap<String, Value>>,
    /// Destination hashes by sync key, reused while files keep size and mtime.
    hashes: Mutex<HashMap<String, HashCache>>,
    /// Version and linger findings about the box, by code.
    warnings: Mutex<BTreeMap<&'static str, String>>,
}

impl HostConn {
    /// The live session, if connected.
    fn session(&self) -> Option<BoxSession> {
        live(&self.state.borrow())
    }

    /// Waits up to `limit` for a live session; `None` when the link gave up.
    async fn session_within(&self, limit: Duration) -> Option<BoxSession> {
        let mut state = self.state.clone();
        let wait = async {
            loop {
                let (session, over) = {
                    let current = state.borrow_and_update();
                    (
                        live(&current),
                        matches!(*current, LinkState::Failed { .. } | LinkState::Stopped),
                    )
                };
                if session.is_some() || over {
                    return session;
                }
                if state.changed().await.is_err() {
                    return None;
                }
            }
        };
        tokio::time::timeout(limit, wait).await.ok().flatten()
    }

    /// True once the link stopped for good, so `connect` must start a new one.
    fn ended(&self) -> bool {
        matches!(
            *self.state.borrow(),
            LinkState::Failed { .. } | LinkState::Stopped
        )
    }

    /// Resolves when the link stops: `Err` carries a permanent failure.
    async fn finished(&self) -> Result<(), String> {
        let mut state = self.state.clone();
        loop {
            match &*state.borrow_and_update() {
                LinkState::Failed { error } => return Err(error.clone()),
                LinkState::Stopped => return Ok(()),
                _ => {}
            }
            if state.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    /// Waits until connected or stopped, at most `limit`.
    async fn settled(&self, limit: Duration) {
        let mut state = self.state.clone();
        let wait = async {
            loop {
                let done = matches!(
                    *state.borrow_and_update(),
                    LinkState::Connected(_) | LinkState::Failed { .. } | LinkState::Stopped
                );
                if done || state.changed().await.is_err() {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(limit, wait).await;
    }

    /// `remote.state` payload for the current state.
    fn state_json(&self) -> Value {
        let mut value = describe(&self.state.borrow());
        if let Some(map) = value.as_object_mut() {
            map.insert("host".into(), json!(self.name));
        }
        value
    }

    /// State plus what is mirrored, for `connect`, `remote ls` and `status`.
    fn status_json(&self) -> Value {
        let mut value = self.state_json();
        let lanes = self.registry.current().lanes().count();
        let view = lock(&self.view).clone();
        let syncs: Vec<Value> = lock(&self.syncs).values().cloned().collect();
        let mut warnings: Vec<String> = lock(&self.warnings).values().cloned().collect();
        warnings.extend(view.warnings());
        if let Some(map) = value.as_object_mut() {
            map.insert("transport".into(), json!(self.kind));
            map.insert("lanes".into(), json!(lanes));
            map.insert("ports".into(), json!(view.ports));
            map.insert(
                "routes".into(),
                json!(
                    view.routes
                        .iter()
                        .map(|route| json!({"host": route.host, "port": route.port, "lane": route.lane, "unit": route.unit}))
                        .collect::<Vec<_>>()
                ),
            );
            map.insert("skipped_lanes".into(), json!(view.skipped));
            map.insert("skipped_hosts".into(), json!(view.refused_hosts));
            map.insert("syncs".into(), json!(syncs));
            map.insert("warnings".into(), json!(warnings));
        }
        value
    }

    /// Stops reconnecting, closes the session and waits until this host's
    /// listeners and routes are gone.
    async fn close(&self) {
        self.cancel.cancel();
        let link = self.link.lock().await.take();
        if let Some(link) = link {
            link.shutdown().await;
        }
        let mirror = lock(&self.mirror_task).take();
        if let Some(mirror) = mirror {
            let _ = mirror.await;
        }
    }
}

/// The session of a connected state that has not ended yet.
fn live(state: &LinkState) -> Option<BoxSession> {
    match state {
        LinkState::Connected(session) if session.end().is_none() => Some(session.clone()),
        _ => None,
    }
}

/// Machine-readable link state.
fn describe(state: &LinkState) -> Value {
    match state {
        LinkState::Connecting { attempt } => json!({"state": "connecting", "attempt": attempt}),
        LinkState::Connected(session) => json!({"state": "connected", "server": session.server()}),
        LinkState::Retrying { delay, error } => json!({
            "state": "reconnecting",
            "retry_in_ms": u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "error": error,
        }),
        LinkState::Failed { error } => json!({"state": "failed", "error": error}),
        LinkState::Stopped => json!({"state": "stopped"}),
    }
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `host_not_connected`: `disconnect` named a box without a `connect`.
fn not_connected(host: &str) -> RpcError {
    RpcError::new("host_not_connected", json!({"host": host}), 4)
}

/// `port_mirrored`: a local service needs a port that the mirror of box `host`
/// holds for its unit `unit` (`project@lane:unit`).
fn port_mirrored(host: &str, port: u16, unit: &str) -> RpcError {
    RpcError::new(
        "port_mirrored",
        json!({
            "port": port,
            "host": host,
            "unit": unit,
            "message": format!(
                "port {port} is mirrored from box {host} for {unit}; stop {unit} on the box or run `devlanes remote disconnect {host}`"
            ),
        }),
        1,
    )
}

/// `remote_failed`: the link failed.
fn remote_failed(host: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        "remote_failed",
        json!({"host": host, "message": message.to_string()}),
        4,
    )
}

/// `connect` parameters.
#[derive(Debug, Deserialize)]
struct ConnectParams {
    host: String,
    #[serde(default)]
    background: bool,
    /// Install devlanes on a box that lacks it.
    #[serde(default)]
    install: bool,
}

/// `disconnect` parameters; no host means every box.
#[derive(Debug, Default, Deserialize)]
struct DisconnectParams {
    #[serde(default)]
    host: Option<String>,
}

/// Extra `subscribe` field a box's tunnel sends to be listed by `status`.
#[derive(Debug, Default, Deserialize)]
struct SubscribeExtra {
    #[serde(default)]
    remote_client: Option<RemoteClient>,
}

/// Removes a tunnel from the box's client list when its subscription ends.
struct ClientGuard {
    engine: Arc<Inner>,
    id: u64,
}

impl Drop for ClientGuard {
    /// Forgets the tunnel.
    fn drop(&mut self) {
        lock(&self.engine.remote.clients).remove(&self.id);
    }
}

/// Names ssh accepts as destinations; a leading `-` would read as an option.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && !host.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `$DEVLANES_HOME/sync/<host>/<project>/<lane>/<leaf>` for a box sync entry.
///
/// Every component is a single validated path name, so nothing the box sends can
/// point the destination elsewhere; the box's own `to` is never used.
fn sync_destination(home: &Path, host: &str, entry: &SyncEntry) -> Result<PathBuf, String> {
    let host = host.replace('/', "_");
    let mut path = home.join("sync");
    for (what, part) in [
        ("host", host.as_str()),
        ("project", entry.project.as_str()),
        ("lane", entry.lane.as_str()),
        ("folder", entry.leaf.as_str()),
    ] {
        let valid = !part.contains('/') && RelPath::new(part).is_ok();
        if !valid {
            return Err(format!("the box sent an unusable {what} name {part:?}"));
        }
        path.push(part);
    }
    Ok(path)
}

impl Inner {
    /// `connect`: creates (or reuses) the box's link, then streams `remote.state`
    /// events until the CLI detaches. The connection lives in the daemon either way;
    /// `background` only returns once the first attempt settled.
    pub(crate) async fn connect_method(
        self: &Arc<Self>,
        value: Value,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: ConnectParams = required(value)?;
        if !valid_host(&args.host) {
            return Err(RpcError::new(
                "bad_request",
                json!({"message": "invalid host", "host": args.host}),
                2,
            ));
        }
        let options = ConnectOptions {
            install: args.install,
        };
        let conn = self.ensure_host(&args.host, options).await?;
        let _watching = conn.board.watch(session);
        if args.background {
            conn.settled(CONNECT_WAIT).await;
            if let LinkState::Failed { error } = &*conn.state.borrow() {
                return Err(remote_failed(&conn.name, error));
            }
            return Ok(conn.status_json());
        }
        let first = Event {
            seq: 0,
            event: wire::STATE_EVENT.into(),
            data: conn.state_json(),
        };
        if session.event(first).await.is_err() {
            return Ok(conn.status_json());
        }
        let host = conn.name.clone();
        let mine = move |event: &Event| {
            (event.event == wire::STATE_EVENT || event.event == wire::WARNING_EVENT)
                && event.data.get("host").and_then(Value::as_str) == Some(host.as_str())
        };
        tokio::select! {
            () = self.bus.forward(session, mine) => Ok(conn.status_json()),
            finished = conn.finished() => match finished {
                Ok(()) => Ok(conn.status_json()),
                Err(error) => Err(remote_failed(&conn.name, error)),
            },
        }
    }

    /// The live connection to `host`, or a new one. A stopped link is closed, with
    /// its listeners, before its replacement starts.
    async fn ensure_host(
        self: &Arc<Self>,
        host: &str,
        options: ConnectOptions,
    ) -> Result<Arc<HostConn>, RpcError> {
        let _connecting = self.remote.connecting.lock().await;
        let old = {
            let mut hosts = lock(&self.remote.hosts);
            match hosts.get(host) {
                Some(conn) if !conn.ended() => return Ok(conn.clone()),
                _ => hosts.remove(host),
            }
        };
        if let Some(old) = old {
            old.close().await;
        }
        let conn = self.open_host(host, options)?;
        lock(&self.remote.hosts).insert(host.to_owned(), conn.clone());
        Ok(conn)
    }

    /// Starts the link and the host's watcher and mirror tasks.
    fn open_host(
        self: &Arc<Self>,
        host: &str,
        options: ConnectOptions,
    ) -> Result<Arc<HostConn>, RpcError> {
        let board = Arc::new(PromptBoard::default());
        let prompter = Arc::new(HostPrompter {
            board: board.clone(),
        });
        let transport = self.integ.remote.transport(host, prompter, options)?;
        let options = SessionOptions::new(format!("devlanes {}", env!("CARGO_PKG_VERSION")));
        let link = Link::spawn(transport, options, self.integ.remote.backoff());
        let conn = Arc::new(HostConn {
            name: host.to_owned(),
            kind: self.integ.remote.kind(),
            state: link.state(),
            registry: link.registry().clone(),
            link: tokio::sync::Mutex::new(Some(link)),
            cancel: CancellationToken::new(),
            board,
            view: Mutex::new(mirror::View::default()),
            wake: tokio::sync::Notify::new(),
            mirror_task: Mutex::new(None),
            syncs: Mutex::new(BTreeMap::new()),
            hashes: Mutex::new(HashMap::new()),
            warnings: Mutex::new(BTreeMap::new()),
        });
        tokio::spawn(watch_link(Arc::downgrade(self), conn.clone()));
        let mirror = tokio::spawn(mirror::run(Arc::downgrade(self), conn.clone()));
        *lock(&conn.mirror_task) = Some(mirror);
        Ok(conn)
    }

    /// `disconnect [host]`: closes one box or all of them and unmirrors them.
    pub(crate) async fn disconnect_method(
        self: &Arc<Self>,
        value: Value,
    ) -> Result<Value, RpcError> {
        let args: DisconnectParams = crate::engine::params(value)?;
        let closing: Vec<Arc<HostConn>> = {
            let mut hosts = lock(&self.remote.hosts);
            match &args.host {
                Some(host) => match hosts.remove(host) {
                    Some(conn) => vec![conn],
                    None => return Err(not_connected(host)),
                },
                None => std::mem::take(&mut *hosts).into_values().collect(),
            }
        };
        let mut names = Vec::new();
        for conn in closing {
            conn.close().await;
            self.bus.emit(
                wire::STATE_EVENT,
                json!({"host": conn.name, "state": "stopped"}),
            );
            names.push(conn.name.clone());
        }
        self.publish_routes();
        Ok(json!({"disconnected": names}))
    }

    /// `remote ls`: boxes known from `remote_hosts` plus live connection state.
    pub(crate) async fn host_list(self: &Arc<Self>) -> Result<Value, RpcError> {
        let stored = self.store.remote_hosts().await.unwrap_or_default();
        let mut items: BTreeMap<String, Value> = BTreeMap::new();
        for host in stored {
            items.insert(
                host.name.clone(),
                json!({
                    "host": host.name,
                    "transport": host.transport,
                    "address": host.address,
                    "daemon_version": host.daemon_version,
                    "last_connected_ms": host.last_connected,
                    "state": "disconnected",
                }),
            );
        }
        for conn in self.remote.all() {
            let mut status = conn.status_json();
            if let (Some(stored), Some(map)) = (items.get(&conn.name), status.as_object_mut()) {
                for key in ["address", "daemon_version", "last_connected_ms"] {
                    if let Some(value) = stored.get(key) {
                        map.insert(key.into(), value.clone());
                    }
                }
            }
            items.insert(conn.name.clone(), status);
        }
        Ok(json!({
            "items": items.into_values().collect::<Vec<_>>(),
            "truncated": false,
            "next_cursor": null,
        }))
    }

    /// Routes binary frames that answer ssh login prompts; false for other streams.
    pub(crate) async fn remote_input(
        &self,
        stream_id: u64,
        data: &Bytes,
        session: &Session,
    ) -> bool {
        self.remote
            .all()
            .iter()
            .any(|conn| conn.board.answer(session.id(), stream_id, data))
    }

    /// `status`, plus the tunnels serving clients (on a box) and the boxes this
    /// machine is connected to.
    pub(crate) async fn remote_status(
        self: &Arc<Self>,
        value: Value,
        context: &devlanes_proto::Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let mut result = self.status_method(value, context, session).await?;
        let clients: Vec<RemoteClient> = lock(&self.remote.clients).values().cloned().collect();
        let hosts: Vec<Value> = self
            .remote
            .all()
            .iter()
            .map(|conn| conn.state_json())
            .collect();
        if let Some(map) = result.as_object_mut() {
            if !clients.is_empty() {
                map.insert("clients".into(), json!(clients));
            }
            if !hosts.is_empty() {
                map.insert("hosts".into(), json!(hosts));
            }
        }
        Ok(result)
    }

    /// `subscribe`; a tunnel's `remote_client` is listed by `status` while it lasts.
    pub(crate) async fn remote_subscribe(
        self: &Arc<Self>,
        value: Value,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let extra: SubscribeExtra = serde_json::from_value(value.clone()).unwrap_or_default();
        let _guard = extra.remote_client.map(|client| {
            let id = self.remote.next_client.fetch_add(1, Ordering::Relaxed);
            lock(&self.remote.clients).insert(id, client);
            ClientGuard {
                engine: self.clone(),
                id,
            }
        });
        self.subscribe_method(value, session).await
    }

    /// `sync.status`: sync entries of lanes with running services (what a tunnel may
    /// stream; `kind: local` with the local copy's state under `local`), plus the
    /// state of syncs this machine receives from boxes (`kind: remote`).
    pub(crate) async fn sync_status_method(self: &Arc<Self>) -> Result<Value, RpcError> {
        let mut items = Vec::new();
        for (project, lane) in self.all_lanes() {
            let (alive, reference) = {
                let st = lane.st();
                (st.services_alive(), st.reference(&project.name()))
            };
            if !alive {
                continue;
            }
            let resolved = match self.resolve_full(&project, &lane).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::debug!(lane = %reference, code = %error.code, "sync entries unavailable");
                    continue;
                }
            };
            for service in resolved.services.values() {
                for (index, spec) in service.sync.iter().enumerate() {
                    let entry = SyncEntry {
                        lane: reference.clone(),
                        project: project.name(),
                        unit: service.unit.clone(),
                        index,
                        from: spec.from.clone(),
                        leaf: spec
                            .from
                            .file_name()
                            .map(|leaf| leaf.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "sync".into()),
                        ignore: spec.ignore.clone(),
                    };
                    // Tunnels read the entry fields; `kind` and `local` are extra.
                    let mut item = serde_json::to_value(entry).unwrap_or_default();
                    item["kind"] = json!("local");
                    item["local"] =
                        crate::local_sync::status(&lane, &service.unit, index).unwrap_or_default();
                    items.push(item);
                }
            }
        }
        let remote: Vec<Value> = self
            .remote
            .all()
            .iter()
            .flat_map(|conn| lock(&conn.syncs).values().cloned().collect::<Vec<_>>())
            .map(|mut status| {
                if let Some(fields) = status.as_object_mut() {
                    fields.insert("kind".into(), json!("remote"));
                }
                status
            })
            .collect();
        Ok(json!({
            "items": items,
            "remote": remote,
            "truncated": false,
            "next_cursor": null,
        }))
    }

    /// Makes sure no box mirror holds one of `ports` before a local service binds
    /// them. A mirror gives up ports inside a local block (local lanes win), so
    /// this wakes the mirrors and waits up to [`RELEASE_WAIT`]; a port still held
    /// then, such as a literal `PORT` the box also uses, fails with `port_mirrored`.
    pub(crate) async fn release_for_local(&self, ports: &BTreeSet<u16>) -> Result<(), RpcError> {
        let deadline = tokio::time::Instant::now() + RELEASE_WAIT;
        loop {
            let held = self.remote.all().into_iter().find_map(|conn| {
                let found = lock(&conn.view)
                    .bound
                    .iter()
                    .find(|(port, _)| ports.contains(*port))
                    .map(|(port, unit)| (*port, unit.clone()));
                found.map(|(port, unit)| (conn, port, unit))
            });
            let Some((conn, port, unit)) = held else {
                return Ok(());
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(port_mirrored(&conn.name, port, &unit));
            }
            conn.wake.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Closes every box connection; called on daemon shutdown so ssh masters exit.
    pub(crate) async fn remote_shutdown(&self) {
        let hosts: Vec<Arc<HostConn>> = std::mem::take(&mut *lock(&self.remote.hosts))
            .into_values()
            .collect();
        for conn in hosts {
            conn.close().await;
        }
    }
}

/// Findings about the box behind a handshake label (`devlanes <version>`,
/// optionally `(linger off)`), as warning code and message.
fn label_warnings(label: &str) -> Vec<(&'static str, String)> {
    let ours = env!("CARGO_PKG_VERSION");
    let mut warnings = Vec::new();
    let theirs = label
        .strip_prefix("devlanes ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("");
    if theirs != ours {
        warnings.push((
            "box_version",
            format!("the box runs devlanes {theirs} and this machine {ours}; install the same release on both"),
        ));
    }
    if label.contains("(linger off)") {
        warnings.push((
            "box_linger_off",
            "the box stops your processes at logout; run `loginctl enable-linger` there or `devlanes daemon install`".to_owned(),
        ));
    }
    warnings
}

/// Publishes state changes, records successful connects and restarts syncs on
/// every new session.
async fn watch_link(engine: std::sync::Weak<Inner>, conn: Arc<HostConn>) {
    let mut state = conn.state.clone();
    let mut syncing: Option<JoinHandle<()>> = None;
    loop {
        let session = live(&state.borrow_and_update());
        let Some(inner) = engine.upgrade() else { break };
        inner.bus.emit(wire::STATE_EVENT, conn.state_json());
        if let Some(session) = session {
            let record = devlanes_state::RemoteHost {
                name: conn.name.clone(),
                transport: conn.kind.to_owned(),
                address: conn.name.clone(),
                daemon_version: Some(session.server().to_owned()),
                last_connected: i64::try_from(now_ms()).ok(),
            };
            if let Err(error) = inner.store.upsert_remote_host(record).await {
                tracing::warn!(%error, host = %conn.name, "cannot record remote host");
            }
            let findings = label_warnings(session.server());
            {
                let mut warnings = lock(&conn.warnings);
                warnings.clear();
                warnings.extend(findings.iter().cloned());
            }
            for (code, message) in findings {
                tracing::warn!(host = %conn.name, code, %message, "box warning");
                inner.bus.emit(
                    wire::WARNING_EVENT,
                    json!({"host": conn.name, "code": code, "message": message}),
                );
            }
            if let Some(task) = syncing.take() {
                task.abort();
            }
            syncing = Some(tokio::spawn(sync::run(
                engine.clone(),
                conn.clone(),
                session,
            )));
        }
        drop(inner);
        tokio::select! {
            () = conn.cancel.cancelled() => break,
            changed = state.changed() => if changed.is_err() { break },
        }
    }
    if let Some(task) = syncing {
        task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(project: &str, lane: &str, leaf: &str) -> SyncEntry {
        SyncEntry {
            lane: lane.into(),
            project: project.into(),
            unit: "web".into(),
            index: 0,
            from: PathBuf::from("/box/ext"),
            leaf: leaf.into(),
            ignore: Vec::new(),
        }
    }

    #[test]
    fn sync_destinations_stay_under_the_sync_root() {
        let home = Path::new("/home/u/.devlanes");
        assert_eq!(
            sync_destination(home, "box", &entry("app", "app@1", "ext")),
            Ok(PathBuf::from("/home/u/.devlanes/sync/box/app/app@1/ext"))
        );
        for bad in [
            entry("..", "app@1", "ext"),
            entry("app", "../..", "ext"),
            entry("app", "app@1", ".."),
            entry("app", "app@1", "a/b"),
            entry("app", "app@1", ""),
            entry("app", "a\0b", "ext"),
        ] {
            assert!(sync_destination(home, "box", &bad).is_err(), "{bad:?}");
        }
        assert!(sync_destination(home, "..", &entry("app", "app@1", "ext")).is_err());
        assert_eq!(
            sync_destination(home, "user@box/x", &entry("app", "app@1", "ext")),
            Ok(PathBuf::from(
                "/home/u/.devlanes/sync/user@box_x/app/app@1/ext"
            ))
        );
    }

    #[test]
    fn labels_report_version_skew_and_linger() {
        let ours = format!("devlanes {}", env!("CARGO_PKG_VERSION"));
        assert!(label_warnings(&ours).is_empty());
        let codes: Vec<_> = label_warnings("devlanes 0.0.0-other (linger off)")
            .into_iter()
            .map(|(code, _)| code)
            .collect();
        assert_eq!(codes, ["box_version", "box_linger_off"]);
    }
}
